//! Bifrost-mode patches. See the Bifrost design doc and NorthgardClient.py's Bifrost section
//! for the marker-file protocol these read and write.
//!
//! Every patch here is gated on `bifrost\active.flag`: the client writes it only while it is
//! connected to a Bifrost-mode slot, so a Conquest player (or anyone playing Bifrost with no
//! client) keeps vanilla Bifrost behavior.
//!
//! Marker files the game READS (written by the client), all under `<config>\bifrost\`:
//!   active.flag             Bifrost mode is active for the connected slot
//!   realm_<Realm>           realm is unlocked (Midgard always, the others by item)
//!   loc_<Realm>             realm still holds at least one unchecked location
//!   obj_<Realm>_<ObjId>     that objective still holds an unchecked location in that realm
//! Marker files the game WRITES (read and deleted by the client), under `bifrost\done\`:
//!   obj_<Realm>_<ObjId>     objective completed
//!   realm_<Realm>           realm won
//!
//! All new code is built with `Assembler`, a tiny assembler with labels: jump offsets are resolved
//! once at the end instead of being counted by hand, which is what made the Conquest patches
//! fragile to edit.

use anyhow::{bail, Context, Result};
use hlbc::opcodes::Opcode;
use hlbc::types::{ObjField, Reg, RefField, RefFloat, RefFun, RefInt, RefString, RefType, Type};
use hlbc::{Bytecode, Resolve};

use super::{find_field_by_name, find_method_findex, find_type_index_by_name};

mod items;

pub const BIFROST_SUBDIR: &str = "bifrost";
pub const DONE_SUBDIR: &str = "done";
pub const ACTIVE_FLAG_NAME: &str = "active.flag";

// ------------------------------------------------------------------------------------------
// Assembler
// ------------------------------------------------------------------------------------------

#[derive(Clone, Copy)]
pub struct Lbl(usize);

pub struct Assembler {
    pub ops: Vec<Opcode>,
    labels: Vec<Option<usize>>,
    fixups: Vec<(usize, usize)>,
    externals: Vec<(usize, usize)>,
}

fn set_jump_offset(op: &mut Opcode, value: i32) -> Result<()> {
    use Opcode::*;
    match op {
        JTrue { offset, .. } | JFalse { offset, .. } | JNull { offset, .. } | JNotNull { offset, .. }
        | JSLt { offset, .. } | JSGte { offset, .. } | JSGt { offset, .. } | JSLte { offset, .. }
        | JULt { offset, .. } | JUGte { offset, .. } | JNotLt { offset, .. } | JNotGte { offset, .. }
        | JEq { offset, .. } | JNotEq { offset, .. } | JAlways { offset } => {
            *offset = value;
            Ok(())
        }
        other => bail!("internal error: tried to set a jump offset on a non-jump op {other:?}"),
    }
}

impl Assembler {
    pub fn new() -> Self {
        Assembler { ops: Vec::new(), labels: Vec::new(), fixups: Vec::new(), externals: Vec::new() }
    }
    pub fn label(&mut self) -> Lbl {
        self.labels.push(None);
        Lbl(self.labels.len() - 1)
    }
    /// Binds a forward-jump target at the next op.
    pub fn bind(&mut self, l: Lbl) {
        self.labels[l.0] = Some(self.ops.len());
    }
    /// Binds a backward-jump target: HashLink wants a `Label` op where loops jump back to.
    pub fn bind_loop(&mut self, l: Lbl) {
        self.labels[l.0] = Some(self.ops.len());
        self.ops.push(Opcode::Label);
    }
    pub fn op(&mut self, op: Opcode) {
        self.ops.push(op);
    }
    /// Pushes a jump op (any offset value) that will land on `l`.
    pub fn jump(&mut self, op: Opcode, l: Lbl) {
        self.fixups.push((self.ops.len(), l.0));
        self.ops.push(op);
    }
    pub fn len(&self) -> usize {
        self.ops.len()
    }
    /// Resolves every label jump, for a block placed at the very start of a function.
    pub fn finish(mut self) -> Result<Vec<Opcode>> {
        for (at, label) in std::mem::take(&mut self.fixups) {
            let target = self.labels[label].context("internal error: unbound label")?;
            set_jump_offset(&mut self.ops[at], target as i32 - at as i32 - 1)?;
        }
        Ok(self.ops)
    }
    /// Pushes a jump that must land on an ORIGINAL op of the function being spliced (see
    /// `splice`); its offset is filled in there.
    pub fn jump_original(&mut self, op: Opcode, original_op: usize) {
        self.externals.push((self.ops.len(), original_op));
        self.ops.push(op);
    }
    /// Resolves internal labels and returns (ops, external jumps) for `splice`.
    pub fn finish_with_externals(mut self) -> Result<(Vec<Opcode>, Vec<(usize, usize)>)> {
        let externals = std::mem::take(&mut self.externals);
        Ok((self.finish()?, externals))
    }
    /// Binds a label to a position past this block's end, relative to its first op (e.g. "the
    /// original body that follows a prepended block").
    pub fn bind_external(&mut self, l: Lbl, position: usize) {
        self.labels[l.0] = Some(position);
    }
}

// ------------------------------------------------------------------------------------------
// Shared types, functions and constants
// ------------------------------------------------------------------------------------------

pub fn find_prim_type(code: &Bytecode, what: &str, pred: impl Fn(&Type) -> bool) -> Result<RefType> {
    code.types
        .iter()
        .position(pred)
        .map(RefType)
        .with_context(|| format!("could not find the {what} type. Northgard build mismatch?"))
}

pub fn obj_type(code: &Bytecode, class_name: &str) -> Result<RefType> {
    find_type_index_by_name(code, class_name)
        .map(RefType)
        .with_context(|| format!("could not find class {class_name}. Northgard build mismatch?"))
}

