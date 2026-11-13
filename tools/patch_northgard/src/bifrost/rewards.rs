//! Objective reward offers (portal perk screens, Wyvern / Beacon / Pillar of Gods one-card
//! rewards). Vanilla rolls them when the objective is created, from the perks the HOST doesn't
//! own, and falls back to one hardcoded perk ("BResourcesBonus") when a pool is empty. Each
//! realm's portal pool holds only 4 perks, and bonus items hand those same perks out, so in
//! co-op (or after enough items) cards came up greyed for someone, or as duplicates, and the
//! screen could never be confirmed. With Bifrost mode active these patches make every offer
//! card one that every human player can take:
//!   - patch_reward_filter: a perk is offered only if every human player can use it and none
//!     owns it yet.
//!   - patch_reward_fallback: a pool that runs dry is topped up with another random perk from
//!     the game's whole compatible list instead of the hardcoded fallback.
//!   - patch_reward_reroll: the offer is rolled again when the reward is handed out (host only),
//!     so perks gained during the map (bonus items, wave picks) are accounted for.
//!   - patch_take_all_owned: one-card "take all" rewards skip a perk a player already owns
//!     instead of applying it twice.
//! The last-resort safety net for pick screens (an owned card can still be picked and gives
//! nothing) lives in items.rs (patch_ap_card_selectable and patch_perk_pick).

use anyhow::{bail, Context, Result};
use hlbc::opcodes::Opcode;
use hlbc::types::{Reg, RefField, RefFun};
use hlbc::Bytecode;

use super::{
    emit_require_active, function, install, int_konst, konst, obj_type, path_in, splice, Assembler, Env, Regs,
    Replacement, Scratch, ACTIVE_FLAG_NAME,
};
use crate::{find_field_by_name, find_method_findex};

const INIT_REWARDS_OPS: usize = 780;

fn init_rewards(code: &Bytecode) -> Result<(usize, Vec<Opcode>, Vec<hlbc::types::RefType>)> {
    let findex = find_method_findex(code, "gamesys.bifrost.BifrostObjective", "initRewards")?;
    let f = function(code, findex, "BifrostObjective.initRewards")?;
    if f.ops.len() != INIT_REWARDS_OPS || f.regs.len() != 72 {
        bail!("initRewards shape changed (expected {INIT_REWARDS_OPS} ops / 72 regs, got {} / {})", f.ops.len(), f.regs.len());
    }
    Ok((findex, f.ops.clone(), f.regs.clone()))
}

