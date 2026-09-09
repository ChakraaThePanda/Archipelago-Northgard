//! Patches Northgard's compiled game logic (`hlboot.dat`) so that
//! `Conquest.getBattleState` -- the function deciding whether a tree node shows as
//! Locked/Unlocked/Done -- additionally requires an Archipelago "unlock marker" file to
//! exist for that node's battle id, on top of the game's own original adjacency check.
//! `NorthgardClient.py` is what creates those marker files as items are received; this
//! tool never touches them, only checks for their existence from inside the patched game.
//!
//! See docs/DEVELOPMENT.md in the repo root for the full writeup of how this was found
//! (in particular, why a raw `String{}` load can't just be handed to functions expecting a
//! real boxed `String` object -- the root cause of every crash hit along the way).
//!
//! Usage:
//!   patch_northgard status  <Northgard install dir>   # exit 0=patched 1=not-patched 2=error
//!   patch_northgard apply   <Northgard install dir>   # idempotent -- safe to call blindly
//!   patch_northgard restore <Northgard install dir>
//!
//! The findices below are specific to the exact Northgard build this was last verified
//! against -- a game update can silently invalidate them. `apply` fails loudly (rather than
//! silently mis-patching) if the shape of `getBattleState` it finds doesn't match what this
//! was written against; if that happens, re-verify these against a fresh `hlbc` disassembly
//! before trusting this tool again.

use anyhow::{bail, Context, Result};
use hlbc::opcodes::Opcode;
use hlbc::types::{ObjField, Reg, RefField, RefFloat, RefFun, RefInt, RefString, RefType, Type};
use hlbc::{Bytecode, Resolve};
use std::env;
use std::fs;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

// NOTE: findices themselves are no longer hardcoded here -- see find_method_findex /
// find_native_findex below. Northgard renumbers its whole function table on every
// recompile (confirmed: this is exactly what silently broke Chapter-lock enforcement
// after an Aug 2026 game update -- GATE_FINDEX pointed at an unrelated function post-update,
// and `apply`'s own shape check should have caught it, but the stale `status` check never
// even tried to reapply). Class/method names are tied to the game's own source and don't
// move around under a recompile the way compiler-assigned indices do, so every lookup below
// resolves by name at `apply` time instead. What's still true even after a shape-preserving
// rename, though harder to fix generically, are the FIELD_*/GLOBAL_* constants and hardcoded
// op-index positions further down (e.g. ANIMATE_NEW_BATTLE_PLOTS_HARDCODED_UNLOCKED_OP) --
// those remain numeric and would need the same treatment if a future update moves *them*.
const ANIMATE_NEW_BATTLE_PLOTS_HARDCODED_UNLOCKED_OP: usize = 46;

// Path-dedup-by-column fix (see patch_path_dedup_by_column below): Conquest.onBattleCompleted
// only appends a won battle's id to the save's `path` ledger if no sibling in the same tree
// row already has an entry there -- a no-op guard in vanilla Linear Mode (only one sibling per
// row is ever winnable) but one that silently drops the SECOND sibling's win once Non-Linear
// Mode lets both be won.
const BATTLE_COMPLETED_SKIP_PATH_PUSH_OP: usize = 55; // JTrue(isColumnInPath(colIndex), +7) -- skips the path.push() below it

// animateLastPath-invokes-callback-on-null-lastButtonIndex fix (see
// patch_animate_last_path_invokes_callback_when_skipped below): `animateLastPath(container,
// onDone)` early-returns via `JNull(this.lastButtonIndex) -> Ret` whenever `lastButtonIndex` is
// null -- WITHOUT ever invoking `onDone`. The Conquest map's post-battle-completion handling
// passes exactly the callback that eventually calls `consumeFinishedBattleId` + `unlockUI` as
// this `onDone` -- so when this early-return path is taken, that callback is silently dropped
// and the Conquest map never unlocks, permanently: no node selectable, Escape included.
// `lastButtonIndex` is null on a freshly-opened map before the player has clicked anything,
// which is exactly when this reveal flow auto-triggers off a stale `finishedBattleId` from a
// battle completed out of vanilla's assumed order (only possible in Non-Linear Mode) -- so this
// path is hit precisely in that scenario, never in vanilla, since vanilla can never have a
// pending finishedBattleId before lastButtonIndex is first set by a real click. Confirmed as the
// actual root cause via direct instrumentation on a real repro, not static analysis alone.
const ANIMATE_LAST_PATH_NULL_JUMP_OP: usize = 1; // JNull(lastButtonIndex) -> Ret, dropping onDone entirely

const FIELD_CMC_CONQUEST: usize = 19; // ConquestMapContent.conquest
const FIELD_CMC_CONTAINER: usize = 13; // ConquestMapContent.container
const FIELD_MAPCONTAINER_BUTTONS: usize = 79; // MapContainer.buttons (nested column/row array of MapButton)
const FIELD_ARRAYOBJ_LENGTH: usize = 0; // hl.types.ArrayObj.length
const FIELD_ARRAYOBJ_ARRAY: usize = 1; // hl.types.ArrayObj.array (raw backing array)
const FIELD_MAPBUTTON_DATA: usize = 108; // MapButton.data -> gamesys.conquest.Battle
const FIELD_BATTLE_DATA: usize = 0; // Battle.data -> virtual{infId, ...}
const FIELD_BATTLEDATA_INFID: usize = 1; // virtual{infId, ...}.infId

const UNLOCK_SUBDIR: &str = "unlocked";
const NON_LINEAR_FLAG_NAME: &str = "non_linear_mode.flag";

// Live/one-shot resource grants (see patch_player_update_grants_resources below). Kept
// separate from UNLOCK_SUBDIR since these markers are one-shot (consumed+deleted by the
// patched game the instant it grants them) rather than persistent per-node state.
//
// Resource ids below are `_Data.ResourceKind`'s own string constants (confirmed via
// `patch_northgard listfields <dir> "_Data.$ResourceKind_Impl_"`), passed straight to
// `ResourcesComponent.addResource`'s generic (non-money/food/wood-dedicated) dispatch,
// confirmed live and repeatedly, both solo and cross-clan. A grant lists more than one id
// only when a resource's *internal* name varies by clan theme (e.g. Lore is "Faith" for some
// clans, and military XP is "HuntTrophy" for Lynx); addResource silently no-ops for
// whichever alias isn't the current clan's own, so listing every known one is always safe.
const PENDING_RESOURCES_SUBDIR: &str = "pending_resources";

/// One resource's whole live-grant configuration: its marker file basenames, its
/// `_Data.ResourceKind` alias(es), and its two tunable amounts. Both marker names are fixed,
/// single files, not a stack or pool, since "Useful" items are each a single, non-stacking
/// copy per Items.py, so there's nothing to count past one; "Filler" items are session-live
/// one-shots, however many copies get received.
struct ResourceConfig {
    key: &'static str, // used only to build marker filenames below, e.g. "money"
    resource_ids: &'static [&'static str],
    useful_amount: f64, // "Extra Starting <X>", applied once at the start of every future battle
    filler_amount: f64, // instant on-the-spot grant, session-live, forfeited if received offline
}

// TUNE FREELY: these seven rows are the entire balance knob for every live-grant item this
// world defines. Changing a number here just needs `patch_northgard apply` re-run; nothing
// else (Items.py, the client) encodes an amount anywhere.
const RESOURCE_CONFIGS: &[ResourceConfig] = &[
    ResourceConfig { key: "money", resource_ids: &["Money"], useful_amount: 50.0, filler_amount: 100.0 },
    ResourceConfig { key: "food", resource_ids: &["Food"], useful_amount: 50.0, filler_amount: 100.0 },
    ResourceConfig { key: "wood", resource_ids: &["Wood"], useful_amount: 50.0, filler_amount: 100.0 },
    ResourceConfig { key: "lore", resource_ids: &["Lore", "Faith"], useful_amount: 40.0, filler_amount: 200.0 },
    ResourceConfig { key: "stone", resource_ids: &["Stone"], useful_amount: 5.0, filler_amount: 10.0 },
    ResourceConfig { key: "iron", resource_ids: &["Iron"], useful_amount: 5.0, filler_amount: 10.0 },
    ResourceConfig { key: "military_xp", resource_ids: &["MilitaryXP", "HuntTrophy"], useful_amount: 25.0, filler_amount: 200.0 },
];

/// Finds a class method's current findex by name instead of a hardcoded number.
/// Northgard renumbers its whole function table on every recompile, but a method's
/// (owning class, method name) pair is tied to the game's own Haxe source and stays put
/// across updates the same way a Python function's qualified name would. Call once per
/// `apply` run; each patch_* function still runs its own shape check (op/reg counts)
/// afterwards as defense in depth, since a same-named method's *body* can still change
/// shape across an update even when its identity doesn't.
fn find_method_findex(code: &Bytecode, class_name: &str, method_name: &str) -> Result<usize> {
    let matches: Vec<usize> = code
        .functions
        .iter()
        .filter(|f| f.name(code) == method_name)
        .filter(|f| {
            f.parent
                .and_then(|p| p.as_obj(code))
                .map(|obj| obj.name(code) == class_name)
                .unwrap_or(false)
        })
        .map(|f| f.findex.0)
        .collect();

    match matches.as_slice() {
        [one] => Ok(*one),
        [] => bail!(
            "could not find {class_name}.{method_name} by name -- Northgard build mismatch? \
             Re-verify with `patch_northgard list <dir> {method_name}` before trusting this patch."
        ),
        many => bail!(
            "{class_name}.{method_name} is ambiguous -- {} candidate findices found: {many:?} \
             -- Northgard build mismatch?",
            many.len()
        ),
    }
}

/// Same idea as find_method_findex, for native (non-Haxe) functions like `sys_exists`,
/// which live in their own table keyed by (lib, name) rather than belonging to a class.
fn find_native_findex(code: &Bytecode, lib: &str, name: &str) -> Result<usize> {
    code.natives
        .iter()
        .find(|n| n.lib(code) == lib && n.name(code) == name)
        .map(|n| n.findex.0)
        .with_context(|| format!("could not find native {lib}.{name} -- Northgard build mismatch?"))
}

/// Finds the index into `code.types` of the `Obj` type named `class_name`, or `None` if no
/// such class exists. Shared by every lookup below that needs to locate a class by name
/// before doing something with it (reading a field, listing fields, adding a field).
fn find_type_index_by_name(code: &Bytecode, class_name: &str) -> Option<usize> {
    code.types.iter().position(|t| matches!(t, Type::Obj(obj) if obj.name(code) == class_name))
}

/// Finds a class field's `RefField` (and its declared type) by (class name, field name)
/// instead of a hardcoded index, following the same by-name-not-by-index philosophy as
/// find_method_findex, applied to fields. `fields` (not `own_fields`) already includes
/// inherited ones, matching what a real `GetThis`/`Field` access on an instance of this class
/// can reach.
fn find_field_by_name(code: &Bytecode, class_name: &str, field_name: &str) -> Result<(RefField, RefType)> {
    let type_index = find_type_index_by_name(code, class_name)
        .with_context(|| format!("could not find class {class_name}. Northgard build mismatch?"))?;
    let Type::Obj(obj) = &code.types[type_index] else {
        bail!("internal error: type_index didn't resolve back to an Obj");
    };
    obj.fields
        .iter()
        .position(|f| f.name(code) == field_name)
        .map(|i| (RefField(i), obj.fields[i].t))
        .with_context(|| format!("class {class_name} has no field named {field_name}. Northgard build mismatch?"))
}