/// Everything the Bifrost patches share: core types, the String layout, and the file helpers.
pub struct Env {
    pub string_ty: RefType,
    pub bytes_ty: RefType,
    pub i32_ty: RefType,
    pub bool_ty: RefType,
    pub void_ty: RefType,
    pub f64_ty: RefType,
    pub dyn_ty: RefType,
    pub array_ty: RefType,
    pub field_bytes: RefField,
    pub field_length: RefField,
    pub fn_add: usize,
    pub fn_exists: usize,
    pub fn_create_dir: usize,
    pub fn_save_content: usize,
    pub fn_get_content: usize,
    pub bifrost_dir: String,
    pub done_dir: String,
}

impl Env {
    pub fn new(code: &Bytecode, bifrost_dir: String, done_dir: String) -> Result<Self> {
        let fn_add = find_method_findex(code, "$String", "__add__")?;
        let add_fn = code
            .functions
            .iter()
            .find(|f| f.findex.0 == fn_add)
            .context("could not find String.__add__. Northgard build mismatch?")?;
        if add_fn.ops.len() <= 27 || add_fn.regs.len() <= 8 {
            bail!("String.__add__ shape changed. Northgard build mismatch?");
        }
        let field_length = match &add_fn.ops[7] {
            Opcode::Field { field, .. } => *field,
            other => bail!("__add__ op7 shape changed (expected Field), got {other:?}"),
        };
        let field_bytes = match &add_fn.ops[27] {
            Opcode::Field { field, .. } => *field,
            other => bail!("__add__ op27 shape changed (expected Field), got {other:?}"),
        };
        Ok(Env {
            string_ty: add_fn.regs[0],
            bytes_ty: add_fn.regs[8],
            i32_ty: add_fn.regs[4],
            bool_ty: find_prim_type(code, "bool", |t| matches!(t, Type::Bool))?,
            void_ty: find_prim_type(code, "void", |t| matches!(t, Type::Void))?,
            f64_ty: find_prim_type(code, "f64", |t| matches!(t, Type::F64))?,
            dyn_ty: find_prim_type(code, "dynamic", |t| matches!(t, Type::Dyn))?,
            array_ty: find_prim_type(code, "array", |t| matches!(t, Type::Array))?,
            field_bytes,
            field_length,
            fn_add,
            fn_exists: find_method_findex(code, "sys.$FileSystem", "exists")?,
            fn_create_dir: find_method_findex(code, "sys.$FileSystem", "createDirectory")?,
            fn_save_content: find_method_findex(code, "sys.io.$File", "saveContent")?,
            fn_get_content: find_method_findex(code, "sys.io.$File", "getContent")?,
            bifrost_dir,
            done_dir,
        })
    }
}

/// An interned string constant: its string-table entry and its length in the int table.
#[derive(Clone, Copy)]
pub struct Konst {
    s: RefString,
    len: RefInt,
}

pub fn konst(code: &mut Bytecode, value: &str) -> Konst {
    let s = RefString(code.strings.len());
    code.strings.push(value.into());
    let len = RefInt(code.ints.len());
    code.ints.push(value.encode_utf16().count() as i32);
    Konst { s, len }
}

pub fn int_konst(code: &mut Bytecode, value: i32) -> RefInt {
    let r = RefInt(code.ints.len());
    code.ints.push(value);
    r
}

pub fn float_konst(code: &mut Bytecode, value: f64) -> RefFloat {
    let r = RefFloat(code.floats.len());
    code.floats.push(value);
    r
}

/// Registers appended to one function, starting after its existing ones.
pub struct Regs {
    base: u32,
    pub new: Vec<RefType>,
}

impl Regs {
    pub fn new(existing: usize) -> Self {
        Regs { base: existing as u32, new: Vec::new() }
    }
    pub fn alloc(&mut self, ty: RefType) -> Reg {
        self.new.push(ty);
        Reg(self.base + self.new.len() as u32 - 1)
    }
}

/// Scratch registers every string/file helper below reuses.
pub struct Scratch {
    pub bytes: Reg,
    pub len: Reg,
    pub boolean: Reg,
    pub void: Reg,
}

impl Scratch {
    pub fn new(env: &Env, regs: &mut Regs) -> Self {
        Scratch {
            bytes: regs.alloc(env.bytes_ty),
            len: regs.alloc(env.i32_ty),
            boolean: regs.alloc(env.bool_ty),
            void: regs.alloc(env.void_ty),
        }
    }
}

/// `dst = new String(constant)`. HashLink's `String{}` op only loads a raw wide-char pointer,
/// so it has to be wrapped into a real String object by hand (see main.rs's
/// patch_get_battle_state for the crash this avoids).
pub fn emit_str(asm: &mut Assembler, env: &Env, sc: &Scratch, dst: Reg, k: Konst) {
    use Opcode::*;
    asm.op(String { dst: sc.bytes, ptr: k.s });
    asm.op(Int { dst: sc.len, ptr: k.len });
    asm.op(New { dst });
    asm.op(SetField { obj: dst, field: env.field_bytes, src: sc.bytes });
    asm.op(SetField { obj: dst, field: env.field_length, src: sc.len });
}

/// `dst = a + b` (String concatenation).
pub fn emit_concat(asm: &mut Assembler, env: &Env, dst: Reg, a: Reg, b: Reg) {
    asm.op(Opcode::Call2 { dst, fun: RefFun(env.fn_add), arg0: a, arg1: b });
}

/// `dst = sys.FileSystem.exists(path)`.
pub fn emit_exists(asm: &mut Assembler, env: &Env, dst: Reg, path: Reg) {
    asm.op(Opcode::Call1 { dst, fun: RefFun(env.fn_exists), arg0: path });
}

/// Jumps to `skip` unless `bifrost\active.flag` exists. `tmp` is a String register.
pub fn emit_require_active(asm: &mut Assembler, env: &Env, sc: &Scratch, active_flag: Konst, tmp: Reg, skip: Lbl) {
    emit_str(asm, env, sc, tmp, active_flag);
    emit_exists(asm, env, sc.boolean, tmp);
    asm.jump(Opcode::JFalse { cond: sc.boolean, offset: 0 }, skip);
}