/// The filter closure initRewards passes to getCompatibleConquestBonusFor (its op 37, an
/// anonymous `function(_) return true`). isConquestBonusCompatibleFor calls it last, after its
/// own checks against the host. Its body becomes: when Bifrost mode is active, return false if
/// any human player (aiJob == null) already owns the perk or can't use it
/// (isConquestBonusCompatibleFor for that player, with no filter so it can't recurse).
pub fn patch_reward_filter(code: &mut Bytecode, env: &Env) -> Result<()> {
    let (_, o, _) = init_rewards(code)?;
    let filter = match &o[37] {
        Opcode::StaticClosure { fun, .. } => fun.0,
        other => bail!("initRewards op37 changed (expected StaticClosure), got {other:?}"),
    };
    let fn_compat = find_method_findex(code, "gamesys.bifrost.BifrostBonusManager", "isConquestBonusCompatibleFor")?;
    let fn_get_compat_list = find_method_findex(code, "gamesys.bifrost.BifrostBonusManager", "getCompatibleConquestBonusFor")?;
    let fn_get_bifrost = find_method_findex(code, "Game", "get_bifrost")?;
    let (field_bonus, bm_ty) = find_field_by_name(code, "gamesys.bifrost.BifrostManager", "bonus")?;
    let (field_aijob, aijob_ty) = find_field_by_name(code, "ent.Player", "aiJob")?;
    let bf_ty = obj_type(code, "gamesys.bifrost.BifrostManager")?;

    // Game.inst, state.players and the ArrayProxy unwrapping, read off getCompatibleConquestBonusFor
    // (ops 6-16) and pickObjective (ops 21-22).
    let gl = function(code, fn_get_compat_list, "getCompatibleConquestBonusFor")?;
    let (field_state, field_players, field_proxy_arr) = match (&gl.ops[8], &gl.ops[10], &gl.ops[15], &gl.ops[16]) {
        (Opcode::Field { field: a, .. }, Opcode::Field { field: b, .. }, Opcode::Field { field: c, .. }, Opcode::SafeCast { .. }) => (*a, *b, *c),
        other => bail!("getCompatibleConquestBonusFor ops 8-16 changed: {other:?}"),
    };
    let gr = gl.regs.clone();
    let (state_ty, proxy_ty, arr_ty, arrdyn_ty, player_ty, rawarr_ty, filter_fn_ty) =
        (gr[10], gr[9], gr[12], gr[13], gr[17], gr[19], gr[2]);
    let po = function(code, find_method_findex(code, "$HBifrost", "pickObjective")?, "pickObjective")?;
    let (global_game, field_inst, game_global_ty, inst_ty) = match (&po.ops[21], &po.ops[22]) {
        (Opcode::GetGlobal { global, .. }, Opcode::Field { field, .. }) => (*global, *field, po.regs[5], po.regs[4]),
        _ => bail!("pickObjective ops 21-22 changed"),
    };

    let f = function(code, filter, "initRewards filter closure")?;
    let shape_ok = f.regs.len() == 2
        && matches!(f.ops.as_slice(), [Opcode::Bool { value: true, .. }, Opcode::Ret { .. }]);
    if !shape_ok {
        bail!("initRewards filter closure changed (expected `return true`). Northgard build mismatch?");
    }
    let active = konst(code, &path_in(&env.bifrost_dir, ACTIVE_FLAG_NAME));
    let zero = int_konst(code, 0);

    let mut regs = Regs::new(2);
    let sc = Scratch::new(env, &mut regs);
    let r_tmp = regs.alloc(env.string_ty);
    let r_gg = regs.alloc(game_global_ty);
    let r_inst = regs.alloc(inst_ty);
    let r_bf = regs.alloc(bf_ty);
    let r_bm = regs.alloc(bm_ty);
    let r_state = regs.alloc(state_ty);
    let r_proxy = regs.alloc(proxy_ty);
    let r_arrdyn = regs.alloc(arrdyn_ty);
    let r_arr = regs.alloc(arr_ty);
    let r_raw = regs.alloc(rawarr_ty);
    let r_dyn = regs.alloc(env.dyn_ty);
    let r_p = regs.alloc(player_ty);
    let r_ai = regs.alloc(aijob_ty);
    let r_i = regs.alloc(env.i32_ty);
    let r_n = regs.alloc(env.i32_ty);
    let r_ok = regs.alloc(env.bool_ty);
    let r_nofilter = regs.alloc(filter_fn_ty);
    let (bonus, ret) = (Reg(0), Reg(1));

    use Opcode::*;
    let mut a = Assembler::new();
    let yes = a.label();
    let no = a.label();
    emit_require_active(&mut a, env, &sc, active, r_tmp, yes);
    a.op(GetGlobal { dst: r_gg, global: global_game });
    a.op(Field { dst: r_inst, obj: r_gg, field: field_inst });
    a.jump(JNull { reg: r_inst, offset: 0 }, yes);
    a.op(Call1 { dst: r_bf, fun: RefFun(fn_get_bifrost), arg0: r_inst });
    a.jump(JNull { reg: r_bf, offset: 0 }, yes);
    a.op(Field { dst: r_bm, obj: r_bf, field: field_bonus });
    a.jump(JNull { reg: r_bm, offset: 0 }, yes);
    a.op(Field { dst: r_state, obj: r_inst, field: field_state });
    a.jump(JNull { reg: r_state, offset: 0 }, yes);
    a.op(Field { dst: r_proxy, obj: r_state, field: field_players });
    a.jump(JNull { reg: r_proxy, offset: 0 }, yes);
    a.op(Field { dst: r_arrdyn, obj: r_proxy, field: field_proxy_arr });
    a.op(SafeCast { dst: r_arr, src: r_arrdyn });
    a.jump(JNull { reg: r_arr, offset: 0 }, yes);
    a.op(Null { dst: r_nofilter });
    a.op(Int { dst: r_i, ptr: zero });
    let lp = a.label();
    let next = a.label();
    a.bind_loop(lp);
    a.op(Field { dst: r_n, obj: r_arr, field: RefField(0) });
    a.jump(JSGte { a: r_i, b: r_n, offset: 0 }, yes);
    a.op(Field { dst: r_raw, obj: r_arr, field: RefField(1) });
    a.op(GetArray { dst: r_dyn, array: r_raw, index: r_i });
    a.op(UnsafeCast { dst: r_p, src: r_dyn });
    a.op(Incr { dst: r_i });
    a.jump(JNull { reg: r_p, offset: 0 }, next);
    a.op(Field { dst: r_ai, obj: r_p, field: field_aijob });
    a.jump(JNotNull { reg: r_ai, offset: 0 }, next);
    a.op(CallN { dst: r_ok, fun: RefFun(fn_compat), args: vec![r_bm, bonus, r_p, r_nofilter] });
    a.jump(JFalse { cond: r_ok, offset: 0 }, no);
    a.bind(next);
    a.jump(JAlways { offset: 0 }, lp);
    a.bind(no);
    a.op(Bool { dst: ret, value: false });
    a.op(Ret { ret });
    a.bind(yes);
    a.op(Bool { dst: ret, value: true });
    a.op(Ret { ret });
    let ops = a.finish()?;
    install(code, filter, ops, regs.new, |_| 0)
}