fn cmd_native_sig(backup_path: &Path, live_path: &Path, lib: &str, name: &str) -> Result<()> {
    let source = if backup_path.exists() { backup_path } else { live_path };
    let path_str = source.to_str().context("path isn't valid UTF-8")?;
    let code = Bytecode::from_file(path_str).context("failed to parse hlboot.dat")?;
    let n = code
        .natives
        .iter()
        .find(|n| n.lib(&code) == lib && n.name(&code) == name)
        .context("native not found")?;
    let ty = n.ty(&code);
    println!("{lib}.{name}@{}: args={:?} ret={:?}", n.findex.0, ty.args, ty.ret);
    for (i, a) in ty.args.iter().enumerate() {
        println!("  arg{i} = {}", a.display::<hlbc::fmt::EnhancedFmt>(&code));
    }
    println!("  ret = {}", ty.ret.display::<hlbc::fmt::EnhancedFmt>(&code));
    Ok(())
}

fn cmd_list_fields(backup_path: &Path, live_path: &Path, class_name: &str) -> Result<()> {
    let source = if backup_path.exists() { backup_path } else { live_path };
    let path_str = source.to_str().context("path isn't valid UTF-8")?;
    let code = Bytecode::from_file(path_str).context("failed to parse hlboot.dat")?;
    let type_index = find_type_index_by_name(&code, class_name).context("class not found")?;
    let Type::Obj(obj) = &code.types[type_index] else {
        bail!("internal error: type_index didn't resolve back to an Obj");
    };
    for (i, f) in obj.fields.iter().enumerate() {
        println!("[{i}] {} : {}", f.name(&code), f.t.display::<hlbc::fmt::EnhancedFmt>(&code));
    }
    println!("total fields: {}", obj.fields.len());
    Ok(())
}

fn cmd_find_field_anywhere(backup_path: &Path, live_path: &Path, field_name: &str) -> Result<()> {
    let source = if backup_path.exists() { backup_path } else { live_path };
    let path_str = source.to_str().context("path isn't valid UTF-8")?;
    let code = Bytecode::from_file(path_str).context("failed to parse hlboot.dat")?;
    let mut count = 0;
    for t in &code.types {
        if let Type::Obj(obj) = t {
            for f in &obj.own_fields {
                if f.name(&code).to_string().eq_ignore_ascii_case(field_name) {
                    println!(
                        "{} . {} : {}",
                        obj.name(&code),
                        f.name(&code),
                        f.t.display::<hlbc::fmt::EnhancedFmt>(&code)
                    );
                    count += 1;
                }
            }
        }
    }
    println!("total: {count}");
    Ok(())
}

fn cmd_subclasses(backup_path: &Path, live_path: &Path, class_name: &str) -> Result<()> {
    let source = if backup_path.exists() { backup_path } else { live_path };
    let path_str = source.to_str().context("path isn't valid UTF-8")?;
    let code = Bytecode::from_file(path_str).context("failed to parse hlboot.dat")?;
    let mut count = 0;
    for t in &code.types {
        if let Type::Obj(obj) = t {
            if let Some(super_ref) = obj.super_ {
                if let Type::Obj(super_obj) = code.get(super_ref) {
                    if super_obj.name(&code) == class_name {
                        println!("{} extends {class_name} directly", obj.name(&code));
                        count += 1;
                    }
                }
            }
        }
    }
    println!("total direct subclasses of {class_name}: {count}");
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    let usage = "usage: patch_northgard <status|apply|restore> <Northgard install dir>";

    // Launched with no arguments at all -- almost certainly a double-click from Explorer,
    // not a script or the bundled client (both always pass real arguments). A console app
    // that just prints a usage error and exits closes its window before anyone can read
    // it, which is actively bad UX for anyone who isn't comfortable with a command line.
    // Run a friendly interactive flow instead, and always pause before exiting.
    if args.len() == 1 {
        return run_interactive();
    }

    if args.len() < 3 {
        bail!(usage);
    }
    let install_dir = PathBuf::from(&args[2]);
    let live_path = install_dir.join("hlboot.dat");
    let backup_path = install_dir.join("hlboot.dat.orig_backup");

    match args[1].as_str() {
        "status" => cmd_status(&live_path),
        "restore" => cmd_restore(&live_path, &backup_path),
        "apply" => cmd_apply(&live_path, &backup_path),
        "findcallers" => {
            let findex: usize = args.get(3).context("usage: patch_northgard findcallers <dir> <findex>")?.parse()?;
            cmd_findcallers(&backup_path, &live_path, findex)
        }
        "dump" => {
            let findex: usize = args.get(3).context("usage: patch_northgard dump <dir> <findex>")?.parse()?;
            cmd_dump(&backup_path, &live_path, findex)
        }
        "list" => {
            let needle = args.get(3).context("usage: patch_northgard list <dir> <source-file-substring>")?;
            cmd_list(&backup_path, &live_path, needle)
        }
        "whois" => {
            let findex: usize = args.get(3).context("usage: patch_northgard whois <dir> <findex>")?.parse()?;
            cmd_whois(&backup_path, &live_path, findex)
        }
        "natives" => cmd_natives(&backup_path, &live_path),
        "listfields" => {
            let class_name = args.get(3).context("usage: patch_northgard listfields <dir> <class>")?;
            cmd_list_fields(&backup_path, &live_path, class_name)
        }
        "findfield" => {
            let field_name = args.get(3).context("usage: patch_northgard findfield <dir> <field>")?;
            cmd_find_field_anywhere(&backup_path, &live_path, field_name)
        }
        "natsig" => {
            let lib = args.get(3).context("usage: patch_northgard natsig <dir> <lib> <name>")?;
            let name = args.get(4).context("usage: patch_northgard natsig <dir> <lib> <name>")?;
            cmd_native_sig(&backup_path, &live_path, lib, name)
        }
        "subclasses" => {
            let class_name = args.get(3).context("usage: patch_northgard subclasses <dir> <class>")?;
            cmd_subclasses(&backup_path, &live_path, class_name)
        }
        "resolvefn" => {
            let class_name = args.get(3).context("usage: patch_northgard resolvefn <dir> <class> <method>")?;
            let method_name = args.get(4).context("usage: patch_northgard resolvefn <dir> <class> <method>")?;
            let source = if backup_path.exists() { &backup_path } else { &live_path };
            let path_str = source.to_str().context("path isn't valid UTF-8")?;
            let code = Bytecode::from_file(path_str).context("failed to parse hlboot.dat")?;
            let findex = find_method_findex(&code, class_name, method_name)?;
            println!("{class_name}.{method_name} -> findex {findex}");
            Ok(())
        }
        _ => bail!(usage),
    }
}

fn read_line() -> String {
    let mut buf = String::new();
    io::stdin().read_line(&mut buf).ok();
    buf
}

fn pause_before_exit() {
    println!();
    print!("Press Enter to close this window...");
    io::stdout().flush().ok();
    read_line();
}

fn print_status_line(live_path: &Path) {
    if !live_path.exists() {
        println!("Status: no hlboot.dat found here -- is this really the Northgard install folder?");
        return;
    }
    match check_battle_state_patched(live_path) {
        Ok(PatchState::Vanilla) => println!("Status: NOT patched (getBattleState is vanilla)"),
        Ok(PatchState::Patched) => println!("Status: PATCHED (Chapter locks are enforced in-game)"),
        Ok(PatchState::Unknown(reason)) => println!("Status: unknown ({reason})"),
        Err(e) => println!("Status: unknown (couldn't parse hlboot.dat: {e})"),
    }
}

/// Every Steam library folder registered on this machine, best-effort. Reads the Steam
/// install path from the registry, then that install's libraryfolders.vdf, which lists
/// every additional drive/folder the user has added as a Steam library -- this is the
/// same mechanism Steam itself uses, so it finds non-default installs without guessing
/// across drive letters. Mirrors NorthgardClient.py's _candidate_steam_library_roots
/// exactly (down to the same regex-equivalent "path" extraction), so both tools agree on
/// what they consider "the Northgard install."
fn candidate_steam_library_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();

    let hkcu = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER);
    let steam_key = match hkcu.open_subkey(r"Software\Valve\Steam") {
        Ok(k) => k,
        Err(_) => return roots,
    };
    let steam_path: String = match steam_key.get_value("SteamPath") {
        Ok(p) => p,
        Err(_) => return roots,
    };
    let steam_path = PathBuf::from(steam_path);
    roots.push(steam_path.clone());

    let vdf_path = steam_path.join("steamapps").join("libraryfolders.vdf");
    let vdf_text = match fs::read_to_string(&vdf_path) {
        Ok(t) => t,
        Err(_) => return roots,
    };

    // Every `"path"  "<value>"` line, without needing a full VDF parser -- this is the
    // only kind of line in this file we ever care about. libraryfolders.vdf escapes
    // backslashes as \\, undone below.
    for line in vdf_text.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with("\"path\"") {
            continue;
        }
        let parts: Vec<&str> = trimmed.split('"').collect();
        if let Some(raw) = parts.get(3) {
            roots.push(PathBuf::from(raw.replace("\\\\", "\\")));
        }
    }
    roots
}

fn find_northgard_via_steam() -> Option<PathBuf> {
    for root in candidate_steam_library_roots() {
        let candidate = root.join("steamapps").join("common").join("Northgard");
        if candidate.join("hlboot.dat").exists() {
            return Some(candidate);
        }
    }
    None
}

fn find_or_ask_install_dir() -> Option<PathBuf> {
    if let Some(p) = find_northgard_via_steam() {
        println!("Found a Northgard install at: {}", p.display());
        print!("Use this one? [Y/n] ");
        io::stdout().flush().ok();
        let answer = read_line().trim().to_lowercase();
        if answer.is_empty() || answer == "y" || answer == "yes" {
            return Some(p);
        }
    }

    loop {
        println!();
        print!("Enter the full path to your Northgard install folder (the one containing \
                Northgard.exe), or leave blank to cancel: ");
        io::stdout().flush().ok();
        let input = read_line();
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return None;
        }
        let p = PathBuf::from(trimmed);
        if p.join("hlboot.dat").exists() {
            return Some(p);
        }
        println!("Couldn't find hlboot.dat in that folder -- double check the path and try again.");
    }
}

fn run_interactive() -> Result<()> {
    println!("Archipelago Northgard Patch Tool");
    println!("================================");
    println!();
    println!("This enables/disables in-game Chapter-lock enforcement for the Archipelago");
    println!("Northgard randomizer. If you're using the Northgard Client, it already does");
    println!("this for you automatically -- you only need this tool to check status, or to");
    println!("switch back to vanilla Northgard.");
    println!();

    let install_dir = match find_or_ask_install_dir() {
        Some(dir) => dir,
        None => {
            pause_before_exit();
            return Ok(());
        }
    };
    let live_path = install_dir.join("hlboot.dat");
    let backup_path = install_dir.join("hlboot.dat.orig_backup");

    loop {
        println!();
        println!("Northgard install: {}", install_dir.display());
        print_status_line(&live_path);
        println!();
        println!("[1] Apply/re-apply the patch (enforce Chapter locks in-game)");
        println!("[2] Restore to vanilla (undo the patch)");
        println!("[3] Exit");
        print!("> ");
        io::stdout().flush().ok();

        match read_line().trim() {
            "1" => {
                if let Err(e) = cmd_apply(&live_path, &backup_path) {
                    println!("Error: {e:#}");
                }
            }
            "2" => {
                if let Err(e) = cmd_restore(&live_path, &backup_path) {
                    println!("Error: {e:#}");
                }
            }
            "3" | "" => break,
            _ => println!("Not a valid choice -- enter 1, 2, or 3."),
        }
    }

    pause_before_exit();
    Ok(())
}