/// Writes an empty marker file at `path` (a String register), creating `done_dir` first so a
/// deleted folder can never make the game throw.
pub fn emit_write_marker(asm: &mut Assembler, env: &Env, sc: &Scratch, done_dir: Konst, empty: Konst, path: Reg, tmp: Reg) {
    emit_str(asm, env, sc, tmp, done_dir);
    asm.op(Opcode::Call1 { dst: sc.void, fun: RefFun(env.fn_create_dir), arg0: tmp });
    emit_str(asm, env, sc, tmp, empty);
    asm.op(Opcode::Call2 { dst: sc.void, fun: RefFun(env.fn_save_content), arg0: path, arg1: tmp });
}

/// Installs `ops` (and `new_regs`) as a function's new body, keeping debug info aligned: new
/// ops borrow the debug position of `debug_from` (an index into the OLD ops).
pub fn install(code: &mut Bytecode, findex: usize, ops: Vec<Opcode>, new_regs: Vec<RefType>, debug_map: impl Fn(usize) -> usize) -> Result<()> {
    let f = code
        .functions
        .iter_mut()
        .find(|f| f.findex.0 == findex)
        .context("internal error: function vanished")?;
    f.regs.extend(new_regs);
    if let Some(debug_info) = &mut f.debug_info {
        let old = debug_info.clone();
        let get = |i: usize| old.get(i).copied().or_else(|| old.last().copied()).unwrap_or((0, 0));
        *debug_info = (0..ops.len()).map(|i| get(debug_map(i))).collect();
    }
    f.ops = ops;
    f.assigns = Some(Vec::new());
    Ok(())
}

pub fn function<'a>(code: &'a Bytecode, findex: usize, what: &str) -> Result<&'a hlbc::types::Function> {
    code.functions
        .iter()
        .find(|f| f.findex.0 == findex)
        .with_context(|| format!("could not find {what}. Northgard build mismatch?"))
}

/// Adds a new field of any type to a class with no subclasses (see main.rs's add_bool_field
/// for why "no subclasses" is required).
pub fn add_field(code: &mut Bytecode, class_name: &str, field_name: &str, ty: RefType) -> Result<RefField> {
    let has_subclass = code.types.iter().any(|t| match t {
        Type::Obj(obj) => obj
            .super_
            .map(|s| matches!(code.get(s), Type::Obj(p) if p.name(code) == class_name))
            .unwrap_or(false),
        _ => false,
    });
    if has_subclass {
        bail!("{class_name} has subclasses, so a field can't safely be added to it");
    }
    let type_index = find_type_index_by_name(code, class_name)
        .with_context(|| format!("could not find class {class_name}. Northgard build mismatch?"))?;
    let name = RefString(code.strings.len());
    code.strings.push(field_name.into());
    let field = ObjField { name, t: ty };
    match &mut code.types[type_index] {
        Type::Obj(obj) => {
            obj.own_fields.push(field.clone());
            obj.fields.push(field);
            Ok(RefField(obj.fields.len() - 1))
        }
        _ => bail!("internal error: {class_name} is not an Obj"),
    }
}

// ------------------------------------------------------------------------------------------
// Patches
// ------------------------------------------------------------------------------------------

pub fn apply_all(code: &mut Bytecode, env: &Env) -> Result<()> {
    patch_get_next_realms(code, env)?;
    patch_realm_click(code, env)?;
    patch_realm_cards_live(code, env)?;
    patch_pick_objective(code, env)?;
    patch_complete_objective(code, env)?;
    patch_bifrost_won(code, env)?;
    items::patch_perk_pick(code, env)?;
    items::patch_bonus_items(code, env)?;
    items::patch_ap_cards(code, env)?;
    Ok(())
}

pub fn path_in(dir: &str, name: &str) -> String {
    format!("{dir}\\{name}")
}