/// initRewards' byTypes branch, op 735 onward: the "no candidate left" case (op 734 found the
/// pick missing from the compatible pool) logs an error and offers "BResourcesBonus", whoever
/// owns it. When Bifrost mode is active and the compatible pool (reg8, already filtered by
/// patch_reward_filter) still has perks, offer a random one of those instead, through the same
/// add-to-rewards closure (reg24, which also removes it from the pool), and continue at op 760
/// (next card). An empty pool keeps the vanilla fallback.
pub fn patch_reward_fallback(code: &mut Bytecode, env: &Env) -> Result<()> {
    let (findex, o, r) = init_rewards(code)?;
    use Opcode::*;
    let fn_pick = match &o[699] {
        Call2 { fun, .. } => fun.0,
        other => bail!("initRewards op699 changed (expected pickRandom), got {other:?}"),
    };
    let shape_ok = matches!(o[734], JNotEq { .. })
        && matches!(o[735], GetGlobal { .. })
        && matches!(&o[752], CallClosure { fun, .. } if *fun == Reg(24))
        && matches!(o[759], CallClosure { .. })
        && matches!(o[760], JAlways { .. });
    if !shape_ok {
        bail!("initRewards ops 734-760 changed. Northgard build mismatch?");
    }
    let active = konst(code, &path_in(&env.bifrost_dir, ACTIVE_FLAG_NAME));
    let zero = int_konst(code, 0);

    let mut regs = Regs::new(r.len());
    let sc = Scratch::new(env, &mut regs);
    let r_tmp = regs.alloc(env.string_ty);
    let r_n = regs.alloc(env.i32_ty);
    let r_zero = regs.alloc(env.i32_ty);
    let r_arrdyn = regs.alloc(r[13]);
    let r_rand = regs.alloc(r[54]);
    let r_dyn = regs.alloc(env.dyn_ty);
    let r_v = regs.alloc(r[42]);
    let r_void = regs.alloc(env.void_ty);
    let (pool, add) = (Reg(8), Reg(24));

    let mut a = Assembler::new();
    let vanilla = a.label();
    emit_require_active(&mut a, env, &sc, active, r_tmp, vanilla);
    a.jump(JNull { reg: pool, offset: 0 }, vanilla);
    a.jump(JNull { reg: add, offset: 0 }, vanilla);
    a.op(Field { dst: r_n, obj: pool, field: RefField(0) });
    a.op(Int { dst: r_zero, ptr: zero });
    a.jump(JSLte { a: r_n, b: r_zero, offset: 0 }, vanilla);
    a.op(SafeCast { dst: r_arrdyn, src: pool });
    a.op(Null { dst: r_rand });
    a.op(Call2 { dst: r_dyn, fun: RefFun(fn_pick), arg0: r_arrdyn, arg1: r_rand });
    a.op(ToVirtual { dst: r_v, src: r_dyn });
    a.jump(JNull { reg: r_v, offset: 0 }, vanilla);
    a.op(CallClosure { dst: r_void, fun: add, args: vec![r_v] });
    a.jump_original(JAlways { offset: 0 }, 760);
    a.bind(vanilla);
    let (mut block, external) = a.finish_with_externals()?;
    block.push(o[735].clone());
    let (ops, old_of_new) = splice(&o, vec![Replacement { at: 735, block, external }])?;
    install(code, findex, ops, regs.new, |i| old_of_new[i])
}