fn cmd_findcallers(backup_path: &Path, live_path: &Path, target_findex: usize) -> Result<()> {
    // Diagnostic: every function that calls a given findex, anywhere in the bytecode.
    // Reads the pristine backup if present (falls back to the live file) -- read-only, does
    // not touch either file.
    let source = if backup_path.exists() { backup_path } else { live_path };
    let path_str = source.to_str().context("path isn't valid UTF-8")?;
    let code = Bytecode::from_file(path_str).context("failed to parse hlboot.dat")?;

    let mut found = 0;
    for f in &code.functions {
        for (i, op) in f.ops.iter().enumerate() {
            let called = match op {
                Opcode::Call0 { fun, .. } => Some(fun.0),
                Opcode::Call1 { fun, .. } => Some(fun.0),
                Opcode::Call2 { fun, .. } => Some(fun.0),
                Opcode::Call3 { fun, .. } => Some(fun.0),
                Opcode::Call4 { fun, .. } => Some(fun.0),
                Opcode::CallN { fun, .. } => Some(fun.0),
                _ => None,
            };
            if called == Some(target_findex) {
                println!("caller findex={} op[{}] = {:?}", f.findex.0, i, op);
                found += 1;
            }
        }
    }
    println!("total call sites of findex {target_findex}: {found}");
    Ok(())
}

fn cmd_dump(backup_path: &Path, live_path: &Path, target_findex: usize) -> Result<()> {
    // Diagnostic: full enhanced disassembly of one function (class/method name, regs, ops).
    let source = if backup_path.exists() { backup_path } else { live_path };
    let path_str = source.to_str().context("path isn't valid UTF-8")?;
    let code = Bytecode::from_file(path_str).context("failed to parse hlboot.dat")?;

    let f = code
        .functions
        .iter()
        .find(|f| f.findex.0 == target_findex)
        .context("no function with that findex")?;
    println!("{}", f.display::<hlbc::fmt::EnhancedFmt>(&code));
    println!("--- raw ops ---");
    for (i, op) in f.ops.iter().enumerate() {
        println!("{i:>3}: {op:?}");
    }
    Ok(())
}

fn cmd_list(backup_path: &Path, live_path: &Path, needle: &str) -> Result<()> {
    // Diagnostic: every function whose header (name, owning/arg types) or first debug
    // source-file entry contains `needle`.
    let source = if backup_path.exists() { backup_path } else { live_path };
    let path_str = source.to_str().context("path isn't valid UTF-8")?;
    let code = Bytecode::from_file(path_str).context("failed to parse hlboot.dat")?;

    let mut found = 0;
    for f in &code.functions {
        let header = f.display_header::<hlbc::fmt::EnhancedFmt>(&code).to_string();
        let file_hit = f
            .debug_info
            .as_ref()
            .and_then(|d| d.first())
            .and_then(|(file, _)| code.debug_files.as_ref().and_then(|files| files.get(*file)))
            .map(|s| s.to_string().contains(needle))
            .unwrap_or(false);
        if header.contains(needle) || file_hit {
            println!("findex={} {}", f.findex.0, header);
            found += 1;
        }
    }
    println!("total matches: {found}");
    Ok(())
}

fn cmd_whois(backup_path: &Path, live_path: &Path, target_findex: usize) -> Result<()> {
    // Diagnostic: what class (if any) owns this findex by name, for verifying
    // find_method_findex's inputs against a real build. Read-only.
    let source = if backup_path.exists() { backup_path } else { live_path };
    let path_str = source.to_str().context("path isn't valid UTF-8")?;
    let code = Bytecode::from_file(path_str).context("failed to parse hlboot.dat")?;

    let f = code
        .functions
        .iter()
        .find(|f| f.findex.0 == target_findex)
        .context("no function with that findex")?;
    let class = f
        .parent
        .and_then(|p| p.as_obj(&code))
        .map(|obj| obj.name(&code).to_string());
    println!(
        "findex={} name={:?} parent_class={:?}",
        target_findex,
        f.name(&code).to_string(),
        class
    );
    Ok(())
}

fn cmd_natives(backup_path: &Path, live_path: &Path) -> Result<()> {
    // Diagnostic: every native (lib.name@findex), for verifying find_native_findex's inputs.
    let source = if backup_path.exists() { backup_path } else { live_path };
    let path_str = source.to_str().context("path isn't valid UTF-8")?;
    let code = Bytecode::from_file(path_str).context("failed to parse hlboot.dat")?;

    for n in &code.natives {
        println!("{}.{}@{}", n.lib(&code), n.name(&code), n.findex.0);
    }
    println!("total natives: {}", code.natives.len());
    Ok(())
}

/// Whether the *actual enforcement logic* is present in a live hlboot.dat right now.
/// Deliberately does NOT diff live bytes against the pristine backup: a legitimate
/// Northgard game update changes those bytes too (new vanilla build != old vanilla
/// backup), which reads as "patched" under a pure byte-diff even though nothing of ours
/// is in there -- confirmed as the actual root cause of Chapter-lock enforcement going
/// silently, permanently absent after an Aug 2026 Northgard update: the stale byte-diff
/// check told the auto-healing client "already patched," so it never even tried to
/// reapply. Checking getBattleState's own op count instead answers the question that
/// actually matters (is the marker check really in there), and keeps answering it
/// correctly across a future update as long as find_method_findex can still locate the
/// function by name.
enum PatchState {
    Vanilla,
    Patched,
    Unknown(String),
}

fn classify_battle_state(code: &Bytecode) -> Result<PatchState> {
    let gate_findex = match find_method_findex(code, "gamesys.conquest.Conquest", "getBattleState") {
        Ok(fx) => fx,
        Err(e) => return Ok(PatchState::Unknown(format!("could not locate getBattleState by name: {e}"))),
    };
    let f = code
        .functions
        .iter()
        .find(|f| f.findex.0 == gate_findex)
        .context("resolved findex vanished immediately after being found -- this should be impossible")?;
    Ok(match f.ops.len() {
        14 => PatchState::Vanilla,
        31 => PatchState::Patched,
        n => PatchState::Unknown(format!(
            "getBattleState has {n} ops -- neither the known vanilla (14) nor patched (31) shape"
        )),
    })
}

fn check_battle_state_patched(live_path: &Path) -> Result<PatchState> {
    let path_str = live_path.to_str().context("path isn't valid UTF-8")?;
    let code = Bytecode::from_file(path_str).context("failed to parse hlboot.dat")?;
    classify_battle_state(&code)
}

fn cmd_status(live_path: &Path) -> Result<()> {
    // Exit code is the machine-readable contract other tools (the Python client) rely on:
    // 0 = currently patched, 1 = not currently patched (apply needed), 2 = error (can't
    // tell at all). Stdout text is for humans only -- don't rely on it elsewhere.
    if !live_path.exists() {
        println!("No hlboot.dat found at {}", live_path.display());
        std::process::exit(2);
    }
    match check_battle_state_patched(live_path) {
        Ok(PatchState::Vanilla) => {
            println!("getBattleState is vanilla -- not currently patched.");
            std::process::exit(1);
        }
        Ok(PatchState::Patched) => {
            println!("getBattleState has the Archipelago marker check installed -- currently patched.");
        }
        Ok(PatchState::Unknown(reason)) => {
            println!("Can't tell if patched: {reason} -- Northgard was likely updated; run 'apply' to re-verify/re-patch.");
            std::process::exit(2);
        }
        Err(e) => {
            println!("Could not parse hlboot.dat: {e}");
            std::process::exit(2);
        }
    }
    Ok(())
}

fn cmd_restore(live_path: &Path, backup_path: &Path) -> Result<()> {
    if !backup_path.exists() {
        bail!("No backup found at {} -- nothing to restore from.", backup_path.display());
    }
    fs::copy(backup_path, live_path)?;
    println!("Restored pristine hlboot.dat from backup.");
    Ok(())
}

/// Writes `data` to `path` via write-to-temp-then-rename in the same directory (so the
/// rename is an atomic same-volume replace) instead of truncating `path` in place. Two
/// uncoordinated `apply` runs racing against the same install dir -- e.g. two Launcher
/// client windows connected to two different rooms, both auto-healing the same real
/// Northgard install (see NorthgardClient.py's module docstring, which explicitly treats
/// that as supported) -- can otherwise truncate-and-rewrite the live file concurrently and
/// leave it torn. With this, whichever racer's rename lands last simply wins outright: the
/// destination always ends up as one complete, non-torn write, never an interleaved mess.
///
/// Deliberately does NOT sweep old leftover temp files: if this process is killed between
/// the write and the rename, a stray `.<name>.tmp_<pid>_<nanos>` file is left behind
/// harmlessly (the target it would have replaced is simply untouched, never corrupted).
/// Aggressively cleaning those up needs its own age check to avoid deleting another
/// concurrent apply's temp file out from under it before its rename runs -- more moving
/// parts to buy back a purely cosmetic leftover file. Not worth it: leave it for a human to
/// notice and delete if it ever bothers them.
fn write_atomically(path: &Path, data: &[u8]) -> Result<()> {
    let dir = path.parent().context("target path has no parent directory")?;
    let file_name = path.file_name().context("target path has no file name")?.to_string_lossy();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp_path = dir.join(format!(".{file_name}.tmp_{}_{nanos}", std::process::id()));

    let write_result = (|| -> Result<()> {
        let file = File::create(&tmp_path).context("failed to create temp file for atomic write")?;
        let mut writer = BufWriter::new(file);
        writer.write_all(data)?;
        writer.flush()?;
        Ok(())
    })();
    if let Err(e) = write_result {
        let _ = fs::remove_file(&tmp_path);
        return Err(e);
    }

    if let Err(e) = fs::rename(&tmp_path, path).context("failed to atomically replace target file") {
        let _ = fs::remove_file(&tmp_path);
        return Err(e);
    }
    Ok(())
}