/// `$HBifrost.getNextRealms(save)`: realm offers. The vanilla body builds `pool` (every realm
/// not yet finished this run, minus Midgard and Vanaheim), returns `[Vanaheim]` at step 3,
/// pushes Midgard first at step 0, then fills up to 2 offers with seeded random picks from the
/// pool. That last loop (op 63 onward) is where our block is inserted: when Bifrost mode is
/// active it sorts the pool into three tiers and fills the offers tier by tier, so unlocked
/// realms with locations come first, then other unlocked realms, then locked ones (shown as
/// greyed cards by patch_realm_cards_live). It never crashes on a short pool: each tier loop
/// also stops when its tier is empty. When not active, it jumps straight into the original
/// loop, untouched.
///
/// Inserting at 63 is safe for the original jumps: only op 59 targets 63 (the "skip Midgard"
/// branch), which now lands on our block's first op, exactly where both paths should enter.
fn patch_get_next_realms(code: &mut Bytecode, env: &Env) -> Result<()> {
    let findex = find_method_findex(code, "$HBifrost", "getNextRealms")?;
    let f = function(code, findex, "$HBifrost.getNextRealms")?;
    if f.ops.len() != 82 || f.regs.len() != 22 {
        bail!("getNextRealms shape changed (expected 82 ops / 22 regs, got {} / {})", f.ops.len(), f.regs.len());
    }
    let o = f.ops.clone();
    let r = f.regs.clone();
    use Opcode::*;
    let (realm_ty, fn_alloc) = match (&o[3], &o[4]) {
        (Type { ty, .. }, Call2 { fun, .. }) => (*ty, fun.0),
        other => bail!("getNextRealms ops 3-4 changed: {other:?}"),
    };
    let fn_wrap = match &o[6] { Call1 { fun, .. } => fun.0, other => bail!("getNextRealms op6 changed: {other:?}") };
    let field_len = match &o[15] { Field { field, .. } => *field, other => bail!("op15 changed: {other:?}") };
    let field_arr = match &o[21] { Field { field, .. } => *field, other => bail!("op21 changed: {other:?}") };
    let field_id = match &o[26] { Field { field, .. } => *field, other => bail!("op26 changed: {other:?}") };
    let fn_push = match &o[39] { Call2 { fun, .. } => fun.0, other => bail!("op39 changed: {other:?}") };
    let field_seed = match &o[71] { Field { field, .. } => *field, other => bail!("op71 changed: {other:?}") };
    let fn_rand_ctor = match &o[72] { Call2 { fun, .. } => fun.0, other => bail!("op72 changed: {other:?}") };
    let fn_pick = match &o[73] { Call2 { fun, .. } => fun.0, other => bail!("op73 changed: {other:?}") };
    let fn_remove = match &o[79] { Call2 { fun, .. } => fun.0, other => bail!("op79 changed: {other:?}") };
    let zero = match &o[2] { Int { ptr, .. } => *ptr, other => bail!("op2 changed: {other:?}") };
    let two = match &o[66] { Int { ptr, .. } => *ptr, other => bail!("op66 changed: {other:?}") };
    if !matches!(o[63], Label) || !matches!(o[81], Ret { .. }) {
        bail!("getNextRealms ops 63/81 changed (expected Label / Ret)");
    }

    let active = konst(code, &path_in(&env.bifrost_dir, ACTIVE_FLAG_NAME));
    let realm_prefix = konst(code, &path_in(&env.bifrost_dir, "realm_"));
    let loc_prefix = konst(code, &path_in(&env.bifrost_dir, "loc_"));

    let mut regs = Regs::new(r.len());
    let sc = Scratch::new(env, &mut regs);
    let r_tiers = [regs.alloc(r[3]), regs.alloc(r[3]), regs.alloc(r[3])];
    let r_i = regs.alloc(env.i32_ty);
    let r_len = regs.alloc(env.i32_ty);
    let r_raw = regs.alloc(r[6]);
    let r_dyn = regs.alloc(env.dyn_ty);
    let r_v = regs.alloc(r[15]);
    let r_id = regs.alloc(env.string_ty);
    let r_pfx = regs.alloc(env.string_ty);
    let r_path = regs.alloc(env.string_ty);
    let r_tmp_i = regs.alloc(env.i32_ty);
    let r_arrdyn = regs.alloc(r[8]);
    let r_rand = regs.alloc(r[21]);
    let r_seed = regs.alloc(env.i32_ty);
    let r_zero = regs.alloc(env.i32_ty);
    let r_two = regs.alloc(env.i32_ty);
    let r_type = regs.alloc(r[7]);
    let r_alloc = regs.alloc(r[5]);
    let r_alloc2 = regs.alloc(r[6]);
    let (pool, save, result) = (Reg(11), Reg(0), Reg(14));

    let mut a = Assembler::new();
    let original = a.label();
    emit_require_active(&mut a, env, &sc, active, r_pfx, original);

    a.op(Int { dst: r_zero, ptr: zero });
    a.op(Int { dst: r_two, ptr: two });
    for t in r_tiers {
        a.op(Type { dst: r_type, ty: realm_ty });
        a.op(Call2 { dst: r_alloc, fun: RefFun(fn_alloc), arg0: r_type, arg1: r_zero });
        a.op(UnsafeCast { dst: r_alloc2, src: r_alloc });
        a.op(Call1 { dst: t, fun: RefFun(fn_wrap), arg0: r_alloc2 });
    }

    // Sort the pool into tiers.
    a.op(Int { dst: r_i, ptr: zero });
    let sort_loop = a.label();
    let sort_done = a.label();
    let to_b = a.label();
    let to_c = a.label();
    a.bind_loop(sort_loop);
    a.op(NullCheck { reg: pool });
    a.op(Field { dst: r_len, obj: pool, field: field_len });
    a.jump(JSGte { a: r_i, b: r_len, offset: 0 }, sort_done);
    a.op(Field { dst: r_raw, obj: pool, field: field_arr });
    a.op(GetArray { dst: r_dyn, array: r_raw, index: r_i });
    a.op(ToVirtual { dst: r_v, src: r_dyn });
    a.op(Incr { dst: r_i });
    a.op(NullCheck { reg: r_v });
    a.op(Field { dst: r_id, obj: r_v, field: field_id });
    emit_str(&mut a, env, &sc, r_pfx, realm_prefix);
    emit_concat(&mut a, env, r_path, r_pfx, r_id);
    emit_exists(&mut a, env, sc.boolean, r_path);
    a.jump(JFalse { cond: sc.boolean, offset: 0 }, to_c);
    emit_str(&mut a, env, &sc, r_pfx, loc_prefix);
    emit_concat(&mut a, env, r_path, r_pfx, r_id);
    emit_exists(&mut a, env, sc.boolean, r_path);
    a.jump(JFalse { cond: sc.boolean, offset: 0 }, to_b);
    a.op(Call2 { dst: r_tmp_i, fun: RefFun(fn_push), arg0: r_tiers[0], arg1: r_v });
    a.jump(JAlways { offset: 0 }, sort_loop);
    a.bind(to_b);
    a.op(Call2 { dst: r_tmp_i, fun: RefFun(fn_push), arg0: r_tiers[1], arg1: r_v });
    a.jump(JAlways { offset: 0 }, sort_loop);
    a.bind(to_c);
    a.op(Call2 { dst: r_tmp_i, fun: RefFun(fn_push), arg0: r_tiers[2], arg1: r_v });
    a.jump(JAlways { offset: 0 }, sort_loop);
    a.bind(sort_done);

    // Fill the offers tier by tier, with the same seeded pick the vanilla loop uses.
    for t in r_tiers {
        let fill = a.label();
        let next = a.label();
        a.bind_loop(fill);
        a.op(NullCheck { reg: result });
        a.op(Field { dst: r_len, obj: result, field: field_len });
        a.jump(JSGte { a: r_len, b: r_two, offset: 0 }, next);
        a.op(Field { dst: r_len, obj: t, field: field_len });
        a.jump(JSLte { a: r_len, b: r_zero, offset: 0 }, next);
        a.op(SafeCast { dst: r_arrdyn, src: t });
        a.op(New { dst: r_rand });
        a.op(NullCheck { reg: save });
        a.op(Field { dst: r_seed, obj: save, field: field_seed });
        a.op(Call2 { dst: sc.void, fun: RefFun(fn_rand_ctor), arg0: r_rand, arg1: r_seed });
        a.op(Call2 { dst: r_dyn, fun: RefFun(fn_pick), arg0: r_arrdyn, arg1: r_rand });
        a.op(ToVirtual { dst: r_v, src: r_dyn });
        a.op(NullCheck { reg: r_v });
        a.op(Field { dst: r_id, obj: r_v, field: field_id });
        a.op(Call2 { dst: r_tmp_i, fun: RefFun(fn_push), arg0: result, arg1: r_id });
        a.op(Call2 { dst: sc.boolean, fun: RefFun(fn_remove), arg0: t, arg1: r_v });
        a.jump(JAlways { offset: 0 }, fill);
        a.bind(next);
    }
    a.op(Ret { ret: result });

    let block_len = a.len();
    a.bind_external(original, block_len);
    let block = a.finish()?;

    let mut ops = o[..63].to_vec();
    ops.extend(block);
    ops.extend(o[63..].iter().cloned());
    install(code, findex, ops, regs.new, |i| if i < 63 { i } else if i < 63 + block_len { 63 } else { i - block_len })
}