/// `BifrostObjective.giveReward` (host only, see set_status): when Bifrost mode is active and
/// the objective has rewards, empty them and roll them again with initRewards before they are
/// handed out, so the offer reflects what every player owns right now.
pub fn patch_reward_reroll(code: &mut Bytecode, env: &Env) -> Result<()> {
    let findex = find_method_findex(code, "gamesys.bifrost.BifrostObjective", "giveReward")?;
    let fn_init = find_method_findex(code, "gamesys.bifrost.BifrostObjective", "initRewards")?;
    let (field_rewards, rewards_ty) = find_field_by_name(code, "gamesys.bifrost.BifrostObjective", "rewards")?;
    let f = function(code, findex, "BifrostObjective.giveReward")?;
    let o = f.ops.clone();
    let nregs = f.regs.len();
    if !matches!(o.get(5), Some(Opcode::GetThis { field, .. }) if *field == field_rewards) {
        bail!("giveReward op5 changed (expected this.rewards). Northgard build mismatch?");
    }
    let active = konst(code, &path_in(&env.bifrost_dir, ACTIVE_FLAG_NAME));
    let zero = int_konst(code, 0);

    let mut regs = Regs::new(nregs);
    let sc = Scratch::new(env, &mut regs);
    let r_tmp = regs.alloc(env.string_ty);
    let r_rw = regs.alloc(rewards_ty);
    let r_n = regs.alloc(env.i32_ty);
    let r_zero = regs.alloc(env.i32_ty);
    let r_void = regs.alloc(env.void_ty);

    use Opcode::*;
    let mut a = Assembler::new();
    let original = a.label();
    emit_require_active(&mut a, env, &sc, active, r_tmp, original);
    a.op(GetThis { dst: r_rw, field: field_rewards });
    a.jump(JNull { reg: r_rw, offset: 0 }, original);
    a.op(Field { dst: r_n, obj: r_rw, field: RefField(0) });
    a.op(Int { dst: r_zero, ptr: zero });
    a.jump(JSLte { a: r_n, b: r_zero, offset: 0 }, original);
    a.op(SetField { obj: r_rw, field: RefField(0), src: r_zero });
    a.op(Call1 { dst: r_void, fun: RefFun(fn_init), arg0: Reg(0) });
    let n = a.len();
    a.bind_external(original, n);
    let mut ops = a.finish()?;
    ops.extend(o);
    install(code, findex, ops, regs.new, |i| i.saturating_sub(n)).context("giveReward")
}