fn cmd_apply(live_path: &Path, backup_path: &Path) -> Result<()> {
    if !live_path.exists() {
        bail!("No hlboot.dat found at {}", live_path.display());
    }

    // Read live_path exactly once. Deciding "is live currently vanilla" and, if so, both
    // backing it up AND patching it must all come from this same snapshot, since two
    // separate reads (one to decide, one to act) leave a gap where a second concurrent
    // `apply` process's completed write can land in between, making us archive ITS
    // already-patched output as "pristine" and permanently corrupt the backup that
    // `restore` relies on.
    let live_bytes = fs::read(live_path).context("failed to read hlboot.dat")?;
    let live_parsed = Bytecode::deserialize(io::Cursor::new(&live_bytes));
    let live_is_vanilla = match &live_parsed {
        Ok(code) => matches!(classify_battle_state(code), Ok(PatchState::Vanilla)),
        Err(_) => false,
    };

    // If live is currently vanilla (unpatched), it's the authoritative pristine build to
    // patch from, so (re)create the backup from it even when an older backup already
    // exists. Without this, a real Northgard update (which ships a fresh vanilla
    // hlboot.dat) would leave `apply` silently re-patching the STALE backup from the
    // previous build and overwriting the freshly-updated live file with it.
    if !backup_path.exists() || live_is_vanilla {
        write_atomically(backup_path, &live_bytes).context("failed to create pristine backup before patching")?;
        println!("Backed up pristine hlboot.dat to {}", backup_path.display());
    }

    let mut code = if live_is_vanilla {
        live_parsed.expect("live_is_vanilla is only true when live_parsed is Ok")
    } else {
        // Not vanilla: live is already patched (normal idempotent re-apply) or unreadable,
        // so patch from the last known-pristine backup instead of live itself.
        let backup_str = backup_path.to_str().context("install dir path isn't valid UTF-8")?;
        Bytecode::from_file(backup_str).context(
            "failed to parse hlboot.dat -- this Northgard build may not match what this tool \
             was written against; see docs/DEVELOPMENT.md before trusting this patch",
        )?
    };

    let config_dir = config_dir_path()?;
    let unlock_dir = config_dir.join(UNLOCK_SUBDIR);
    fs::create_dir_all(&unlock_dir).context("failed to create the unlock-marker directory")?;
    // Trailing separator so the game only needs to concatenate the battle id onto this.
    let marker_prefix = format!("{}\\", unlock_dir.display());
    let non_linear_flag_path = config_dir.join(NON_LINEAR_FLAG_NAME).display().to_string();

    let gate_findex = find_method_findex(&code, "gamesys.conquest.Conquest", "getBattleState")?;
    let unlocked_global = patch_get_battle_state(&mut code, gate_findex, &marker_prefix, &non_linear_flag_path)?;
    patch_map_auto_refresh(&mut code, gate_findex)?;
    patch_reveal_uses_real_state(&mut code, gate_findex, unlocked_global)?;
    patch_path_dedup_by_column(&mut code)?;
    patch_animate_last_path_invokes_callback_when_skipped(&mut code)?;

    let pending_resources_dir = config_dir.join(PENDING_RESOURCES_SUBDIR);
    fs::create_dir_all(&pending_resources_dir).context("failed to create the pending-resources directory")?;

    // One fixed, single-file marker per resource per grant kind. "Useful" items are each a
    // single non-stacking copy (Items.py never places more than one), so there's nothing to
    // pool/stack here the way the old "Extra Starting Food" design needed; "Filler" items are
    // session-live one-shots regardless of how many copies get received (NorthgardClient.py
    // resolves each received copy to its own turn at this same single marker file, one at a
    // time, rather than needing a distinct marker per pending copy).
    let mut grants = Vec::with_capacity(RESOURCE_CONFIGS.len() * 2);
    for cfg in RESOURCE_CONFIGS {
        grants.push(ResourceGrant {
            kind: GrantKind::Useful,
            key: cfg.key,
            marker_path: pending_resources_dir.join(format!("useful_{}.flag", cfg.key)).display().to_string(),
            resource_ids: cfg.resource_ids,
            amount: cfg.useful_amount,
        });
        grants.push(ResourceGrant {
            kind: GrantKind::Filler,
            key: cfg.key,
            marker_path: pending_resources_dir.join(format!("filler_{}.flag", cfg.key)).display().to_string(),
            resource_ids: cfg.resource_ids,
            amount: cfg.filler_amount,
        });
    }
    patch_player_update_grants_resources(&mut code, &grants)?;

    let mut serialized = Vec::new();
    code.serialize(&mut serialized).context("failed to serialize patched bytecode")?;
    write_atomically(live_path, &serialized).context("failed to write patched hlboot.dat")?;

    println!("Patched hlboot.dat installed at {}", live_path.display());
    println!("Marker directory: {unlock_dir_display}", unlock_dir_display = unlock_dir.display());
    println!("Non-linear-mode flag file (its mere presence skips the adjacency check): {non_linear_flag_path}");
    println!("Pending-resources directory: {}", pending_resources_dir.display());
    Ok(())
}

/// `%UserProfile%\Saved Games\Archipelago\Northgard` -- matches NorthgardClient.py's
/// `_CONFIG_DIR` under the *default*, non-redirected "Saved Games" location. If you've
/// redirected that folder elsewhere in Windows, patch this function (or pass the real path
/// in) to match -- there's no cheap way to call the real known-folder API from this tool
/// without an extra Windows API dependency.
fn config_dir_path() -> Result<PathBuf> {
    let profile = env::var("USERPROFILE").context("USERPROFILE environment variable not set")?;
    Ok(PathBuf::from(profile).join("Saved Games").join("Archipelago").join("Northgard"))
}

/// Returns the raw `RefGlobal` index for the `BattleState.Unlocked` enum constructor --
/// extracted from getBattleState's own original op10 (`GetGlobal reg3 = global(Unlocked)`,
/// see the shape check above) rather than a second hardcoded constant, since this global
/// index drifts across Northgard updates exactly the same way findices do. Callers that
/// need to recognize the *same* Unlocked value elsewhere (patch_reveal_uses_real_state)
/// take it as a parameter instead of re-deriving or hardcoding it themselves.
fn patch_get_battle_state(
    code: &mut Bytecode,
    gate_findex: usize,
    marker_prefix: &str,
    non_linear_flag_path: &str,
) -> Result<usize> {
    let string_add_findex = find_method_findex(code, "$String", "__add__")?;
    let get_path_findex = find_method_findex(code, "$Sys", "getPath")?;
    let sys_exists_findex = find_native_findex(code, "std", "sys_exists")?;

    let prefix_ref = RefString(code.strings.len());
    code.strings.push(marker_prefix.into());
    let nl_flag_ref = RefString(code.strings.len());
    code.strings.push(non_linear_flag_path.into());

    // hashlink's `String{}` opcode only ever loads a raw wide-char pointer (confirmed
    // straight from hashlink's own src/jit.c, case OString) -- it does NOT construct a
    // full boxed `hl.types.String` object ({bytes, length}). Every function that expects a
    // real String argument (here: __add__, getPath) reads .bytes/.length off of it, so the
    // raw pointer has to be wrapped by hand first: New-allocate a real String object and
    // set its fields, mirroring exactly what __add__ does when building its own return
    // value. Skipping this step is what caused every crash during development (see
    // docs/DEVELOPMENT.md) -- a `wcslen`-adjacent read off a garbage length/pointer that
    // was actually just misinterpreted text content.
    let add_fn = code
        .functions
        .iter()
        .find(|f| f.findex.0 == string_add_findex)
        .context("could not find String.__add__. Northgard build mismatch?")?;
    if add_fn.ops.len() <= 27 || add_fn.regs.len() <= 8 {
        bail!(
            "__add__'s shape doesn't match what this patch was designed against \
             (expected at least 28 ops / 9 regs, got {} ops / {} regs); Northgard was \
             likely updated, re-verify with hlbc before trusting this tool. See docs/DEVELOPMENT.md.",
            add_fn.ops.len(),
            add_fn.regs.len()
        );
    }
    let field_length = match &add_fn.ops[7] {
        Opcode::Field { field, .. } => *field,
        other => bail!("__add__ op7 shape changed (expected Field), got {other:?}. Northgard build mismatch?"),
    };
    let field_bytes = match &add_fn.ops[27] {
        Opcode::Field { field, .. } => *field,
        other => bail!("__add__ op27 shape changed (expected Field), got {other:?}. Northgard build mismatch?"),
    };
    let string_ty = add_fn.regs[0]; // the real boxed String class
    let bytes_ty = add_fn.regs[8]; // raw HBYTES, matching what String{} actually produces
    let int_ty = add_fn.regs[4]; // plain int, matching __add__'s own length arithmetic

    let len_ref = RefInt(code.ints.len());
    code.ints.push(marker_prefix.len() as i32); // pure ASCII path: byte count == UTF-16 char count
    let nl_len_ref = RefInt(code.ints.len());
    code.ints.push(non_linear_flag_path.len() as i32);

    let f = code
        .functions
        .iter_mut()
        .find(|f| f.findex.0 == gate_findex)
        .context("could not find Conquest.getBattleState -- Northgard build mismatch?")?;

    if f.ops.len() != 14 || f.regs.len() != 8 {
        bail!(
            "getBattleState's shape doesn't match what this patch was designed against \
             (expected 14 ops / 8 regs, got {} ops / {} regs) -- Northgard was likely \
             updated; re-verify with hlbc before trusting this tool. See docs/DEVELOPMENT.md.",
            f.ops.len(),
            f.regs.len()
        );
    }

    use Opcode::*;
    let orig = f.ops.clone();
    let orig_debug = f.debug_info.clone();
    let bool_ty = f.regs[5];
    let unlocked_global = match &orig[10] {
        Opcode::GetGlobal { global, .. } => global.0,
        other => bail!(
            "getBattleState op10 doesn't match the expected GetGlobal(Unlocked) (got {other:?}) \
             -- Northgard build mismatch? Re-verify with `dump` before trusting this patch."
        ),
    };

    f.regs.push(bytes_ty); // reg8: non-linear flag path, raw HBYTES from String{}
    f.regs.push(int_ty); // reg9: non-linear flag path length
    f.regs.push(string_ty); // reg10: non-linear flag path wrapped into a real String object
    f.regs.push(bytes_ty); // reg11: non-linear flag path bytes from getPath
    f.regs.push(bool_ty); // reg12: non-linear flag exists result
    f.regs.push(bytes_ty); // reg13: prefix raw HBYTES from String{}
    f.regs.push(int_ty); // reg14: prefix length
    f.regs.push(string_ty); // reg15: prefix wrapped into a real String object
    f.regs.push(string_ty); // reg16: concat result (prefix + id) -- __add__'s return type
    f.regs.push(bytes_ty); // reg17: path bytes from getPath
    f.regs.push(bool_ty); // reg18: exists result
    let reg_nl_bytes = Reg(8);
    let reg_nl_len = Reg(9);
    let reg_nl_str = Reg(10);
    let reg_nl_pathbytes = Reg(11);
    let reg_nl_exists = Reg(12);
    let reg_prefix_bytes = Reg(13);
    let reg_len = Reg(14);
    let reg_prefix_str = Reg(15);
    let reg_concat = Reg(16);
    let reg_pathbytes = Reg(17);
    let reg_exists = Reg(18);

    f.ops = vec![
        /*0*/ orig[0].clone(), // Call2 reg2 = getProgress(reg0, reg1)
        /*1*/ JNull { reg: Reg(2), offset: 2 },
        /*2*/ orig[2].clone(), // GetGlobal reg3 = global(Done)
        /*3*/ orig[3].clone(), // Ret reg3
        /*4*/ orig[4].clone(), // Call2 reg4 = getBattle(reg0, reg1)
        /*5*/ orig[5].clone(), // NullCheck reg4
        /*6*/ orig[6].clone(), // Field reg6 = reg4.colIndex
        /*7*/ orig[7].clone(), // SafeCast reg7 = cast reg6
        /*8*/ orig[8].clone(), // Call2 reg5 = isColumnUnlocked(reg0, reg7)
        // Non-Linear Mode check: if the flag file exists, skip the game's own adjacency
        // gate entirely (op9-16) and go straight to the per-node marker check (op17+).
        // Otherwise (Linear Mode, the default), fall through to the original adjacency gate.
        /*9*/ String { dst: reg_nl_bytes, ptr: nl_flag_ref },
        /*10*/ Int { dst: reg_nl_len, ptr: nl_len_ref },
        /*11*/ New { dst: reg_nl_str },
        /*12*/ SetField { obj: reg_nl_str, field: field_bytes, src: reg_nl_bytes },
        /*13*/ SetField { obj: reg_nl_str, field: field_length, src: reg_nl_len },
        /*14*/ Call1 { dst: reg_nl_pathbytes, fun: RefFun(get_path_findex), arg0: reg_nl_str },
        /*15*/ Call1 { dst: reg_nl_exists, fun: RefFun(sys_exists_findex), arg0: reg_nl_pathbytes },
        /*16*/ JTrue { cond: reg_nl_exists, offset: 1 }, // -> 18 (skip adjacency gate)
        /*17*/ JFalse { cond: Reg(5), offset: 11 }, // -> 29 (Locked) if the game's own check already says no
        /*18*/ String { dst: reg_prefix_bytes, ptr: prefix_ref },
        /*19*/ Int { dst: reg_len, ptr: len_ref },
        /*20*/ New { dst: reg_prefix_str },
        /*21*/ SetField { obj: reg_prefix_str, field: field_bytes, src: reg_prefix_bytes },
        /*22*/ SetField { obj: reg_prefix_str, field: field_length, src: reg_len },
        /*23*/ Call2 { dst: reg_concat, fun: RefFun(string_add_findex), arg0: reg_prefix_str, arg1: Reg(1) },
        /*24*/ Call1 { dst: reg_pathbytes, fun: RefFun(get_path_findex), arg0: reg_concat },
        /*25*/ Call1 { dst: reg_exists, fun: RefFun(sys_exists_findex), arg0: reg_pathbytes },
        /*26*/ JFalse { cond: reg_exists, offset: 2 }, // -> 29 (Locked) if no marker file
        /*27*/ orig[10].clone(), // Unlocked: GetGlobal reg3 = global(Unlocked)
        /*28*/ orig[11].clone(), // Ret reg3
        /*29*/ orig[12].clone(), // Locked: GetGlobal reg3 = global(Locked)
        /*30*/ orig[13].clone(), // Ret reg3
    ];

    if let Some(debug_info) = &mut f.debug_info {
        let base = orig_debug.unwrap_or_default();
        let get = |i: usize| base.get(i).copied().unwrap_or((0, 0));
        *debug_info = vec![
            get(0), get(1), get(2), get(3), get(4), get(5), get(6), get(7), get(8), get(9),
            get(9), get(9), get(9), get(9), get(9), get(9), get(9), get(9), get(9), get(9),
            get(9), get(9), get(9), get(9), get(9), get(9), get(9),
            get(10), get(11), get(12), get(13),
        ];
    }
    f.assigns = Some(Vec::new());
    Ok(unlocked_global)
}