/// `RealmChoice.onRealmSelected()`: clicking a realm card. Prepends "if Bifrost mode is
/// active and this card's realm has no unlock marker, do nothing".
fn patch_realm_click(code: &mut Bytecode, env: &Env) -> Result<()> {
    let findex = find_method_findex(code, "ui.menus.bifrost.RealmChoice", "onRealmSelected")?;
    let f = function(code, findex, "RealmChoice.onRealmSelected")?;
    if f.ops.len() != 44 || f.regs.len() != 13 {
        bail!("RealmChoice.onRealmSelected shape changed (expected 44 ops / 13 regs, got {} / {})", f.ops.len(), f.regs.len());
    }
    let o = f.ops.clone();
    let nregs = f.regs.len();
    let (field_realm, _) = find_field_by_name(code, "ui.menus.bifrost.RealmChoice", "realm")?;
    let active = konst(code, &path_in(&env.bifrost_dir, ACTIVE_FLAG_NAME));
    let realm_prefix = konst(code, &path_in(&env.bifrost_dir, "realm_"));

    let mut regs = Regs::new(nregs);
    let sc = Scratch::new(env, &mut regs);
    let r_realm = regs.alloc(env.string_ty);
    let r_pfx = regs.alloc(env.string_ty);
    let r_path = regs.alloc(env.string_ty);

    use Opcode::*;
    let mut a = Assembler::new();
    let original = a.label();
    emit_require_active(&mut a, env, &sc, active, r_pfx, original);
    a.op(GetThis { dst: r_realm, field: field_realm });
    a.jump(JNull { reg: r_realm, offset: 0 }, original);
    emit_str(&mut a, env, &sc, r_pfx, realm_prefix);
    emit_concat(&mut a, env, r_path, r_pfx, r_realm);
    emit_exists(&mut a, env, sc.boolean, r_path);
    a.jump(JTrue { cond: sc.boolean, offset: 0 }, original);
    a.op(Ret { ret: sc.void });
    let n = a.len();
    a.bind_external(original, n);
    let mut ops = a.finish()?;
    ops.extend(o);
    install(code, findex, ops, regs.new, |i| i.saturating_sub(n))
}