/// `GameState.giveObjectiveReward` (host only): the one-card "take all" rewards (Wyvern, Beacon,
/// Pillar of Gods) hand each perk to every player with netAddAndApplyBifrostBonus (op 66), which
/// never checks ownership. When Bifrost mode is active, a player who already owns the perk is
/// skipped (continue at op 71), the same "nothing" outcome patch_perk_pick gives on pick screens.
/// patch_reward_filter keeps owned perks out of offers, so this only matters when a perk arrives
/// while the screen is open.
pub fn patch_take_all_owned(code: &mut Bytecode, env: &Env) -> Result<()> {
    let findex = find_method_findex(code, "GameState", "giveObjectiveReward")?;
    let fn_apply = find_method_findex(code, "ent.Player", "netAddAndApplyBifrostBonus")?;
    let fn_has = find_method_findex(code, "gamesys.bifrost.BifrostBonusManager", "playerHasBonus")?;
    let fn_get_bifrost = find_method_findex(code, "Game", "get_bifrost")?;
    let (field_bonus, bm_ty) = find_field_by_name(code, "gamesys.bifrost.BifrostManager", "bonus")?;
    let bf_ty = obj_type(code, "gamesys.bifrost.BifrostManager")?;
    let f = function(code, findex, "GameState.giveObjectiveReward")?;
    let o = f.ops.clone();
    let r = f.regs.clone();
    use Opcode::*;
    let field_game = match &o[3] {
        GetThis { dst, field } if *dst == Reg(4) => *field,
        other => bail!("giveObjectiveReward op3 changed (expected this.game), got {other:?}"),
    };
    let shape_ok = matches!(&o[66], Call2 { fun, arg0, arg1, .. } if fun.0 == fn_apply && *arg0 == Reg(14) && *arg1 == Reg(19))
        && matches!(o[71], JAlways { .. });
    if !shape_ok {
        bail!("giveObjectiveReward ops 66-71 changed. Northgard build mismatch?");
    }
    let active = konst(code, &path_in(&env.bifrost_dir, ACTIVE_FLAG_NAME));

    let mut regs = Regs::new(r.len());
    let sc = Scratch::new(env, &mut regs);
    let r_tmp = regs.alloc(env.string_ty);
    let r_game = regs.alloc(r[4]);
    let r_bf = regs.alloc(bf_ty);
    let r_bm = regs.alloc(bm_ty);
    let r_has = regs.alloc(env.bool_ty);
    let (player, bonus) = (Reg(14), Reg(19));

    let mut a = Assembler::new();
    let apply = a.label();
    emit_require_active(&mut a, env, &sc, active, r_tmp, apply);
    a.jump(JNull { reg: bonus, offset: 0 }, apply);
    a.op(GetThis { dst: r_game, field: field_game });
    a.jump(JNull { reg: r_game, offset: 0 }, apply);
    a.op(Call1 { dst: r_bf, fun: RefFun(fn_get_bifrost), arg0: r_game });
    a.jump(JNull { reg: r_bf, offset: 0 }, apply);
    a.op(Field { dst: r_bm, obj: r_bf, field: field_bonus });
    a.jump(JNull { reg: r_bm, offset: 0 }, apply);
    a.op(Call3 { dst: r_has, fun: RefFun(fn_has), arg0: r_bm, arg1: bonus, arg2: player });
    a.jump(JFalse { cond: r_has, offset: 0 }, apply);
    a.jump_original(JAlways { offset: 0 }, 71);
    a.bind(apply);
    let (mut block, external) = a.finish_with_externals()?;
    block.push(o[66].clone());
    let (ops, old_of_new) = splice(&o, vec![Replacement { at: 66, block, external }])?;
    install(code, findex, ops, regs.new, |i| old_of_new[i])
}