/// Which of the two marker lifetimes a grant uses. See patch_player_update_grants_resources
/// for what each one actually does differently in the generated bytecode.
#[derive(Clone, Copy, PartialEq, Eq)]
enum GrantKind {
    /// Applied once per battle *instance*, gated by this grant's own `key`-specific sentinel
    /// field (see add_bool_field), and never deletes its marker, since ownership is
    /// permanent. Each Useful kind gets its OWN sentinel, not one shared by all of them,
    /// specifically so that a kind received mid-battle, after some other kind already got
    /// granted this instance, still lands immediately instead of waiting for the next
    /// battle. Confirmed live that this was worth the extra fields: a single shared sentinel
    /// regressed exactly that case.
    Useful,
    /// Applied once per marker, which IS deleted the instant it's granted. NorthgardClient.py
    /// is solely responsible for ever writing another one.
    Filler,
}

/// One grant slot: "if `marker_path` exists, add `amount` of each of `resource_ids` to the
/// human player." See patch_player_update_grants_resources for how these get assembled and how
/// `kind` changes what happens after the grant. Almost always exactly one id (e.g. `["Money"]`);
/// more than one exists so a single grant can target a resource whose *internal* id varies by
/// clan theme (e.g. Lore is called "Faith" for some clans), without needing to detect which
/// clan is actually in play: `ResourcesComponent.addResource` silently no-ops for a kind name
/// that isn't the current clan's own, so listing every known alias here is always safe, not
/// just for the right one.
struct ResourceGrant {
    kind: GrantKind,
    key: &'static str, // matches RESOURCE_CONFIGS.key, used to name this grant's own sentinel field (Useful only)
    marker_path: String,
    resource_ids: &'static [&'static str],
    amount: f64,
}

/// Adds a brand-new field to a class, entirely owned by this patch. No original compiled
/// code ever reads or writes it, so it's completely invisible to (and safe from interfering
/// with) real gameplay, save-file serialization, or hxbit's network-replication schema (all of
/// which only ever touch the specific fields their own macro-generated code was compiled
/// against; they have no way to "notice" a field that didn't exist at compile time). Every
/// fresh instance of the class gets it zero-initialized by the VM's own allocator, exactly
/// like any other field. Confirmed via hlbc's own on-disk format: an object's field list
/// carries no separate size/offset metadata anywhere (`write.rs` serializes a type's fields by
/// just writing `own_fields.len()` then each field in order), so there is nothing else that
/// needs updating to keep a real byte-layout in sync; the HashLink runtime that loads
/// hlboot.dat computes it from this same field list.
///
/// ONLY safe to call on a class with zero subclasses (verified with `patch_northgard
/// subclasses <dir> <class>` before ever using this). Fields are addressed by a single index
/// flattened across the whole inheritance chain (parent's own fields first, then each
/// subclass's own, in order, see hlbc's read.rs), so appending to a class that something
/// else extends would shift every one of that subclass's own field indices by one, silently
/// breaking every compiled reference to them.
fn add_bool_field(code: &mut Bytecode, class_name: &str, field_name: &str, bool_ty: RefType) -> Result<RefField> {
    let type_index = find_type_index_by_name(code, class_name)
        .with_context(|| format!("could not find class {class_name}. Northgard build mismatch?"))?;

    let name_ref = RefString(code.strings.len());
    code.strings.push(field_name.into());
    let new_field = ObjField { name: name_ref, t: bool_ty };

    match &mut code.types[type_index] {
        Type::Obj(obj) => {
            obj.own_fields.push(new_field.clone());
            let new_index = obj.fields.len();
            obj.fields.push(new_field);
            Ok(RefField(new_index))
        }
        _ => bail!("internal error: type_index didn't resolve back to an Obj"),
    }
}