/// Live greying of locked realm cards. Adds `apRealmModule` to `BifrostMenu`, has the
/// `RealmChoiceModule` constructor store itself there, and prepends to `BifrostMenu.update`
/// (runs every frame while the Bifrost menus are open) a walk over that module's cards that
/// sets each card's alpha to 1.0 when its realm marker exists and 0.35 when it doesn't. An
/// item that arrives while the lobby is open therefore un-greys its card within a frame, and
/// the click patch above lets it be picked at the same moment.
fn patch_realm_cards_live(code: &mut Bytecode, env: &Env) -> Result<()> {
    let module_ty = obj_type(code, "ui.menus.bifrost.RealmChoiceModule")?;
    let card_ty = obj_type(code, "ui.menus.bifrost.RealmChoice")?;
    let field_module = add_field(code, "ui.menus.bifrost.BifrostMenu", "apRealmModule", module_ty)?;
    let (field_cards, cards_ty) = find_field_by_name(code, "ui.menus.bifrost.RealmChoiceModule", "realmChoices")?;
    let (field_realm, _) = find_field_by_name(code, "ui.menus.bifrost.RealmChoice", "realm")?;
    let (field_alpha, _) = find_field_by_name(code, "ui.menus.bifrost.RealmChoice", "alpha")?;
    let field_len = RefField(0); // hl.types.ArrayObj.length
    let field_arr = RefField(1); // hl.types.ArrayObj.array

    // Constructor: store `this` into menu.apRealmModule (menu is the 4th argument, reg3).
    let ctor = find_method_findex(code, "ui.menus.bifrost.$RealmChoiceModule", "__constructor__")
        .or_else(|_| {
            code.functions
                .iter()
                .find(|f| {
                    f.name(code) == "__constructor__"
                        && f.regs.first().map(|t| *t == module_ty).unwrap_or(false)
                })
                .map(|f| f.findex.0)
                .context("could not find RealmChoiceModule's constructor")
        })?;
    {
        let f = function(code, ctor, "RealmChoiceModule constructor")?;
        if f.regs.get(3).copied() != Some(obj_type(code, "ui.menus.bifrost.BifrostMenu")?) {
            bail!("RealmChoiceModule constructor's 4th argument is no longer the BifrostMenu");
        }
        let o = f.ops.clone();
        let nregs = f.regs.len();
        use Opcode::*;
        let mut a = Assembler::new();
        let skip = a.label();
        a.jump(JNull { reg: Reg(3), offset: 0 }, skip);
        a.op(SetField { obj: Reg(3), field: field_module, src: Reg(0) });
        let n = a.len();
        a.bind_external(skip, n);
        let mut ops = a.finish()?;
        ops.extend(o);
        install(code, ctor, ops, Vec::new(), |i| i.saturating_sub(n))?;
        let _ = nregs;
    }

    let update = find_method_findex(code, "ui.menus.bifrost.BifrostMenu", "update")?;
    let f = function(code, update, "BifrostMenu.update")?;
    let o = f.ops.clone();
    let nregs = f.regs.len();
    let active = konst(code, &path_in(&env.bifrost_dir, ACTIVE_FLAG_NAME));
    let realm_prefix = konst(code, &path_in(&env.bifrost_dir, "realm_"));
    let bright = float_konst(code, 1.0);
    let dim = float_konst(code, 0.35);
    let zero = int_konst(code, 0);

    let mut regs = Regs::new(nregs);
    let sc = Scratch::new(env, &mut regs);
    let r_module = regs.alloc(module_ty);
    let r_cards = regs.alloc(cards_ty);
    let r_i = regs.alloc(env.i32_ty);
    let r_len = regs.alloc(env.i32_ty);
    let r_raw = regs.alloc(env.array_ty);
    let r_dyn = regs.alloc(env.dyn_ty);
    let r_card = regs.alloc(card_ty);
    let r_realm = regs.alloc(env.string_ty);
    let r_pfx = regs.alloc(env.string_ty);
    let r_path = regs.alloc(env.string_ty);
    let r_alpha = regs.alloc(env.f64_ty);

    use Opcode::*;
    let mut a = Assembler::new();
    let original = a.label();
    emit_require_active(&mut a, env, &sc, active, r_pfx, original);
    a.op(GetThis { dst: r_module, field: field_module });
    a.jump(JNull { reg: r_module, offset: 0 }, original);
    a.op(Field { dst: r_cards, obj: r_module, field: field_cards });
    a.jump(JNull { reg: r_cards, offset: 0 }, original);
    a.op(Int { dst: r_i, ptr: zero });
    let lp = a.label();
    let next = a.label();
    let set = a.label();
    a.bind_loop(lp);
    a.op(Field { dst: r_len, obj: r_cards, field: field_len });
    a.jump(JSGte { a: r_i, b: r_len, offset: 0 }, original);
    a.op(Field { dst: r_raw, obj: r_cards, field: field_arr });
    a.op(GetArray { dst: r_dyn, array: r_raw, index: r_i });
    a.op(UnsafeCast { dst: r_card, src: r_dyn });
    a.op(Incr { dst: r_i });
    a.jump(JNull { reg: r_card, offset: 0 }, next);
    a.op(Field { dst: r_realm, obj: r_card, field: field_realm });
    a.jump(JNull { reg: r_realm, offset: 0 }, next);
    emit_str(&mut a, env, &sc, r_pfx, realm_prefix);
    emit_concat(&mut a, env, r_path, r_pfx, r_realm);
    emit_exists(&mut a, env, sc.boolean, r_path);
    a.op(Float { dst: r_alpha, ptr: bright });
    a.jump(JTrue { cond: sc.boolean, offset: 0 }, set);
    a.op(Float { dst: r_alpha, ptr: dim });
    a.bind(set);
    a.op(SetField { obj: r_card, field: field_alpha, src: r_alpha });
    a.bind(next);
    a.jump(JAlways { offset: 0 }, lp);
    let n = a.len();
    a.bind_external(original, n);
    let mut ops = a.finish()?;
    ops.extend(o);
    install(code, update, ops, regs.new, |i| i.saturating_sub(n))
}