/// Patches `ent.Player.update(dt)`, which runs every simulation tick for every `ent.Player`
/// instance that exists, human and AI alike, for as long as an actual match/battle is loaded,
/// so that the *human* player additionally checks each of `grants`' marker-file paths every
/// tick and, for any that exist, grants the corresponding resource(s). `NorthgardClient.py` is
/// what creates these markers; this tool never touches them itself. The two grant kinds
/// (`GrantKind`) behave differently once granted:
///   - Filler items (e.g. "100 Food"): the marker is deleted the instant it's granted, so it
///     can't be re-applied next tick. NorthgardClient.py deletes it itself if it goes
///     unconsumed too long instead (deliberately session-live, not queued for later).
///   - Useful items (e.g. "50 Starting Food"): the marker is NEVER deleted. It represents
///     permanent ownership, and NorthgardClient.py only ever needs to write it once, the first
///     time the item is received. Instead, applying it exactly once per *battle* (not once
///     ever) is gated by a dedicated sentinel bool this function adds to
///     `ent.player.ResourcesComponent` itself, one per resource key (`apUsefulApplied_<key>`,
///     via add_bool_field), which is `false` on every fresh instance the game constructs: a
///     new battle, or even just quitting to the Conquest map and re-entering the *same* unwon
///     battle (confirmed live that this alone, with no chapter completed, was enough to lose
///     the bonus under the previous NorthgardClient-side-timing design this replaced). This is
///     the reason a game-side sentinel exists at all: NorthgardClient.py has no reliable
///     file-based "a battle just started" signal to gate a marker-rewrite on (a save's
///     `battleFocusId` does get set the instant a battle is chosen, but live-tested against a
///     real save, it's never flushed back to the .sav file at that point, see save_state.py's
///     own comment), so the *only* externally-observable proxy it ever had (Chapter
///     completion) couldn't cover this case. Each Useful kind gets its own independent
///     sentinel rather than one shared by all of them, so that a kind received mid-battle,
///     after some other kind already got granted this instance, still lands on this same
///     check instead of waiting for the next battle.
///
/// **Why `ResourcesComponent.addResource` is called with a capitalized resource-kind string**
/// (e.g. `"Money"`, `"Lore"`), and not the lowercase field name (`"money"`) or the friendlier
/// `Player.addResource` wrapper: money/food/wood each have their own dedicated, checksum-guarded
/// branch inside addResource (a `ctrlMoney`-style obfuscated field, `this.money` XOR'd against a
/// magic constant, that the branch both reads and rewrites), and calling *that* branch from here
/// reliably threw an internal exception, confirmed live and repeatedly, as did a raw field
/// write (silently reverted by that same checksum machinery on the next natural income tick) and
/// `Player.getResource`/`netAddResource`. But addResource also has a second, *generic* dispatch
/// for resource kinds with no dedicated branch (confirmed via real ruin-reward code, which grants
/// "Lore"/"Stone"/"Iron"/etc. through this exact call), and, confirmed live, calling that
/// generic path with a resource kind's real `_Data.ResourceKind` name (capitalized: "Money" not
/// "money") reaches it instead of the crash-prone dedicated branch, for every resource this was
/// tried against (Food/Wood/Money/Stone/Iron/Lore/Faith), with no cap other than food's own real
/// storage-max (which the game enforces correctly on its own).
///
/// Prepended as one self-contained block before update()'s own body, the same technique
/// patch_map_auto_refresh uses and explains: every original jump offset is relative, so
/// inserting whole instructions uniformly before the entire body leaves every original offset
/// valid with no need to touch (or risk miscalculating) any of them. The block does its own
/// fresh `this.aiJob` check up front, reusing the exact field the original op2 already reads
/// rather than a hardcoded field index, so AI-controlled players skip straight past every
/// grant slot without ever running them.
fn patch_player_update_grants_resources(code: &mut Bytecode, grants: &[ResourceGrant]) -> Result<()> {
    let update_findex = find_method_findex(code, "ent.Player", "update")?;
    let get_path_findex = find_method_findex(code, "$Sys", "getPath")?;
    let sys_exists_findex = find_native_findex(code, "std", "sys_exists")?;
    let sys_delete_findex = find_native_findex(code, "std", "sys_delete")?;
    let add_resource_findex = find_method_findex(code, "ent.player.ResourcesComponent", "addResource")?;
    let (res_field, res_ty) = find_field_by_name(code, "ent.Player", "res")?;

    let add_resource_fn = code
        .functions
        .iter()
        .find(|f| f.findex.0 == add_resource_findex)
        .context("could not find ent.player.ResourcesComponent.addResource. Northgard build mismatch?")?;
    if add_resource_fn.regs.len() < 4 {
        bail!(
            "ent.player.ResourcesComponent.addResource's shape doesn't match what this patch was \
             designed against (expected at least 4 regs, got {}). Northgard was likely updated, \
             so re-verify with hlbc before trusting this tool.",
            add_resource_fn.regs.len()
        );
    }
    let string_ty = add_resource_fn.regs[1]; // String (resource id arg)
    let f64_ty = add_resource_fn.regs[2]; // f64 (amount arg)
    let reason_ty = add_resource_fn.regs[3]; // enum<ent.player.ChangeReason>, left null; see below

    // HashLink's `String{}` opcode only ever loads a raw wide-char pointer, not a real boxed
    // `hl.types.String` object. See patch_get_battle_state's docstring/docs/DEVELOPMENT.md.
    // Source the real String class's shape (and a matching raw-bytes/int type) from
    // String.__add__ exactly the same way that patch already does.
    let string_add_findex = find_method_findex(code, "$String", "__add__")?;
    let add_fn = code
        .functions
        .iter()
        .find(|f| f.findex.0 == string_add_findex)
        .context("could not find String.__add__. Northgard build mismatch?")?;
    if add_fn.ops.len() <= 27 || add_fn.regs.len() <= 8 {
        bail!(
            "__add__'s shape doesn't match what this patch was designed against \
             (expected at least 28 ops / 9 regs, got {} ops / {} regs); Northgard was \
             likely updated, re-verify with hlbc before trusting this tool.",
            add_fn.ops.len(),
            add_fn.regs.len()
        );
    }
    let field_length = match &add_fn.ops[7] {
        Opcode::Field { field, .. } => *field,
        other => bail!("__add__ op7 shape changed (expected Field), got {other:?}. Northgard build mismatch?"),
    };
    let field_bytes = match &add_fn.ops[27] {
        Opcode::Field { field, .. } => *field,
        other => bail!("__add__ op27 shape changed (expected Field), got {other:?}. Northgard build mismatch?"),
    };
    let bytes_ty = add_fn.regs[8];
    let int_ty = add_fn.regs[4];
    // Sourced directly from sys_exists's own declared native signature, confirmed via
    // `patch_northgard natsig <dir> std sys_exists` to return bool, rather than guessing at
    // an unrelated function's register (a mistake made once already: some functions' reg5 or
    // reg2 are typed completely differently, e.g. void or an array, not bool).
    let sys_exists_native = code
        .natives
        .iter()
        .find(|n| n.findex.0 == sys_exists_findex)
        .context("could not find sys_exists native. Northgard build mismatch?")?;
    let bool_ty = sys_exists_native.ty(code).ret;

    // One independent sentinel per Useful grant, keyed by its own `key` and not shared across
    // kinds (see GrantKind::Useful's own docstring for why one shared sentinel isn't enough).
    // Verified once, by hand, before this was ever written: `patch_northgard subclasses <dir>
    // ent.player.ResourcesComponent` reports zero. See add_bool_field's own docstring for why
    // that's the load-bearing precondition for this being safe at all, let alone seven times
    // over.
    let mut sentinel_fields: Vec<(&'static str, RefField)> = Vec::new();
    for grant in grants.iter().filter(|g| g.kind == GrantKind::Useful) {
        let field_name = format!("apUsefulApplied_{}", grant.key);
        let field = add_bool_field(code, "ent.player.ResourcesComponent", &field_name, bool_ty)?;
        sentinel_fields.push((grant.key, field));
    }

    // Fetched, validated, and updated in its own scope, separate from the rest of this
    // function, which needs to freely push onto code.strings/code.ints/code.floats while
    // building the prelude below. Rust cannot see those as disjoint from code.functions once
    // a mutable borrow of the latter is threaded through an ordinary function parameter (only
    // direct, same-scope field access gets that treatment), so f's borrow has to end here and
    // get re-acquired afterward, once the whole prelude is ready to write back in one line.
    let (orig, aijob_field, base) = {
        let f = code
            .functions
            .iter_mut()
            .find(|f| f.findex.0 == update_findex)
            .context("could not find ent.Player.update. Northgard build mismatch?")?;

        if f.ops.len() != 10 || f.regs.len() != 5 {
            bail!(
                "ent.Player.update's shape doesn't match what this patch was designed against \
                 (expected 10 ops / 5 regs, got {} ops / {} regs). Northgard was likely \
                 updated, so re-verify with hlbc before trusting this tool.",
                f.ops.len(),
                f.regs.len()
            );
        }
        let orig = f.ops.clone();
        let aijob_field = match &orig[2] {
            Opcode::GetThis { field, .. } => *field,
            other => bail!(
                "update() op2 shape changed (expected GetThis this.aiJob), got {other:?}. \
                 Northgard build mismatch."
            ),
        };
        match &orig[3] {
            Opcode::JNull { .. } => {}
            other => bail!("update() op3 shape changed (expected JNull), got {other:?}. Northgard build mismatch."),
        }
        let aijob_ty = f.regs[3];

        let base = f.regs.len() as u32; // 5
        f.regs.push(aijob_ty); // base+0  r_aijob
        f.regs.push(bytes_ty); // base+1  r_marker_bytes
        f.regs.push(int_ty); // base+2  r_marker_len
        f.regs.push(string_ty); // base+3  r_marker_str
        f.regs.push(bytes_ty); // base+4  r_marker_path (native path bytes from getPath)
        f.regs.push(bool_ty); // base+5  r_exists
        f.regs.push(bytes_ty); // base+6  r_name_bytes
        f.regs.push(int_ty); // base+7  r_name_len
        f.regs.push(string_ty); // base+8  r_name_str
        f.regs.push(f64_ty); // base+9  r_amount
        f.regs.push(reason_ty); // base+10 r_reason (always null)
        f.regs.push(res_ty); // base+11 r_res (this.res)
        f.regs.push(bool_ty); // base+12 r_grant_result
        f.regs.push(bool_ty); // base+13 r_delete_result
        f.regs.push(bool_ty); // base+14 r_sentinel: holds whichever Useful grant's own sentinel is currently being checked
        f.regs.push(bool_ty); // base+15 r_true: constant, written into a sentinel once its grant is applied
        // Every grant slot, and every resource-id alias within one grant, reuses these same
        // registers. Each use's liveness ends before the next one starts, so nothing needs to
        // be preserved across them, and there is no need for a fresh set of registers per
        // grant or alias.

        (orig, aijob_field, base)
    };

    let r_aijob = Reg(base);
    let r_marker_bytes = Reg(base + 1);
    let r_marker_len = Reg(base + 2);
    let r_marker_str = Reg(base + 3);
    let r_marker_path = Reg(base + 4);
    let r_exists = Reg(base + 5);
    let r_name_bytes = Reg(base + 6);
    let r_name_len = Reg(base + 7);
    let r_name_str = Reg(base + 8);
    let r_amount = Reg(base + 9);
    let r_reason = Reg(base + 10);
    let r_res = Reg(base + 11);
    let r_grant_result = Reg(base + 12);
    let r_delete_result = Reg(base + 13);
    let r_sentinel = Reg(base + 14);
    let r_true = Reg(base + 15);

    use Opcode::*;
    const ALIAS_LEN: i32 = 9; // ops per resource-id alias granted within a slot

    // One grant call for one resource-id alias. Builds its already-interned name string and
    // grants it, reusing the shared r_res/scratch registers above. The reason argument is
    // left null, since addResource substitutes its own default in that case, exactly like its
    // own real callers do when they do not care which reason gets recorded.
    let build_alias = |name_ref: RefString, name_len_ref: RefInt, amount_ref: RefFloat| -> Vec<Opcode> {
        vec![
            /*0*/ String { dst: r_name_bytes, ptr: name_ref },
            /*1*/ Int { dst: r_name_len, ptr: name_len_ref },
            /*2*/ New { dst: r_name_str },
            /*3*/ SetField { obj: r_name_str, field: field_bytes, src: r_name_bytes },
            /*4*/ SetField { obj: r_name_str, field: field_length, src: r_name_len },
            /*5*/ Float { dst: r_amount, ptr: amount_ref },
            /*6*/ Null { dst: r_reason },
            /*7*/ NullCheck { reg: r_res },
            /*8*/ Call4 { dst: r_grant_result, fun: RefFun(add_resource_findex), arg0: r_res, arg1: r_name_str, arg2: r_amount, arg3: r_reason },
        ]
    };

    // Appends every resource-id alias for one grant, given that its marker has already been
    // confirmed to exist (the caller is responsible for the marker check and its own
    // conditional skip around this). `code` is taken as an explicit parameter rather than
    // captured, so this closure can be called freely alongside the Useful-grant loop below
    // without a borrow conflict over `code`.
    let push_aliases = |code: &mut Bytecode, prelude: &mut Vec<Opcode>, grant: &ResourceGrant| -> Result<()> {
        for resource_id in grant.resource_ids {
            let name_ref = RefString(code.strings.len());
            code.strings.push((*resource_id).into());
            let name_len_ref = RefInt(code.ints.len());
            code.ints.push(resource_id.len() as i32);
            let amount_ref = RefFloat(code.floats.len());
            code.floats.push(grant.amount);

            let alias = build_alias(name_ref, name_len_ref, amount_ref);
            if alias.len() != ALIAS_LEN as usize {
                bail!(
                    "internal error: a resource-grant alias drifted from its expected \
                     {ALIAS_LEN} ops, so the jump offsets above are no longer valid. Fix \
                     this before applying."
                );
            }
            prelude.extend(alias);
        }
        Ok(())
    };

    // One Filler grant slot: check its marker, and if present, grant every resource-id alias
    // and delete the marker. `code` is an explicit parameter for the same reason as
    // push_aliases above.
    let push_filler_slot = |code: &mut Bytecode, prelude: &mut Vec<Opcode>, grant: &ResourceGrant| -> Result<()> {
        let marker_ref = RefString(code.strings.len());
        code.strings.push(grant.marker_path.as_str().into());
        let marker_len_ref = RefInt(code.ints.len());
        code.ints.push(grant.marker_path.len() as i32); // pure ASCII path, so byte count equals UTF-16 char count

        prelude.extend(vec![
            String { dst: r_marker_bytes, ptr: marker_ref },
            Int { dst: r_marker_len, ptr: marker_len_ref },
            New { dst: r_marker_str },
            SetField { obj: r_marker_str, field: field_bytes, src: r_marker_bytes },
            SetField { obj: r_marker_str, field: field_length, src: r_marker_len },
            Call1 { dst: r_marker_path, fun: RefFun(get_path_findex), arg0: r_marker_str },
            Call1 { dst: r_exists, fun: RefFun(sys_exists_findex), arg0: r_marker_path },
        ]);
        let jfalse_index = prelude.len();
        prelude.push(JFalse { cond: r_exists, offset: 0 }); // patched in below, once this slot's real end is known
        prelude.push(GetThis { dst: r_res, field: res_field });

        push_aliases(code, prelude, grant)?;
        prelude.push(Call1 { dst: r_delete_result, fun: RefFun(sys_delete_findex), arg0: r_marker_path });

        let slot_end = prelude.len();
        prelude[jfalse_index] = JFalse { cond: r_exists, offset: (slot_end - jfalse_index - 1) as i32 };
        Ok(())
    };

    let mut prelude = vec![GetThis { dst: r_aijob, field: aijob_field }];
    let jnotnull_index = prelude.len();
    prelude.push(JNotNull { reg: r_aijob, offset: 0 }); // offset patched in below, once the real end is known

    // Useful grants: each has its own independent sentinel bool on this.res (see
    // sentinel_fields above), so each one is applied once per *battle instance* (a fresh
    // ResourcesComponent, see add_bool_field) on its own schedule, regardless of how many
    // times NorthgardClient.py has written its marker, and regardless of whether it ever
    // detects this particular battle starting at all (it does not need to; see this
    // function's own module-level doc comment). A kind's marker is never deleted here, since
    // ownership is permanent, so the same marker is exactly what should still be sitting
    // there the next time a fresh instance checks it. Giving each kind its own sentinel,
    // instead of one shared by all of them, matters in practice: a kind received mid-battle,
    // after some other kind already got granted this instance, still lands on this same
    // check instead of waiting for the next battle.
    for grant in grants.iter().filter(|g| g.kind == GrantKind::Useful) {
        let (_, sentinel_field) = *sentinel_fields
            .iter()
            .find(|(key, _)| *key == grant.key)
            .context("internal error: a Useful grant's sentinel field was not registered above")?;

        prelude.push(GetThis { dst: r_res, field: res_field });
        prelude.push(Field { dst: r_sentinel, obj: r_res, field: sentinel_field });
        let jtrue_index = prelude.len();
        prelude.push(JTrue { cond: r_sentinel, offset: 0 }); // patched below: skip this slot entirely if already applied

        let marker_ref = RefString(code.strings.len());
        code.strings.push(grant.marker_path.as_str().into());
        let marker_len_ref = RefInt(code.ints.len());
        code.ints.push(grant.marker_path.len() as i32); // pure ASCII path, so byte count equals UTF-16 char count
        prelude.extend(vec![
            String { dst: r_marker_bytes, ptr: marker_ref },
            Int { dst: r_marker_len, ptr: marker_len_ref },
            New { dst: r_marker_str },
            SetField { obj: r_marker_str, field: field_bytes, src: r_marker_bytes },
            SetField { obj: r_marker_str, field: field_length, src: r_marker_len },
            Call1 { dst: r_marker_path, fun: RefFun(get_path_findex), arg0: r_marker_str },
            Call1 { dst: r_exists, fun: RefFun(sys_exists_findex), arg0: r_marker_path },
        ]);
        let jfalse_index = prelude.len();
        prelude.push(JFalse { cond: r_exists, offset: 0 }); // patched below: skip the grant if not owned yet
        prelude.push(GetThis { dst: r_res, field: res_field });

        push_aliases(code, &mut prelude, grant)?;

        // r_res is still valid here (nothing above reassigns it), so this GetThis is a
        // provably redundant re-fetch. Left in place deliberately rather than removed: it
        // executes at most once ever per resource per battle instance (the sentinel above
        // gates re-entry), so the cost is unmeasurable, and this exact op sequence is the
        // one already verified correct via a live disassembly dump. Removing it would need
        // that same verification redone for zero real benefit.
        prelude.push(GetThis { dst: r_res, field: res_field });
        prelude.push(Bool { dst: r_true, value: true });
        prelude.push(SetField { obj: r_res, field: sentinel_field, src: r_true });

        let slot_end = prelude.len();
        prelude[jtrue_index] = JTrue { cond: r_sentinel, offset: (slot_end - jtrue_index - 1) as i32 };
        prelude[jfalse_index] = JFalse { cond: r_exists, offset: (slot_end - jfalse_index - 1) as i32 };
    }

    // Filler grants: unchanged from before. Checked and deleted every tick, independent of
    // any Useful sentinel.
    for grant in grants.iter().filter(|g| g.kind == GrantKind::Filler) {
        push_filler_slot(code, &mut prelude, grant)?;
    }

    let prelude_len_before_body = prelude.len();
    prelude[jnotnull_index] = JNotNull { reg: r_aijob, offset: (prelude_len_before_body - jnotnull_index - 1) as i32 };

    prelude.extend(orig.iter().cloned());

    let f = code
        .functions
        .iter_mut()
        .find(|f| f.findex.0 == update_findex)
        .context("could not find ent.Player.update. Northgard build mismatch?")?;
    f.ops = prelude;

    if let Some(debug_info) = &mut f.debug_info {
        let first = debug_info.first().copied().unwrap_or((0, 0));
        let mut new_debug = vec![first; prelude_len_before_body];
        new_debug.extend(debug_info.iter().copied());
        *debug_info = new_debug;
    }
    f.assigns = Some(Vec::new());

    Ok(())
}

/// Register type of an existing, known-good function's register -- used to source correctly
/// typed registers for brand-new code without needing to hand-construct type descriptors.
fn reg_type(code: &Bytecode, findex: usize, reg: usize, what: &str) -> Result<RefType> {
    code.functions
        .iter()
        .find(|f| f.findex.0 == findex)
        .with_context(|| format!("could not find findex {findex} ({what}) -- Northgard build mismatch?"))?
        .regs
        .get(reg)
        .copied()
        .with_context(|| format!("findex {findex} has no reg{reg} ({what}) -- Northgard build mismatch?"))
}

/// Patches `ConquestMapContent.update(dt)` -- which already runs every frame while the
/// Conquest map screen is open (it's what drives edge-of-screen panning) -- to also walk
/// every node on the map and re-render it if its true state (marker-aware, via the
/// already-patched `Conquest.getBattleState`) no longer matches what's cached in its
/// `MapButton.battleState`. Without this, the map only ever evaluates node state once, at
/// construction time -- newly-unlocked Chapters (or the reverse, a stale Locked node)
/// don't visually update until the player backs out and back into the screen.
///
/// This is deliberately just *prepended* as a self-contained block before all of update()'s
/// existing ops, rather than interleaved with them: every jump inside the original body is a
/// *relative* offset, so inserting whole instructions uniformly before the entire body shifts
/// every jump's source and target by the same amount and leaves every original offset valid,
/// with zero need to touch (or risk miscalculating) any of the original function's own jumps.
/// `MapButton.changeState` is itself a no-op if the state passed in matches what's already
/// cached, so this doesn't need to duplicate that comparison -- it can call it unconditionally
/// for every node, every frame.
fn patch_map_auto_refresh(code: &mut Bytecode, gate_findex: usize) -> Result<()> {
    let update_findex = find_method_findex(code, "ui.menus.conquest.ConquestMapContent", "update")?;
    let change_state_findex = find_method_findex(code, "ui.menus.conquest.MapButton", "changeState")?;
    let has_next_to_unlock_findex =
        find_method_findex(code, "ui.menus.conquest.ConquestMapContent", "hasNextToUnlock")?;
    let get_button_findex = find_method_findex(code, "ui.menus.conquest.MapContainer", "getButton")?;
    let get_button_by_id_findex = find_method_findex(code, "ui.menus.conquest.MapContainer", "getButtonById")?;

    let conquest_ty = reg_type(code, has_next_to_unlock_findex, 3, "gamesys.conquest.Conquest")?;
    let map_container_ty = reg_type(code, has_next_to_unlock_findex, 14, "ui.menus.conquest.MapContainer")?;
    let battle_state_ty = reg_type(code, has_next_to_unlock_findex, 15, "enum<BattleState>")?;
    let array_obj_ty = reg_type(code, get_button_findex, 4, "hl.types.ArrayObj")?;
    let dynamic_ty = reg_type(code, get_button_findex, 8, "dynamic")?;
    let raw_array_ty = reg_type(code, get_button_findex, 9, "array")?;
    let map_button_ty = reg_type(code, get_button_findex, 11, "ui.menus.conquest.MapButton")?;
    let battle_ty = reg_type(code, get_button_by_id_findex, 14, "gamesys.conquest.Battle")?;
    let battle_data_ty = reg_type(code, get_button_by_id_findex, 13, "Battle.data virtual")?;
    let string_ty = reg_type(code, get_button_by_id_findex, 1, "String")?;
    let callback_ty = reg_type(code, change_state_findex, 2, "() -> void callback")?;

    let f = code
        .functions
        .iter_mut()
        .find(|f| f.findex.0 == update_findex)
        .context("could not find ConquestMapContent.update -- Northgard build mismatch?")?;

    let i32_ty = *f
        .regs
        .get(10)
        .context("update()'s reg10 isn't present -- Northgard build mismatch?")?;
    let void_reg = Reg(3); // update()'s own existing void-typed scratch register, reused throughout its body

    let base = f.regs.len() as u32;
    f.regs.push(conquest_ty); // base+0  rConquest
    f.regs.push(map_container_ty); // base+1  rContainer
    f.regs.push(array_obj_ty); // base+2  rColumns (outer: MapContainer.buttons)
    f.regs.push(i32_ty); // base+3  rI (outer loop index)
    f.regs.push(i32_ty); // base+4  rLen1
    f.regs.push(array_obj_ty); // base+5  rColumn (inner array for this column)
    f.regs.push(dynamic_ty); // base+6  rDyn (GetArray scratch, reused both levels)
    f.regs.push(raw_array_ty); // base+7  rRaw1
    f.regs.push(map_button_ty); // base+8  rButton
    f.regs.push(i32_ty); // base+9  rJ (inner loop index)
    f.regs.push(i32_ty); // base+10 rLen2
    f.regs.push(raw_array_ty); // base+11 rRaw2
    f.regs.push(battle_ty); // base+12 rBattle
    f.regs.push(battle_data_ty); // base+13 rBattleData
    f.regs.push(string_ty); // base+14 rInfId
    f.regs.push(battle_state_ty); // base+15 rNewState
    f.regs.push(callback_ty); // base+16 rNullCb

    let r_conquest = Reg(base);
    let r_container = Reg(base + 1);
    let r_columns = Reg(base + 2);
    let r_i = Reg(base + 3);
    let r_len1 = Reg(base + 4);
    let r_column = Reg(base + 5);
    let r_dyn = Reg(base + 6);
    let r_raw1 = Reg(base + 7);
    let r_button = Reg(base + 8);
    let r_j = Reg(base + 9);
    let r_len2 = Reg(base + 10);
    let r_raw2 = Reg(base + 11);
    let r_battle = Reg(base + 12);
    let r_battle_data = Reg(base + 13);
    let r_infid = Reg(base + 14);
    let r_new_state = Reg(base + 15);
    let r_null_cb = Reg(base + 16);

    let field_conquest = RefField(FIELD_CMC_CONQUEST);
    let field_container = RefField(FIELD_CMC_CONTAINER);
    let field_buttons = RefField(FIELD_MAPCONTAINER_BUTTONS);
    let field_len = RefField(FIELD_ARRAYOBJ_LENGTH);
    let field_arr = RefField(FIELD_ARRAYOBJ_ARRAY);
    let field_button_data = RefField(FIELD_MAPBUTTON_DATA);
    let field_battle_data = RefField(FIELD_BATTLE_DATA);
    let field_infid = RefField(FIELD_BATTLEDATA_INFID);

    use Opcode::*;
    // Offsets are `target = source_index + 1 + offset` (confirmed against this same
    // bytecode's own existing jumps, e.g. MapContainer.getButton). All targets below are
    // local indices into this prelude alone -- OUTER_END (40) is exactly where the
    // original function's ops get appended right after, unchanged.
    let mut prelude = vec![
        /*0*/ GetThis { dst: r_conquest, field: field_conquest },
        /*1*/ GetThis { dst: r_container, field: field_container },
        /*2*/ NullCheck { reg: r_container },
        /*3*/ Field { dst: r_columns, obj: r_container, field: field_buttons },
        /*4*/ Int { dst: r_i, ptr: RefInt(0) },
        /*5*/ Label, // OUTER_LABEL
        /*6*/ NullCheck { reg: r_columns },
        /*7*/ Field { dst: r_len1, obj: r_columns, field: field_len },
        /*8*/ JSGte { a: r_i, b: r_len1, offset: 31 }, // -> 40 (OUTER_END)
        /*9*/ Field { dst: r_len1, obj: r_columns, field: field_len },
        /*10*/ JULt { a: r_i, b: r_len1, offset: 2 }, // -> 13
        /*11*/ Null { dst: r_column },
        /*12*/ JAlways { offset: 3 }, // -> 16
        /*13*/ Field { dst: r_raw1, obj: r_columns, field: field_arr },
        /*14*/ GetArray { dst: r_dyn, array: r_raw1, index: r_i },
        /*15*/ SafeCast { dst: r_column, src: r_dyn },
        /*16*/ Incr { dst: r_i },
        /*17*/ Int { dst: r_j, ptr: RefInt(0) },
        /*18*/ Label, // INNER_LABEL
        /*19*/ NullCheck { reg: r_column },
        /*20*/ Field { dst: r_len2, obj: r_column, field: field_len },
        /*21*/ JSGte { a: r_j, b: r_len2, offset: -17 }, // -> 5 (OUTER_LABEL)
        /*22*/ Field { dst: r_len2, obj: r_column, field: field_len },
        /*23*/ JULt { a: r_j, b: r_len2, offset: 2 }, // -> 26
        /*24*/ Null { dst: r_button },
        /*25*/ JAlways { offset: 3 }, // -> 29
        /*26*/ Field { dst: r_raw2, obj: r_column, field: field_arr },
        /*27*/ GetArray { dst: r_dyn, array: r_raw2, index: r_j },
        /*28*/ UnsafeCast { dst: r_button, src: r_dyn },
        /*29*/ Incr { dst: r_j },
        /*30*/ NullCheck { reg: r_button },
        /*31*/ Field { dst: r_battle, obj: r_button, field: field_button_data },
        /*32*/ NullCheck { reg: r_battle },
        /*33*/ Field { dst: r_battle_data, obj: r_battle, field: field_battle_data },
        /*34*/ NullCheck { reg: r_battle_data },
        /*35*/ Field { dst: r_infid, obj: r_battle_data, field: field_infid },
        /*36*/ Call2 { dst: r_new_state, fun: RefFun(gate_findex), arg0: r_conquest, arg1: r_infid },
        /*37*/ Null { dst: r_null_cb },
        /*38*/ Call3 { dst: void_reg, fun: RefFun(change_state_findex), arg0: r_button, arg1: r_new_state, arg2: r_null_cb },
        /*39*/ JAlways { offset: -22 }, // -> 18 (INNER_LABEL)
    ];
    let prelude_len = prelude.len();
    if prelude_len != 40 {
        bail!("internal error: map-refresh prelude drifted from its expected 40 ops -- jump offsets above are no longer valid, fix before applying");
    }

    prelude.extend(f.ops.iter().cloned());
    f.ops = prelude;

    if let Some(debug_info) = &mut f.debug_info {
        let first = debug_info.first().copied().unwrap_or((0, 0));
        let mut new_debug = vec![first; prelude_len];
        new_debug.extend(debug_info.iter().copied());
        *debug_info = new_debug;
    }
    f.assigns = Some(Vec::new());

    Ok(())
}

/// Fixes the post-battle "reveal" animation showing a not-actually-unlocked sibling node as
/// selectable: `ConquestMapContent.animateNewBattlePlots` loops over every node in the newly
/// reachable column and unconditionally sets each one's cached state to `Unlocked` (a
/// hardcoded `GetGlobal` of the enum's `Unlocked` constructor) -- vanilla-correct when the
/// only gate was adjacency (which just became true for the whole column), but wrong once
/// Archipelago also requires a per-node item marker. At the point that hardcoded load sits,
/// the loop already has the live `Conquest` instance (reg6) and this node's `infId` (reg4)
/// in registers -- exactly `getBattleState`'s two arguments -- so this replaces that one
/// opcode with a real call, in place, needing no new registers and no jump-offset changes.
fn patch_reveal_uses_real_state(code: &mut Bytecode, gate_findex: usize, unlocked_global: usize) -> Result<()> {
    let animate_new_battle_plots_findex =
        find_method_findex(code, "ui.menus.conquest.ConquestMapContent", "animateNewBattlePlots")?;

    let f = code
        .functions
        .iter_mut()
        .find(|f| f.findex.0 == animate_new_battle_plots_findex)
        .context("could not find ConquestMapContent.animateNewBattlePlots -- Northgard build mismatch?")?;

    let op = f
        .ops
        .get_mut(ANIMATE_NEW_BATTLE_PLOTS_HARDCODED_UNLOCKED_OP)
        .context("animateNewBattlePlots is shorter than expected -- Northgard build mismatch?")?;
    match op {
        Opcode::GetGlobal { dst, global } if global.0 == unlocked_global => {
            let dst = *dst;
            *op = Opcode::Call2 { dst, fun: RefFun(gate_findex), arg0: Reg(6), arg1: Reg(4) };
        }
        other => bail!(
            "animateNewBattlePlots op{ANIMATE_NEW_BATTLE_PLOTS_HARDCODED_UNLOCKED_OP} doesn't \
             match the expected hardcoded GetGlobal(Unlocked) (got {other:?}) -- Northgard build \
             mismatch? Re-verify with `dump` before trusting this patch."
        ),
    }

    Ok(())
}

/// Fixes Northgard's own `Conquest.onBattleCompleted` silently dropping a battle win from the
/// save's `path` ledger -- the sole source of truth `NorthgardClient.py`/`save_state.py` use
/// to know a Chapter is done -- whenever another node in the same tree row/column already has
/// an entry there. Vanilla Northgard only ever lets you win ONE of a row's two sibling
/// battles (winning either one locks out the other), so this guard is a no-op there in
/// practice; Non-Linear Mode (this project's own `non_linear_mode.flag`, see
/// patch_get_battle_state) is what first makes BOTH siblings winnable, which is exactly when
/// this pre-existing bug surfaces: the second sibling won in a row never lands in `path` even
/// though the game shows it as won, regardless of how it was won -- confirmed on a real save
/// where winning "MuspellWrath" then "KeepSimple" (same tree row) left `path` with
/// MuspellWrath but not KeepSimple, while `finishedBattleId` (a separate field, unaffected by
/// this bug) correctly named KeepSimple as the most recently finished battle.
///
/// The fix replaces the single conditional jump (`JTrue` on `isColumnInPath(colIndex)`) that
/// skips the `path.push(id)` call with an unconditional fallthrough (`JAlways{offset: 0}`,
/// i.e. never skip) -- one opcode, no new registers, no other jump offsets affected. The
/// `isColumnInPath` call itself is left in place (now simply unused) rather than removed, to
/// avoid touching anything else in the function's register/jump layout.
fn patch_path_dedup_by_column(code: &mut Bytecode) -> Result<()> {
    let battle_completed_findex = find_method_findex(code, "gamesys.conquest.Conquest", "onBattleCompleted")?;
    let f = code
        .functions
        .iter_mut()
        .find(|f| f.findex.0 == battle_completed_findex)
        .context("could not find Conquest.onBattleCompleted -- Northgard build mismatch?")?;

    let op = f
        .ops
        .get_mut(BATTLE_COMPLETED_SKIP_PATH_PUSH_OP)
        .context("onBattleCompleted is shorter than expected -- Northgard build mismatch?")?;
    match op {
        Opcode::JTrue { offset, .. } if *offset == 7 => {
            *op = Opcode::JAlways { offset: 0 };
        }
        other => bail!(
            "onBattleCompleted op{BATTLE_COMPLETED_SKIP_PATH_PUSH_OP} doesn't match the \
             expected JTrue(isColumnInPath, +7) (got {other:?}) -- Northgard build mismatch? \
             Re-verify with `dump` before trusting this patch."
        ),
    }
    Ok(())
}

/// Fixes the actual root cause of the Conquest map permanently locking (no node selectable,
/// Escape included) after a battle completed out of vanilla's assumed column order: confirmed
/// via direct instrumentation, not static analysis alone. `animateLastPath` drops its `onDone`
/// callback entirely when `this.lastButtonIndex` is null, jumping straight to `Ret` instead of
/// ever invoking it. That callback is what eventually calls `unlockUI` -- so when this path is
/// taken, the Conquest map never unlocks. The fix appends two new ops after the function's
/// existing body (`CallClosure(onDone)` then `Ret`) and retargets the existing `JNull` to land
/// there instead of the original `Ret` -- the callback still runs even though `buildPath`'s own
/// animation is skipped, so the "unlock everything once this finishes" contract is preserved
/// either way.
fn patch_animate_last_path_invokes_callback_when_skipped(code: &mut Bytecode) -> Result<()> {
    let animate_last_path_findex =
        find_method_findex(code, "ui.menus.conquest.ConquestMapContent", "animateLastPath")?;
    let f = code
        .functions
        .iter_mut()
        .find(|f| f.findex.0 == animate_last_path_findex)
        .context("could not find ConquestMapContent.animateLastPath -- Northgard build mismatch?")?;

    if f.ops.len() != 19 {
        bail!(
            "animateLastPath's shape doesn't match what this patch was designed against \
             (expected 19 ops, got {}) -- Northgard was likely updated; re-verify with hlbc \
             before trusting this tool.",
            f.ops.len()
        );
    }

    use Opcode::*;
    let void_reg = Reg(2); // this function's own existing void-typed scratch register
    let r_callback = Reg(1); // the onDone: () -> void parameter

    let op = f
        .ops
        .get_mut(ANIMATE_LAST_PATH_NULL_JUMP_OP)
        .context("animateLastPath is shorter than expected -- Northgard build mismatch?")?;
    match op {
        Opcode::JNull { reg, offset } if *offset == 16 => {
            let reg = *reg;
            *op = Opcode::JNull { reg, offset: 17 }; // -> 19 (new CallClosure below), instead of -> 18 (Ret)
        }
        other => bail!(
            "animateLastPath op{ANIMATE_LAST_PATH_NULL_JUMP_OP} doesn't match the expected \
             JNull(lastButtonIndex, +16) (got {other:?}) -- Northgard build mismatch? Re-verify \
             with `dump` before trusting this patch."
        ),
    }

    f.ops.push(CallClosure { dst: void_reg, fun: r_callback, args: vec![] });
    f.ops.push(Ret { ret: void_reg });

    if let Some(debug_info) = &mut f.debug_info {
        let last = debug_info.last().copied().unwrap_or((0, 0));
        debug_info.push(last);
        debug_info.push(last);
    }
    f.assigns = Some(Vec::new());

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::write_atomically;
    use std::fs;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;

    fn unique_test_dir(name: &str) -> std::path::PathBuf {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "patch_northgard_test_{name}_{}_{n}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn write_atomically_basic_roundtrip() {
        let dir = unique_test_dir("basic");
        let target = dir.join("hlboot.dat");
        write_atomically(&target, b"hello world").unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"hello world");

        // overwriting an existing target must also work (this is the common case: apply
        // replacing an already-patched live file)
        write_atomically(&target, b"a different, longer payload").unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"a different, longer payload");

        // no leftover temp file
        let leftovers: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp_"))
            .collect();
        assert!(leftovers.is_empty(), "leftover temp file(s): {leftovers:?}");

        let _ = fs::remove_dir_all(&dir);
    }

    /// The actual concern this whole change exists for: two Launcher/client windows both
    /// auto-healing the same real hlboot.dat at once, fully uncoordinated on the Python
    /// side. Simulates that with many threads racing write_atomically against the SAME
    /// destination path, each with a distinct, easily-identifiable full payload, repeated
    /// over many rounds to make sure the destination is never a torn/mixed file -- always
    /// exactly one full writer's content, whichever rename won.
    #[test]
    fn write_atomically_concurrent_never_tears() {
        let dir = unique_test_dir("concurrent");
        let target = dir.join("hlboot.dat");
        let target = Arc::new(target);

        const THREADS: usize = 16;
        const ROUNDS: usize = 25;
        const PAYLOAD_LEN: usize = 200_000; // big enough that a naive truncate+buffered-write race would very likely interleave

        for round in 0..ROUNDS {
            let mut handles = Vec::new();
            for t in 0..THREADS {
                let target = Arc::clone(&target);
                // distinctive, self-describing payload: every byte is this thread+round's
                // own marker byte, so any interleaving from two different writers would
                // show up as mixed byte values instead of one uniform value throughout.
                let marker = (round * THREADS + t) as u8;
                let payload = vec![marker; PAYLOAD_LEN];
                handles.push(thread::spawn(move || {
                    write_atomically(&target, &payload).expect("write_atomically failed under concurrency");
                    marker
                }));
            }
            let markers: Vec<u8> = handles.into_iter().map(|h| h.join().unwrap()).collect();

            let final_bytes = fs::read(&*target).expect("destination missing after concurrent writes");
            assert_eq!(final_bytes.len(), PAYLOAD_LEN, "round {round}: final file size doesn't match any single writer's payload -- looks torn");
            let final_marker = final_bytes[0];
            assert!(
                final_bytes.iter().all(|&b| b == final_marker),
                "round {round}: final file contains mixed bytes -- torn/interleaved write, not an atomic replace"
            );
            assert!(
                markers.contains(&final_marker),
                "round {round}: final content (marker {final_marker}) doesn't match any of this round's writers {markers:?}"
            );

            let leftovers: Vec<_> = fs::read_dir(&*dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_name().to_string_lossy().contains(".tmp_"))
                .collect();
            assert!(leftovers.is_empty(), "round {round}: leftover temp file(s) after all writers finished: {leftovers:?}");
        }

        let _ = fs::remove_dir_all(&*dir);
    }
}