/// `$HBifrost.pickObjective()`: objective spawn priority. Each of its three seeded
/// `pickRandom(list, rand)` calls (one portal pick, the filtered portal pick, the standard
/// pick) is replaced in place (see `splice`) with a block that, when Bifrost mode is active,
/// filters the list down to objectives that still hold a location in the current realm
/// (`obj_<Realm>_<ObjId>` markers) and picks from that if it is not empty, else from the full
/// list as vanilla does. The block falls through to the op after the original call, so it
/// never jumps backward into original code.
fn patch_pick_objective(code: &mut Bytecode, env: &Env) -> Result<()> {
    let findex = find_method_findex(code, "$HBifrost", "pickObjective")?;
    let f = function(code, findex, "$HBifrost.pickObjective")?;
    if f.ops.len() != 131 || f.regs.len() != 31 {
        bail!("pickObjective shape changed (expected 131 ops / 31 regs, got {} / {})", f.ops.len(), f.regs.len());
    }
    let o = f.ops.clone();
    let r = f.regs.clone();
    use Opcode::*;
    // Each site: (call op index, the ArrayObj the list was cast from).
    let mut sites = Vec::new();
    for at in [43usize, 123, 128] {
        let (dst, fun, arg0, arg1) = match &o[at] {
            Call2 { dst, fun, arg0, arg1 } => (*dst, fun.0, *arg0, *arg1),
            other => bail!("pickObjective op{at} changed (expected pickRandom), got {other:?}"),
        };
        let src = match &o[at - 1] {
            SafeCast { dst: d, src } if *d == arg0 => *src,
            other => bail!("pickObjective op{} changed (expected SafeCast), got {other:?}", at - 1),
        };
        sites.push((at, dst, fun, arg0, arg1, src));
    }
    let (fn_alloc, obj_ty_ref) = match (&o[68], &o[67]) {
        (Call2 { fun, .. }, Type { ty, .. }) => (fun.0, *ty),
        other => bail!("pickObjective ops 67-68 changed: {other:?}"),
    };
    let fn_wrap = match &o[70] { Call1 { fun, .. } => fun.0, other => bail!("op70 changed: {other:?}") };
    let field_len = match &o[75] { Field { field, .. } => *field, other => bail!("op75 changed: {other:?}") };
    let field_arr = match &o[81] { Field { field, .. } => *field, other => bail!("op81 changed: {other:?}") };
    let field_obj_id = match &o[109] { Field { field, .. } => *field, other => bail!("op109 changed: {other:?}") };
    let fn_push = match &o[116] { Call2 { fun, .. } => fun.0, other => bail!("op116 changed: {other:?}") };
    let (g_game, field_inst, fn_get_bifrost) = match (&o[21], &o[22], &o[24]) {
        (GetGlobal { global, .. }, Field { field, .. }, Call1 { fun, .. }) => (*global, *field, fun.0),
        other => bail!("pickObjective ops 21-24 changed: {other:?}"),
    };
    let (field_bf_realm, _) = find_field_by_name(code, "gamesys.bifrost.BifrostManager", "realm")?;

    let active = konst(code, &path_in(&env.bifrost_dir, ACTIVE_FLAG_NAME));
    let obj_prefix = konst(code, &path_in(&env.bifrost_dir, "obj_"));
    let underscore = konst(code, "_");
    let zero = int_konst(code, 0);

    let mut regs = Regs::new(r.len());
    let sc = Scratch::new(env, &mut regs);
    let r_game = regs.alloc(r[5]);
    let r_inst = regs.alloc(r[4]);
    let r_bf = regs.alloc(r[8]);
    let r_realm = regs.alloc(env.string_ty);
    let r_type = regs.alloc(r[22]);
    let r_alloc = regs.alloc(r[20]);
    let r_alloc2 = regs.alloc(r[21]);
    let r_filtered = regs.alloc(r[14]);
    let r_i = regs.alloc(env.i32_ty);
    let r_len = regs.alloc(env.i32_ty);
    let r_zero = regs.alloc(env.i32_ty);
    let r_raw = regs.alloc(r[21]);
    let r_dyn = regs.alloc(env.dyn_ty);
    let r_v = regs.alloc(r[19]);
    let r_id = regs.alloc(env.string_ty);
    let r_pfx = regs.alloc(env.string_ty);
    let r_path = regs.alloc(env.string_ty);
    let r_tmp_i = regs.alloc(env.i32_ty);
    let r_arrdyn = regs.alloc(r[18]);

    let mut replacements: Vec<Replacement> = Vec::new();
    for (at, dst, fun, arg0, arg1, src) in sites {
        let mut a = Assembler::new();
        let vanilla = a.label();
        let pick_filtered = a.label();
        let back = a.label();
        emit_require_active(&mut a, env, &sc, active, r_pfx, vanilla);
        a.op(GetGlobal { dst: r_game, global: g_game });
        a.op(Field { dst: r_inst, obj: r_game, field: field_inst });
        a.jump(JNull { reg: r_inst, offset: 0 }, vanilla);
        a.op(Call1 { dst: r_bf, fun: RefFun(fn_get_bifrost), arg0: r_inst });
        a.jump(JNull { reg: r_bf, offset: 0 }, vanilla);
        a.op(Field { dst: r_realm, obj: r_bf, field: field_bf_realm });
        a.jump(JNull { reg: r_realm, offset: 0 }, vanilla);
        a.jump(JNull { reg: src, offset: 0 }, vanilla);
        a.op(Int { dst: r_zero, ptr: zero });
        a.op(Type { dst: r_type, ty: obj_ty_ref });
        a.op(Call2 { dst: r_alloc, fun: RefFun(fn_alloc), arg0: r_type, arg1: r_zero });
        a.op(UnsafeCast { dst: r_alloc2, src: r_alloc });
        a.op(Call1 { dst: r_filtered, fun: RefFun(fn_wrap), arg0: r_alloc2 });
        a.op(Int { dst: r_i, ptr: zero });
        let lp = a.label();
        let done = a.label();
        let next = a.label();
        a.bind_loop(lp);
        a.op(Field { dst: r_len, obj: src, field: field_len });
        a.jump(JSGte { a: r_i, b: r_len, offset: 0 }, done);
        a.op(Field { dst: r_raw, obj: src, field: field_arr });
        a.op(GetArray { dst: r_dyn, array: r_raw, index: r_i });
        a.op(ToVirtual { dst: r_v, src: r_dyn });
        a.op(Incr { dst: r_i });
        a.jump(JNull { reg: r_v, offset: 0 }, next);
        a.op(Field { dst: r_id, obj: r_v, field: field_obj_id });
        a.jump(JNull { reg: r_id, offset: 0 }, next);
        emit_str(&mut a, env, &sc, r_pfx, obj_prefix);
        emit_concat(&mut a, env, r_path, r_pfx, r_realm);
        emit_str(&mut a, env, &sc, r_pfx, underscore);
        emit_concat(&mut a, env, r_path, r_path, r_pfx);
        emit_concat(&mut a, env, r_path, r_path, r_id);
        emit_exists(&mut a, env, sc.boolean, r_path);
        a.jump(JFalse { cond: sc.boolean, offset: 0 }, next);
        a.op(Call2 { dst: r_tmp_i, fun: RefFun(fn_push), arg0: r_filtered, arg1: r_v });
        a.bind(next);
        a.jump(JAlways { offset: 0 }, lp);
        a.bind(done);
        a.op(Field { dst: r_len, obj: r_filtered, field: field_len });
        a.jump(JSGt { a: r_len, b: r_zero, offset: 0 }, pick_filtered);
        a.bind(vanilla);
        a.op(Call2 { dst, fun: RefFun(fun), arg0, arg1 });
        a.jump(JAlways { offset: 0 }, back);
        a.bind(pick_filtered);
        a.op(SafeCast { dst: r_arrdyn, src: r_filtered });
        a.op(Call2 { dst, fun: RefFun(fun), arg0: r_arrdyn, arg1 });
        a.bind(back); // falls through to the op after the original call
        replacements.push(Replacement { at, block: a.finish()?, external: Vec::new() });
    }
    let (ops, old_of_new) = splice(&o, replacements)?;
    install(code, findex, ops, regs.new, |i| old_of_new[i])
}

/// A replacement for one original op: the block, plus jumps inside it that must land on an
/// ORIGINAL op (block-local op index, original op index).
pub struct Replacement {
    pub at: usize,
    pub block: Vec<Opcode>,
    pub external: Vec<(usize, usize)>,
}

/// Rebuilds `ops` with some single ops replaced by blocks, re-targeting every ORIGINAL jump
/// and Switch so it still lands on the same original op (a jump to a replaced op lands on its
/// block's first op). A block falls through at its end to the op after the one it replaces;
/// its own jumps are relative to itself, except those listed in `external`, which land on an
/// original op. Returns the new ops and, for each new op, the old op index it came from (for
/// debug info).
pub fn splice(ops: &[Opcode], mut replacements: Vec<Replacement>) -> Result<(Vec<Opcode>, Vec<usize>)> {
    replacements.sort_by_key(|r| r.at);
    let mut new_index = vec![0usize; ops.len() + 1];
    let mut out: Vec<Opcode> = Vec::new();
    let mut old_of_new: Vec<usize> = Vec::new();
    let mut fixups: Vec<(usize, usize)> = Vec::new(); // (new position, old target)
    let mut switches: Vec<(usize, Vec<usize>, usize)> = Vec::new(); // (new position, old targets, old end target)
    let mut reps = replacements.into_iter().peekable();
    for (i, op) in ops.iter().enumerate() {
        new_index[i] = out.len();
        if reps.peek().map(|r| r.at == i).unwrap_or(false) {
            let r = reps.next().unwrap();
            let start = out.len();
            for (local, old_target) in r.external {
                fixups.push((start + local, old_target));
            }
            old_of_new.extend(std::iter::repeat_n(i, r.block.len()));
            out.extend(r.block);
            continue;
        }
        if let Some(off) = jump_offset(op) {
            fixups.push((out.len(), (i as i64 + 1 + off as i64) as usize));
        }
        if let Opcode::Switch { offsets, end, .. } = op {
            let targets = offsets.iter().map(|o| (i as i64 + 1 + *o as i64) as usize).collect();
            switches.push((out.len(), targets, (i as i64 + 1 + *end as i64) as usize));
        }
        old_of_new.push(i);
        out.push(op.clone());
    }
    new_index[ops.len()] = out.len();
    let map = |t: usize| new_index.get(t).copied().context("internal error: jump target out of range");
    for (at, old_target) in fixups {
        let target = map(old_target)?;
        set_jump_offset(&mut out[at], target as i32 - at as i32 - 1)?;
    }
    for (at, targets, end_target) in switches {
        let new_offsets = targets
            .iter()
            .map(|t| map(*t).map(|n| n as i32 - at as i32 - 1))
            .collect::<Result<Vec<_>>>()?;
        let new_end = map(end_target)? as i32 - at as i32 - 1;
        if let Opcode::Switch { offsets, end, .. } = &mut out[at] {
            *offsets = new_offsets;
            *end = new_end;
        }
    }
    Ok((out, old_of_new))
}

pub fn jump_offset(op: &Opcode) -> Option<i32> {
    use Opcode::*;
    match op {
        JTrue { offset, .. } | JFalse { offset, .. } | JNull { offset, .. } | JNotNull { offset, .. }
        | JSLt { offset, .. } | JSGte { offset, .. } | JSGt { offset, .. } | JSLte { offset, .. }
        | JULt { offset, .. } | JUGte { offset, .. } | JNotLt { offset, .. } | JNotGte { offset, .. }
        | JEq { offset, .. } | JNotEq { offset, .. } | JAlways { offset } => Some(*offset),
        _ => None,
    }
}

/// Common shape for "prepend: if active, write done\<prefix><realm>[_<suffix>]".
fn prepend_done_marker(
    code: &mut Bytecode,
    env: &Env,
    findex: usize,
    what: &str,
    marker_prefix: &str,
    suffix: Option<(Reg, RefField)>,
) -> Result<()> {
    let f = function(code, findex, what)?;
    let o = f.ops.clone();
    let nregs = f.regs.len();
    let (field_realm, _) = find_field_by_name(code, "gamesys.bifrost.BifrostManager", "realm")?;
    let active = konst(code, &path_in(&env.bifrost_dir, ACTIVE_FLAG_NAME));
    let done_dir = konst(code, &env.done_dir);
    let prefix = konst(code, &path_in(&env.done_dir, marker_prefix));
    let underscore = konst(code, "_");
    let empty = konst(code, "");

    let mut regs = Regs::new(nregs);
    let sc = Scratch::new(env, &mut regs);
    let r_realm = regs.alloc(env.string_ty);
    let r_tmp = regs.alloc(env.string_ty);
    let r_path = regs.alloc(env.string_ty);
    let r_suffix = regs.alloc(env.string_ty);

    use Opcode::*;
    let mut a = Assembler::new();
    let original = a.label();
    emit_require_active(&mut a, env, &sc, active, r_tmp, original);
    a.op(GetThis { dst: r_realm, field: field_realm });
    a.jump(JNull { reg: r_realm, offset: 0 }, original);
    emit_str(&mut a, env, &sc, r_path, prefix);
    emit_concat(&mut a, env, r_path, r_path, r_realm);
    if let Some((obj, field)) = suffix {
        a.jump(JNull { reg: obj, offset: 0 }, original);
        a.op(Field { dst: r_suffix, obj, field });
        a.jump(JNull { reg: r_suffix, offset: 0 }, original);
        emit_str(&mut a, env, &sc, r_tmp, underscore);
        emit_concat(&mut a, env, r_path, r_path, r_tmp);
        emit_concat(&mut a, env, r_path, r_path, r_suffix);
    }
    emit_write_marker(&mut a, env, &sc, done_dir, empty, r_path, r_tmp);
    let n = a.len();
    a.bind_external(original, n);
    let mut ops = a.finish()?;
    ops.extend(o);
    install(code, findex, ops, regs.new, |i| i.saturating_sub(n))
}

/// `BifrostManager.completeObjective(obj)`: writes `done\obj_<Realm>_<ObjId>`.
fn patch_complete_objective(code: &mut Bytecode, env: &Env) -> Result<()> {
    let findex = find_method_findex(code, "gamesys.bifrost.BifrostManager", "completeObjective")?;
    let (field_kind, _) = find_field_by_name(code, "gamesys.bifrost.BifrostObjective", "kind")?;
    prepend_done_marker(code, env, findex, "BifrostManager.completeObjective", "obj_", Some((Reg(1), field_kind)))
}

/// `BifrostManager.onBifrostWon()`: writes `done\realm_<Realm>`.
fn patch_bifrost_won(code: &mut Bytecode, env: &Env) -> Result<()> {
    let findex = find_method_findex(code, "gamesys.bifrost.BifrostManager", "onBifrostWon")?;
    prepend_done_marker(code, env, findex, "BifrostManager.onBifrostWon", "realm_", None)
}
