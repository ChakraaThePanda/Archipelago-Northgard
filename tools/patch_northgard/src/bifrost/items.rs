//! Bifrost perk-pick locations and category bonus items. See the parent module for the
//! marker-file protocol; this file adds:
//!   reads  bifrost\card_<Realm>_<Screen>_<K>   that perk card position is an unchecked location
//!   reads  bifrost\bonus_<Key>_<N>             the slot owns at least N copies of that bonus item
//!   writes bifrost\done\card_<Realm>_<Screen>_<K>  an AP card was picked

use anyhow::{bail, Context, Result};
use hlbc::opcodes::Opcode;
use hlbc::types::{Reg, RefEnumConstruct, RefField, RefFun, RefType, Type};
use hlbc::{Bytecode, Resolve};

use super::{
    add_field, emit_concat, emit_exists, emit_require_active, emit_str, emit_write_marker, find_prim_type, function,
    install, int_konst, konst, obj_type, path_in, splice, Assembler, Env, Konst, Lbl, Regs, Replacement, Scratch,
    ACTIVE_FLAG_NAME,
};
use crate::{find_field_by_name, find_method_findex, RefInt};

fn find_method_any(code: &Bytecode, candidates: &[(&str, &str)]) -> Result<usize> {
    for (class, method) in candidates {
        if let Ok(f) = find_method_findex(code, class, method) {
            return Ok(f);
        }
    }
    bail!("could not find any of {candidates:?}. Northgard build mismatch?")
}

/// A virtual type's field index, by name.
fn virtual_field(code: &Bytecode, ty: RefType, name: &str) -> Result<RefField> {
    match code.get(ty) {
        Type::Virtual { fields } => fields
            .iter()
            .position(|f| f.name(code) == name)
            .map(RefField)
            .with_context(|| format!("virtual has no field {name}")),
        _ => bail!("internal error: expected a virtual type"),
    }
}

/// Registers the card-key helpers use (allocated once per patched function).
struct CardKeyRegs {
    kind: Reg,
    i: Reg,
    dyn_: Reg,
    tmp: Reg,
    c0: Reg,
    c1: Reg,
}

impl CardKeyRegs {
    fn new(env: &Env, regs: &mut Regs) -> Self {
        CardKeyRegs {
            kind: regs.alloc(env.i32_ty),
            i: regs.alloc(env.i32_ty),
            dyn_: regs.alloc(env.dyn_ty),
            tmp: regs.alloc(env.string_ty),
            c0: regs.alloc(env.i32_ty),
            c1: regs.alloc(env.i32_ty),
        }
    }
}

/// Constants the card-key helpers use.
struct CardKeyConsts {
    wave: Konst,
    underscore: Konst,
    zero: RefInt,
    one: RefInt,
    fn_std_string: usize,
}

impl CardKeyConsts {
    fn new(code: &mut Bytecode) -> Result<Self> {
        Ok(CardKeyConsts {
            wave: konst(code, "Wave"),
            underscore: konst(code, "_"),
            zero: int_konst(code, 0),
            one: int_konst(code, 1),
            fn_std_string: find_method_any(code, &[("$Std", "string"), ("Std", "string")])?,
        })
    }
}

/// `out` = the perk screen's name for a BonusSourceType value: Wave(i) -> "Wave<i+1>",
/// Objective(kind) -> kind. Jumps to `none` for any other source (Start screens) or a null kind.
/// Shared by the pick patch and the card-marking patch, which must always build the same key.
fn emit_screen_name(a: &mut Assembler, env: &Env, sc: &Scratch, r: &CardKeyRegs, k: &CardKeyConsts, source: Reg, out: Reg, none: Lbl) {
    use Opcode::*;
    let objective = a.label();
    let done = a.label();
    a.op(Int { dst: r.c0, ptr: k.zero });
    a.op(Int { dst: r.c1, ptr: k.one });
    a.op(EnumIndex { dst: r.kind, value: source });
    a.jump(JEq { a: r.kind, b: r.c1, offset: 0 }, objective);
    a.jump(JNotEq { a: r.kind, b: r.c0, offset: 0 }, none);
    a.op(EnumField { dst: r.i, value: source, construct: RefEnumConstruct(0), field: RefField(0) });
    a.op(Add { dst: r.i, a: r.i, b: r.c1 });
    a.op(ToDyn { dst: r.dyn_, src: r.i });
    a.op(Call1 { dst: r.tmp, fun: RefFun(k.fn_std_string), arg0: r.dyn_ });
    emit_str(a, env, sc, out, k.wave);
    emit_concat(a, env, out, out, r.tmp);
    a.jump(JAlways { offset: 0 }, done);
    a.bind(objective);
    a.op(EnumField { dst: out, value: source, construct: RefEnumConstruct(1), field: RefField(0) });
    a.jump(JNull { reg: out, offset: 0 }, none);
    a.bind(done);
}

/// `out` = "<realm>_<screen>_<position + 1>", the card key in `card_`, `scout_`, `seen_` and done
/// markers (Bifrost.py's location markers use the same format). `position` is 0-based and is
/// left unchanged.
fn emit_card_key(a: &mut Assembler, env: &Env, sc: &Scratch, r: &CardKeyRegs, k: &CardKeyConsts, realm: Reg, screen: Reg, position: Reg, out: Reg) {
    use Opcode::*;
    emit_str(a, env, sc, r.tmp, k.underscore);
    emit_concat(a, env, out, realm, r.tmp);
    emit_concat(a, env, out, out, screen);
    emit_concat(a, env, out, out, r.tmp);
    a.op(Int { dst: r.c1, ptr: k.one });
    a.op(Add { dst: r.i, a: position, b: r.c1 });
    a.op(ToDyn { dst: r.dyn_, src: r.i });
    a.op(Call1 { dst: r.tmp, fun: RefFun(k.fn_std_string), arg0: r.dyn_ });
    emit_concat(a, env, out, out, r.tmp);
}

/// `BifrostBonusManager.registerBonusPick`: perk picks as locations, decided on the host. Every
/// co-op player runs the client on the same slot, so the host's markers are everyone's.
///   - op 274, the per-player apply branch (SelectionsPerPlayer / OnePerPlayer): reg25 still
///     holds the picked card's index there. When that position is an unchecked location
///     (`card_<Realm>_<Screen>_<K>`), write `done\card_<...>` and jump to op 333 ("next slot"),
///     skipping only the apply calls; every bit of selection bookkeeping still runs. Skipping
///     the tech branch too keeps ongoingTechReplacements empty, so onBonusSelectionDone fires.
///   - op 459, the TakeAll branch (one-card objective rewards): same check with K = 1, jumping
///     to op 471 to skip giveObjectiveReward.
///
/// Screen names: see emit_screen_name (Start picks are never locations). Positions count across
/// the whole screen (solo 6 wave cards, co-op 3 per player laid out player by player, portals 4
/// shared), so K = index + 1 for every screen and every player count.
pub fn patch_perk_pick(code: &mut Bytecode, env: &Env) -> Result<()> {
    let findex = find_method_findex(code, "gamesys.bifrost.BifrostBonusManager", "registerBonusPick")?;
    let f = function(code, findex, "BifrostBonusManager.registerBonusPick")?;
    if f.ops.len() != 558 || f.regs.len() != 58 {
        bail!("registerBonusPick shape changed (expected 558 ops / 58 regs, got {} / {})", f.ops.len(), f.regs.len());
    }
    let o = f.ops.clone();
    let nregs = f.regs.len();
    use Opcode::*;
    let shape_ok = matches!(o[274], NullCheck { reg } if reg == Reg(41))
        && matches!(o[459], Call1 { .. })
        && matches!(o[470], Call2 { .. })
        && matches!(o[471], Call1 { .. })
        && matches!(o[278], EnumField { dst, .. } if dst == Reg(44))
        && matches!(o[279], NullCheck { reg } if reg == Reg(36))
        && matches!(o[280], Call2 { arg0, arg1, .. } if arg0 == Reg(36) && arg1 == Reg(44));
    if !shape_ok {
        bail!("registerBonusPick ops 274-280/459/470/471 changed. Northgard build mismatch?");
    }
    let fn_has = find_method_findex(code, "gamesys.bifrost.BifrostBonusManager", "playerHasBonus")?;
    let fn_get_bifrost = find_method_findex(code, "gamesys.bifrost.BifrostBonusManager", "get_bifrost")?;
    let bf_ty = obj_type(code, "gamesys.bifrost.BifrostManager")?;
    let (field_realm, _) = find_field_by_name(code, "gamesys.bifrost.BifrostManager", "realm")?;
    let keys = CardKeyConsts::new(code)?;
    let active = konst(code, &path_in(&env.bifrost_dir, ACTIVE_FLAG_NAME));
    let card_prefix = konst(code, &path_in(&env.bifrost_dir, "card_"));
    let done_prefix = konst(code, &path_in(&env.done_dir, "card_"));
    let done_dir = konst(code, &env.done_dir);
    let empty = konst(code, "");

    let mut regs = Regs::new(nregs);
    let sc = Scratch::new(env, &mut regs);
    let kr = CardKeyRegs::new(env, &mut regs);
    let r_bf = regs.alloc(bf_ty);
    let r_realm = regs.alloc(env.string_ty);
    let r_screen = regs.alloc(env.string_ty);
    let r_k = regs.alloc(env.i32_ty);
    let r_key = regs.alloc(env.string_ty);
    let r_path = regs.alloc(env.string_ty);
    let r_tmp = regs.alloc(env.string_ty);
    let r_has = regs.alloc(env.bool_ty);

    // If this pick is an AP card: write its done marker and jump to original op `skip_to`.
    // `index` is the card index register; None means position 0 (the TakeAll branch).
    let build = |index: Option<Reg>, skip_to: usize| -> Result<(Vec<Opcode>, Vec<(usize, usize)>)> {
        let mut a = Assembler::new();
        let not_ap = a.label();
        emit_require_active(&mut a, env, &sc, active, r_tmp, not_ap);
        a.op(Call1 { dst: r_bf, fun: RefFun(fn_get_bifrost), arg0: Reg(0) });
        a.jump(JNull { reg: r_bf, offset: 0 }, not_ap);
        a.op(Field { dst: r_realm, obj: r_bf, field: field_realm });
        a.jump(JNull { reg: r_realm, offset: 0 }, not_ap);
        a.jump(JNull { reg: Reg(4), offset: 0 }, not_ap);
        emit_screen_name(&mut a, env, &sc, &kr, &keys, Reg(4), r_screen, not_ap);
        match index {
            Some(idx) => a.op(Mov { dst: r_k, src: idx }),
            None => a.op(Int { dst: r_k, ptr: keys.zero }),
        }
        emit_card_key(&mut a, env, &sc, &kr, &keys, r_realm, r_screen, r_k, r_key);
        emit_str(&mut a, env, &sc, r_tmp, card_prefix);
        emit_concat(&mut a, env, r_path, r_tmp, r_key);
        emit_exists(&mut a, env, sc.boolean, r_path);
        a.jump(JFalse { cond: sc.boolean, offset: 0 }, not_ap);
        emit_str(&mut a, env, &sc, r_tmp, done_prefix);
        emit_concat(&mut a, env, r_path, r_tmp, r_key);
        emit_write_marker(&mut a, env, &sc, done_dir, empty, r_path, r_tmp);
        a.jump_original(JAlways { offset: 0 }, skip_to);
        a.bind(not_ap);
        a.finish_with_externals()
    };

    let (mut block_a, ext_a) = build(Some(Reg(25)), 333)?;
    block_a.push(o[274].clone());
    let (mut block_b, ext_b) = build(None, 471)?;
    block_b.push(o[459].clone());
    // Safety net: a perk the picking player (reg36) already owns (reg44, see patch_reward_filter
    // for why this should be rare) is picked as "nothing": skip its apply call, op 280.
    let (mut block_c, ext_c) = {
        let mut a = Assembler::new();
        let apply = a.label();
        emit_require_active(&mut a, env, &sc, active, r_tmp, apply);
        a.jump(JNull { reg: Reg(36), offset: 0 }, apply);
        a.jump(JNull { reg: Reg(44), offset: 0 }, apply);
        a.op(Call3 { dst: r_has, fun: RefFun(fn_has), arg0: Reg(0), arg1: Reg(44), arg2: Reg(36) });
        a.jump(JFalse { cond: r_has, offset: 0 }, apply);
        a.jump_original(JAlways { offset: 0 }, 333);
        a.bind(apply);
        a.finish_with_externals()?
    };
    block_c.push(o[279].clone());
    let (ops, old_of_new) = splice(
        &o,
        vec![
            Replacement { at: 274, block: block_a, external: ext_a },
            Replacement { at: 279, block: block_c, external: ext_c },
            Replacement { at: 459, block: block_b, external: ext_b },
        ],
    )?;
    install(code, findex, ops, regs.new, |i| old_of_new[i])
}

/// The category tables, mirrored by Bifrost.py's BONUS_CATEGORIES (same keys and caps). Every
/// copy is a random pick among what the player doesn't have yet:
///   Fixed: from the listed ids (they need no unit/resource/building choice).
///   Pool: from the game's own clan-compatible bonus list (getCompatibleConquestBonusFor, which
///         fills in each variant's unit/resource/building), entries whose id is listed.
///   Building: from the game's clan-compatible Bifrost building list (getCompatibleBuildingsFor),
///         unlocked for the run like a picked building card (addBuildingToBifrostPool).
enum BonusSource {
    Fixed(&'static [&'static str]),
    Pool(&'static [&'static str]),
    Building,
}

struct BonusCategory {
    key: &'static str,
    cap: usize,
    source: BonusSource,
}

// Ids per the conquestBonus sheet's own groups; entries the game never offers in Bifrost
// (bifrostSelectionWeight 0) are left out.
const BONUS_CATEGORIES: &[BonusCategory] = &[
    BonusCategory {
        key: "Military",
        cap: 6,
        source: BonusSource::Pool(&[
            "BWarband", "BMilitaryAttack", "BMilitaryDefense", "BWarchief", "BRecruitMilitary",
            "BWarchiefVsUndead", "BWarchiefCooldown", "BFreeMilitaryExtension",
        ]),
    },
    BonusCategory {
        key: "Economy",
        cap: 8,
        source: BonusSource::Pool(&[
            "BResBonus", "BHappyBonus", "BWinter", "BMineral", "BJobProd", "BPopGrowth", "BSilo",
            "BUpgradeCivilianBuilding", "BUpgradeMilitaryBuilding", "BFreeSlotCivilian", "BFreeSlotMilitary",
        ]),
    },
    BonusCategory {
        key: "Special",
        cap: 3,
        source: BonusSource::Pool(&["BFoesAttack", "BRuinsExploration", "BHousePopulation", "BMoreSheeps", "BForgeRelics"]),
    },
    BonusCategory {
        key: "Clan",
        cap: 3,
        source: BonusSource::Pool(&[
            "BSkalds", "BSiloImproved", "BStagFame2", "BGoatSheeps", "BBarricades", "BFeast",
            "BFoodAnimals", "BBerserkerBetter", "BMilitaryHappy", "BHomeland", "BColonizeKrowns", "BMercenaries",
            "BKaija", "BShieldBearerUpgraded", "BRelicBear", "BMender", "BAltar", "BBuildingUpgrade",
            "BSkirmisher", "BSigny", "BScorchedEarth", "BDragonmen", "BSacrifice", "BSlaves",
            "BWarchiefRegen", "BWarchiefProd", "BGreatTower", "BNorn", "BSpectralWarrior", "BCoastalHurt",
            "BRecycling", "BHeartyLife", "BOxShadow", "BHomeDeer", "BArchery", "BWilderHunt",
            "BMultipleMeal", "BCook", "BSuperMeal", "BDeathDisease", "BOverwork", "BMoreHousing",
            "BOffensiveCivilian", "BFarColonize", "BGlacialWinds", "BHildegardHeal", "BMilitaryChurch", "BUnlimitedRelics",
            "BDuchyHouse", "BRebelion", "BFreeSentries", "BLeggedBranch", "BNoNornCap", "BThreeWarchief",
            "BOver9000", "BMilitarySpoils", "BYourTotemAnimal", "BEverythingCheaper", "BLandlords", "BFinalDeploy",
            "BStrongerBraziers", "BVarangianAreKey", "BCatapultFiesta",
        ]),
    },
    BonusCategory {
        key: "Objective",
        cap: 8,
        source: BonusSource::Fixed(&[
            "BFame", "BUnitFree", "BTowerMultiTarget", "BCaCTowers", "BMinerals", "BResourcesBonus",
            "BMilitaryBonus", "BMultipleTowers", "BLore", "BPopulation", "BTowerMultiShot", "BExplodingTowers",
        ]),
    },
    BonusCategory { key: "Muspelheim", cap: 4, source: BonusSource::Fixed(&["BMuspelheimDoTUnits", "BMuspelheimAoEUnits", "BMuspelheimTowers", "BMuspelheimBarricades"]) },
    BonusCategory { key: "Niflheim", cap: 4, source: BonusSource::Fixed(&["BNiflheimUnit", "BNiflheimVampirism", "BNiflheimTowers", "BNiflheimBarricades"]) },
    BonusCategory { key: "Idavoll", cap: 4, source: BonusSource::Fixed(&["BIdavollThunderUnit", "BIdavollBlindAbility", "BIdavollThunderTower", "BIdavollThunderBarricades"]) },
    BonusCategory { key: "Nidavellir", cap: 4, source: BonusSource::Fixed(&["BNidavellirFearUnit", "BNidavellirDoTUnit", "BNidavellirTowers", "BNidavellirBarricades"]) },
    BonusCategory { key: "Vanaheim", cap: 4, source: BonusSource::Fixed(&["BVanaheimRootUnit", "BVanaheimConfusionUnit", "BVanaheimTowers", "BVanaheimBarricades"]) },
    BonusCategory { key: "Building", cap: 6, source: BonusSource::Building },
];

/// `ent.Player.update` (already prepended by the resource-grant patch): grants received
/// category bonus items to every human player, recorded in the run save so they show in the
/// game's Bonuses tab like picked perks.
///
/// The client writes `bonus_<Key>_<n>` for n = 1..copies received. When the host grants copy n
/// to a player in a run, it also writes `granted\<runSeed>_<playerUID>_<Key>_<n>`, the "AP gave
/// this already" record. A copy is granted when its bonus marker exists and its granted record
/// doesn't, so:
///   - later realms of the same run don't grant it again (the run save already re-applies it
///     through onPlayerStartGame);
///   - a new run (new seed) grants everything again;
///   - real perks the player picks never count against their item copies.
///
/// The run save only reaches disk when a realm is won (onBifrostWon raises save.step, then
/// saves), so a grant made during a realm that is then lost, retried or reloaded is gone from
/// the save while its record stays. Records therefore hold
/// "<step>|<id>|<unitId>|<buildingId>|<resId>|<isAdvanced>" (see write_bonus_record): an older
/// step means the grant was saved; the current step means it may not have been, so the exact
/// same bonus is granted again unless the player has one with that id. Records from patch v17
/// and older (just the id) are checked by ownership, and a lost pool copy is re-rolled.
///
/// Each (category, copy) also gets a bool sentinel on ent.Player (fresh every realm) caching
/// "handled this realm", so after a realm's first tick there are no file checks for it at all,
/// and a category stops at its first missing bonus marker.
///
/// Granted from the host only (Game.host.isAuth) through `Player.netAddAndApplyBifrostBonus`,
/// the same network call vanilla perk picks use: its implementation runs on EVERY machine and
/// records the bonus in that machine's copy of the run save (so every co-op player's Bonuses tab
/// shows it), while the actual apply happens on the host only and replicates through hxbit. This
/// mirrors what made co-op resource grants work: nothing here deletes a marker, and every
/// player has their own sentinel and their own granted record (keyed by their uid).
pub fn patch_bonus_items(code: &mut Bytecode, env: &Env) -> Result<()> {
    let update = find_method_findex(code, "ent.Player", "update")?;
    let fn_apply = find_method_findex(code, "ent.Player", "netAddAndApplyBifrostBonus")?;
    let fn_has = find_method_findex(code, "ent.Player", "hasBonus")?;
    let fn_compat = find_method_findex(code, "gamesys.bifrost.BifrostBonusManager", "getCompatibleConquestBonusFor")?;
    let fn_get_bifrost = find_method_findex(code, "Game", "get_bifrost")?;
    let fn_uid = find_method_findex(code, "$HBifrost", "getPlayerUID")?;
    let fn_std_string = find_method_any(code, &[("$Std", "string"), ("Std", "string")])?;
    let fn_std_random = find_method_any(code, &[("$Std", "random"), ("Std", "random")])?;
    let bonus_ty = function(code, fn_apply, "netAddAndApplyBifrostBonus")?.regs[1];
    let compat = function(code, fn_compat, "getCompatibleConquestBonusFor")?;
    let closure_ty = compat.regs[2];
    let array_obj_ty = compat.regs[4];
    // The game's own "accept everything" bonus filter, (bonus) -> true, the closure
    // BifrostObjective.initRewards passes to getCompatibleConquestBonusFor.
    let always_true = code
        .functions
        .iter()
        .find(|f| {
            f.t == closure_ty
                && f.ops.len() == 2
                && matches!(f.ops[0], Opcode::Bool { value: true, .. })
                && matches!(f.ops[1], Opcode::Ret { .. })
        })
        .map(|f| f.findex.0)
        .context("could not find the always-true bonus filter closure. Northgard build mismatch?")?;
    let null_bool_ty = find_prim_type(code, "null<bool>", |t| matches!(t, hlbc::types::Type::Null(r) if *r == env.bool_ty))?;
    let (field_aijob, aijob_ty) = find_field_by_name(code, "ent.Player", "aiJob")?;
    let (field_game, game_ty) = find_field_by_name(code, "ent.Player", "game")?;
    let (field_host, host_ty) = find_field_by_name(code, "Game", "host")?;
    let (field_is_auth, _) = find_field_by_name(code, "hxbit.NetworkHost", "isAuth")?;
    let (field_bonus_mgr, bonus_mgr_ty) = find_field_by_name(code, "gamesys.bifrost.BifrostManager", "bonus")?;
    let (field_save, save_ty) = find_field_by_name(code, "gamesys.bifrost.BifrostManager", "save")?;
    let field_seed = virtual_field(code, save_ty, "seed")?;
    // The run's key: startSeed is fixed for a whole run, while seed changes every realm.
    let field_start_seed = virtual_field(code, save_ty, "startSeed")?;
    let start_seed_ty = match code.get(save_ty) {
        hlbc::types::Type::Virtual { fields } => fields[field_start_seed.0].t,
        _ => bail!("internal error: save is not a virtual"),
    };
    let fn_string_compare = crate::find_native_findex(code, "std", "string_compare")?;
    let fn_get_content = env.fn_get_content;
    let fn_buildings = find_method_findex(code, "gamesys.bifrost.BifrostBonusManager", "getCompatibleBuildingsFor")?;
    let fn_add_building = find_method_findex(code, "ent.Player", "addBuildingToBifrostPool")?;
    let fn_building_allowed = find_method_findex(code, "gamesys.bifrost.BifrostManager", "isBuildingAllowedForAny")?;
    let building_ty = function(code, fn_buildings, "getCompatibleBuildingsFor")?.regs[19];
    let field_building_id = virtual_field(code, building_ty, "id")?;
    // building.props.bifrostSelectionWeight: vanilla rolls buildings by this weight, so 0 or
    // missing means "never offered in Bifrost" (e.g. LureSacred, Lynx only). Only weight > 0 count.
    let field_building_props = virtual_field(code, building_ty, "props")?;
    let building_props_ty = match code.get(building_ty) {
        hlbc::types::Type::Virtual { fields } => fields[field_building_props.0].t,
        _ => bail!("internal error: building data is not a virtual"),
    };
    let field_building_weight = virtual_field(code, building_props_ty, "bifrostSelectionWeight")?;
    let building_weight_ty = match code.get(building_props_ty) {
        hlbc::types::Type::Virtual { fields } => fields[field_building_weight.0].t,
        _ => bail!("internal error: building props is not a virtual"),
    };
    let fn_allowed = find_method_findex(code, "$HConquestBonus", "bonusIsAllowed")
        .or_else(|_| find_method_any(code, &[("HConquestBonus", "bonusIsAllowed")]))?;
    let (field_clan, clan_ty) = find_field_by_name(code, "ent.Player", "clan")?;
    let bf_ty = obj_type(code, "gamesys.bifrost.BifrostManager")?;
    let field_id = virtual_field(code, bonus_ty, "id")?;
    let field_adv = virtual_field(code, bonus_ty, "isAdvanced")?;
    let field_unit = virtual_field(code, bonus_ty, "unitId")?;
    let field_bld = virtual_field(code, bonus_ty, "buildingId")?;
    let field_res = virtual_field(code, bonus_ty, "resId")?;
    // save.step: the run's realm counter, raised (then saved) only when a realm is won.
    let field_step = virtual_field(code, save_ty, "step")?;
    let fn_split = find_string_split(code, env)?;
    let parts_ty = match code.get(code.functions.iter().find(|f| f.findex.0 == fn_split).unwrap().t) {
        hlbc::types::Type::Fun(tf) => tf.ret,
        _ => bail!("internal error: String.split is not a function"),
    };
    // The player's own bonus list, read the way Player.getBonus does (it is the only reliable
    // way to walk it): this.bonuses (an hxbit array proxy) -> .array (ArrayDyn) -> ArrayObj of
    // PlayerBonus, each with .data (the bonus virtual).
    let fn_get_bonus = find_method_findex(code, "ent.Player", "getBonus")?;
    let gb = function(code, fn_get_bonus, "Player.getBonus")?;
    if gb.ops.len() != 38 || gb.regs.len() != 19 {
        bail!("Player.getBonus shape changed (expected 38 ops / 19 regs, got {} / {})", gb.ops.len(), gb.regs.len());
    }
    let field_bonuses = match &gb.ops[8] {
        Opcode::GetThis { field, .. } => *field,
        other => bail!("Player.getBonus op8 changed (expected GetThis), got {other:?}"),
    };
    let field_proxy_array = match &gb.ops[13] {
        Opcode::Field { field, .. } => *field,
        other => bail!("Player.getBonus op13 changed (expected Field), got {other:?}"),
    };
    let field_pb_data = match &gb.ops[30] {
        Opcode::Field { field, .. } => *field,
        other => bail!("Player.getBonus op30 changed (expected Field), got {other:?}"),
    };
    let (proxy_ty, owned_arr_ty, owned_dyn_ty, pb_ty, pdata_ty, pv_ty) =
        (gb.regs[7], gb.regs[8], gb.regs[9], gb.regs[13], gb.regs[17], gb.regs[18]);
    let field_pv_id = virtual_field(code, pv_ty, "id")?;

    struct Slot {
        sentinel: RefField,
        marker: Konst,
        suffix: Konst, // "_<Key>_<n>", appended to "<seed>_<uid>" for the granted record
    }
    let mut slots: Vec<Vec<Slot>> = Vec::new();
    for cat in BONUS_CATEGORIES {
        let mut v = Vec::new();
        for n in 1..=cat.cap {
            let sentinel = add_field(code, "ent.Player", &format!("apBonus_{}_{n}", cat.key), env.bool_ty)?;
            let marker = konst(code, &path_in(&env.bifrost_dir, &format!("bonus_{}_{n}", cat.key)));
            let suffix = konst(code, &format!("_{}_{n}", cat.key));
            v.push(Slot { sentinel, marker, suffix });
        }
        slots.push(v);
    }
    let fallback_ids: Vec<Vec<Konst>> = BONUS_CATEGORIES
        .iter()
        .map(|c| fallback_ids_for(c.key).iter().map(|id| konst(code, id)).collect())
        .collect();
    let ids: Vec<Vec<Konst>> = BONUS_CATEGORIES
        .iter()
        .map(|c| match &c.source {
            BonusSource::Fixed(l) | BonusSource::Pool(l) => l.iter().map(|id| konst(code, id)).collect(),
            BonusSource::Building => Vec::new(),
        })
        .collect();
    let granted_dir_path = path_in(&env.bifrost_dir, "granted");
    let active = konst(code, &path_in(&env.bifrost_dir, ACTIVE_FLAG_NAME));
    let granted_dir = konst(code, &granted_dir_path);
    let granted_prefix = konst(code, &path_in(&granted_dir_path, ""));
    let underscore = konst(code, "_");
    let bar = konst(code, "|");
    let empty = konst(code, "");
    let flag_true = konst(code, "1");
    let flag_false = konst(code, "0");
    let zero = int_konst(code, 0);
    let one = int_konst(code, 1);
    let part_idx: Vec<RefInt> = (0..RECORD_PARTS).map(|i| int_konst(code, i as i32)).collect();
    let parts_count = int_konst(code, RECORD_PARTS as i32);
    let f = function(code, update, "ent.Player.update")?;
    let o = f.ops.clone();
    let nregs = f.regs.len();
    let mut regs = Regs::new(nregs);
    let sc = Scratch::new(env, &mut regs);
    let r_aijob = regs.alloc(aijob_ty);
    let r_game = regs.alloc(game_ty);
    let r_host = regs.alloc(host_ty);
    let r_auth = regs.alloc(env.bool_ty);
    let r_bf = regs.alloc(bf_ty);
    let r_bm = regs.alloc(bonus_mgr_ty);
    let r_save = regs.alloc(save_ty);
    let r_seed = regs.alloc(env.i32_ty);
    let r_uid = regs.alloc(env.string_ty);
    let r_base = regs.alloc(env.string_ty); // "<granted dir>\<seed>_<uid>"
    let r_record = regs.alloc(env.string_ty);
    let r_sent = regs.alloc(env.bool_ty);
    let r_true = regs.alloc(env.bool_ty);
    let r_false = regs.alloc(env.bool_ty);
    let r_nb = regs.alloc(null_bool_ty);
    let r_null = regs.alloc(env.string_ty);
    let r_id = regs.alloc(env.string_ty);
    let r_v = regs.alloc(bonus_ty);
    let r_clos = regs.alloc(closure_ty);
    let r_list = regs.alloc(array_obj_ty);
    let r_i = regs.alloc(env.i32_ty);
    let r_len = regs.alloc(env.i32_ty);
    let r_zero = regs.alloc(env.i32_ty);
    let r_cmp = regs.alloc(env.i32_ty);
    let r_raw = regs.alloc(env.array_ty);
    let r_dyn = regs.alloc(env.dyn_ty);
    let r_vid = regs.alloc(env.string_ty);
    let r_tmp = regs.alloc(env.string_ty);
    let r_count = regs.alloc(env.i32_ty);
    let r_roll = regs.alloc(env.i32_ty);
    let r_one = regs.alloc(env.i32_ty);
    let r_start = regs.alloc(start_seed_ty);
    let r_la = regs.alloc(env.i32_ty);
    let r_lb = regs.alloc(env.i32_ty);
    let r_ba = regs.alloc(env.bytes_ty);
    let r_bb = regs.alloc(env.bytes_ty);
    let r_bld = regs.alloc(building_ty);
    let r_bprops = regs.alloc(building_props_ty);
    let r_bweight_n = regs.alloc(building_weight_ty);
    let r_bweight = regs.alloc(env.i32_ty);
    let r_clan = regs.alloc(clan_ty);
    let r_nb_true = regs.alloc(null_bool_ty);
    let r_step = regs.alloc(env.string_ty); // save.step as a string: the current realm's stamp
    let r_step_i = regs.alloc(env.i32_ty);
    let r_content = regs.alloc(env.string_ty);
    let r_bar = regs.alloc(env.string_ty);
    let r_parts = regs.alloc(parts_ty);
    let r_part = regs.alloc(env.string_ty);
    let r_k = regs.alloc(env.i32_ty);
    let r_adv_b = regs.alloc(env.bool_ty);
    let r_proxy = regs.alloc(proxy_ty);
    let r_owned = regs.alloc(owned_arr_ty);
    let r_owned_dyn = regs.alloc(owned_dyn_ty);
    let r_pb = regs.alloc(pb_ty);
    let r_pdata = regs.alloc(pdata_ty);
    let r_pv = regs.alloc(pv_ty);
    let r_pid = regs.alloc(env.string_ty);
    let r_j = regs.alloc(env.i32_ty);
    let r_jlen = regs.alloc(env.i32_ty);
    let r_jraw = regs.alloc(env.array_ty);
    let r_jdyn = regs.alloc(env.dyn_ty);
    let r_adv_n = regs.alloc(null_bool_ty);

    use Opcode::*;
    // Jumps to `differ` unless strings x and y are equal (a null on either side differs).
    let str_eq = |a: &mut Assembler, x: Reg, y: Reg, differ: Lbl| {
        a.jump(JNull { reg: x, offset: 0 }, differ);
        a.jump(JNull { reg: y, offset: 0 }, differ);
        a.op(Field { dst: r_la, obj: x, field: env.field_length });
        a.op(Field { dst: r_lb, obj: y, field: env.field_length });
        a.jump(JNotEq { a: r_la, b: r_lb, offset: 0 }, differ);
        a.op(Field { dst: r_ba, obj: x, field: env.field_bytes });
        a.op(Field { dst: r_bb, obj: y, field: env.field_bytes });
        a.op(Call3 { dst: r_cmp, fun: RefFun(fn_string_compare), arg0: r_ba, arg1: r_bb, arg2: r_la });
        a.jump(JNotEq { a: r_cmp, b: r_zero, offset: 0 }, differ);
    };
    // dst = r_parts[i] (the caller has checked the length).
    let part = |a: &mut Assembler, i: usize, dst: Reg| {
        a.op(Field { dst: r_jraw, obj: r_parts, field: RefField(1) });
        a.op(Int { dst: r_k, ptr: part_idx[i] });
        a.op(GetArray { dst: r_jdyn, array: r_jraw, index: r_k });
        a.op(UnsafeCast { dst, src: r_jdyn });
    };
    // Jumps to `owned` when this player has any bonus whose id is `id`, whatever its unit,
    // building or resource. Deliberately loose: a false "owned" can only skip a re-grant,
    // never grant a bonus twice.
    let owns_id = |a: &mut Assembler, id: Reg, owned: Lbl| {
        let not_owned = a.label();
        let lp = a.label();
        let next = a.label();
        a.op(GetThis { dst: r_proxy, field: field_bonuses });
        a.jump(JNull { reg: r_proxy, offset: 0 }, not_owned);
        a.op(Field { dst: r_owned_dyn, obj: r_proxy, field: field_proxy_array });
        a.jump(JNull { reg: r_owned_dyn, offset: 0 }, not_owned);
        a.op(SafeCast { dst: r_owned, src: r_owned_dyn });
        a.jump(JNull { reg: r_owned, offset: 0 }, not_owned);
        a.op(Int { dst: r_j, ptr: zero });
        a.bind_loop(lp);
        a.op(Field { dst: r_jlen, obj: r_owned, field: RefField(0) });
        a.jump(JSGte { a: r_j, b: r_jlen, offset: 0 }, not_owned);
        a.op(Field { dst: r_jraw, obj: r_owned, field: RefField(1) });
        a.op(GetArray { dst: r_jdyn, array: r_jraw, index: r_j });
        a.op(Incr { dst: r_j });
        a.op(UnsafeCast { dst: r_pb, src: r_jdyn });
        a.jump(JNull { reg: r_pb, offset: 0 }, next);
        a.op(Field { dst: r_pdata, obj: r_pb, field: field_pb_data });
        a.jump(JNull { reg: r_pdata, offset: 0 }, next);
        a.op(ToVirtual { dst: r_pv, src: r_pdata });
        a.jump(JNull { reg: r_pv, offset: 0 }, next);
        a.op(Field { dst: r_pid, obj: r_pv, field: field_pv_id });
        str_eq(a, r_pid, id, next);
        a.jump(JAlways { offset: 0 }, owned);
        a.bind(next);
        a.jump(JAlways { offset: 0 }, lp);
        a.bind(not_owned);
    };
    // r_content += "|" + s, with a null s written as "".
    let append = |a: &mut Assembler, s: Reg| {
        let have = a.label();
        emit_str(a, env, &sc, r_tmp, bar);
        emit_concat(a, env, r_content, r_content, r_tmp);
        a.op(Mov { dst: r_part, src: s });
        a.jump(JNotNull { reg: r_part, offset: 0 }, have);
        emit_str(a, env, &sc, r_part, empty);
        a.bind(have);
        emit_concat(a, env, r_content, r_content, r_part);
    };
    // Writes the granted record for the bonus in r_v: "<step>|<id>|<unitId>|<buildingId>|<resId>|<isAdvanced 1/0/empty>".
    let write_bonus_record = |a: &mut Assembler| {
        let adv_done = a.label();
        let adv_false = a.label();
        a.op(Mov { dst: r_content, src: r_step });
        a.op(Field { dst: r_vid, obj: r_v, field: field_id });
        append(a, r_vid);
        a.op(Field { dst: r_vid, obj: r_v, field: field_unit });
        append(a, r_vid);
        a.op(Field { dst: r_vid, obj: r_v, field: field_bld });
        append(a, r_vid);
        a.op(Field { dst: r_vid, obj: r_v, field: field_res });
        append(a, r_vid);
        a.op(Null { dst: r_vid });
        a.op(Field { dst: r_adv_n, obj: r_v, field: field_adv });
        a.jump(JNull { reg: r_adv_n, offset: 0 }, adv_done);
        a.op(SafeCast { dst: r_adv_b, src: r_adv_n });
        a.jump(JFalse { cond: r_adv_b, offset: 0 }, adv_false);
        emit_str(a, env, &sc, r_vid, flag_true);
        a.jump(JAlways { offset: 0 }, adv_done);
        a.bind(adv_false);
        emit_str(a, env, &sc, r_vid, flag_false);
        a.bind(adv_done);
        append(a, r_vid);
        emit_str(a, env, &sc, r_tmp, granted_dir);
        a.op(Call1 { dst: sc.void, fun: RefFun(env.fn_create_dir), arg0: r_tmp });
        a.op(Call2 { dst: sc.void, fun: RefFun(env.fn_save_content), arg0: r_record, arg1: r_content });
    };
    // Same record format for a building: "<step>|<buildingId>||||".
    let write_building_record = |a: &mut Assembler, bid: Reg| {
        a.op(Mov { dst: r_content, src: r_step });
        append(a, bid);
        for _ in 0..(RECORD_PARTS - 2) {
            append(a, r_null);
        }
        emit_str(a, env, &sc, r_tmp, granted_dir);
        a.op(Call1 { dst: sc.void, fun: RefFun(env.fn_create_dir), arg0: r_tmp });
        a.op(Call2 { dst: sc.void, fun: RefFun(env.fn_save_content), arg0: r_record, arg1: r_content });
    };
    let mut a = Assembler::new();
    let original = a.label();
    a.op(GetThis { dst: r_aijob, field: field_aijob });
    a.jump(JNotNull { reg: r_aijob, offset: 0 }, original);
    a.op(GetThis { dst: r_game, field: field_game });
    a.jump(JNull { reg: r_game, offset: 0 }, original);
    a.op(Field { dst: r_host, obj: r_game, field: field_host });
    a.jump(JNull { reg: r_host, offset: 0 }, original);
    a.op(Field { dst: r_auth, obj: r_host, field: field_is_auth });
    a.jump(JFalse { cond: r_auth, offset: 0 }, original);
    a.op(Call1 { dst: r_bf, fun: RefFun(fn_get_bifrost), arg0: r_game });
    a.jump(JNull { reg: r_bf, offset: 0 }, original);
    emit_require_active(&mut a, env, &sc, active, r_tmp, original);
    a.op(Field { dst: r_bm, obj: r_bf, field: field_bonus_mgr });
    a.jump(JNull { reg: r_bm, offset: 0 }, original);
    a.op(Field { dst: r_save, obj: r_bf, field: field_save });
    a.jump(JNull { reg: r_save, offset: 0 }, original);
    a.op(Call1 { dst: r_uid, fun: RefFun(fn_uid), arg0: Reg(0) });
    a.jump(JNull { reg: r_uid, offset: 0 }, original);
    // r_base = "<granted dir>\<run seed>_<uid>", run seed = startSeed (or seed if missing)
    let have_seed = a.label();
    // startSeed is a null<i32>: unwrap it to a plain i32 first (SafeCast, as the game's own code
    // does), then box it for Std.string. Missing startSeed: fall back to seed.
    let unwrap = a.label();
    a.op(Field { dst: r_start, obj: r_save, field: field_start_seed });
    a.jump(JNotNull { reg: r_start, offset: 0 }, unwrap);
    a.op(Field { dst: r_seed, obj: r_save, field: field_seed });
    a.jump(JAlways { offset: 0 }, have_seed);
    a.bind(unwrap);
    a.op(SafeCast { dst: r_seed, src: r_start });
    a.bind(have_seed);
    a.op(ToDyn { dst: r_dyn, src: r_seed });
    a.op(Call1 { dst: r_tmp, fun: RefFun(fn_std_string), arg0: r_dyn });
    emit_str(&mut a, env, &sc, r_base, granted_prefix);
    emit_concat(&mut a, env, r_base, r_base, r_tmp);
    emit_str(&mut a, env, &sc, r_tmp, underscore);
    emit_concat(&mut a, env, r_base, r_base, r_tmp);
    emit_concat(&mut a, env, r_base, r_base, r_uid);
    a.op(Field { dst: r_step_i, obj: r_save, field: field_step });
    a.op(ToDyn { dst: r_dyn, src: r_step_i });
    a.op(Call1 { dst: r_step, fun: RefFun(fn_std_string), arg0: r_dyn });
    a.op(Bool { dst: r_true, value: true });
    a.op(Bool { dst: r_false, value: false });
    a.op(ToDyn { dst: r_nb, src: r_false });
    a.op(ToDyn { dst: r_nb_true, src: r_true });
    a.op(Null { dst: r_null });
    a.op(Int { dst: r_zero, ptr: zero });
    a.op(Int { dst: r_one, ptr: one });

    for (ci, cat) in BONUS_CATEGORIES.iter().enumerate() {
        let cat_end = a.label();
        for slot in &slots[ci] {
            let next_copy = a.label();
            let handled = a.label();
            let grant = a.label();
            a.op(GetThis { dst: r_sent, field: slot.sentinel });
            a.jump(JTrue { cond: r_sent, offset: 0 }, next_copy);
            // Not owned (yet): stop this category for this tick.
            emit_str(&mut a, env, &sc, r_tmp, slot.marker);
            emit_exists(&mut a, env, sc.boolean, r_tmp);
            a.jump(JFalse { cond: sc.boolean, offset: 0 }, cat_end);
            // Already granted in this run? See the record format in write_bonus_record.
            emit_str(&mut a, env, &sc, r_tmp, slot.suffix);
            emit_concat(&mut a, env, r_record, r_base, r_tmp);
            emit_exists(&mut a, env, sc.boolean, r_record);
            let fresh = a.label();
            let legacy = a.label();
            a.jump(JFalse { cond: sc.boolean, offset: 0 }, fresh);
            a.op(Call1 { dst: r_content, fun: RefFun(fn_get_content), arg0: r_record });
            a.jump(JNull { reg: r_content, offset: 0 }, handled);
            emit_str(&mut a, env, &sc, r_bar, bar);
            a.op(Call2 { dst: r_parts, fun: RefFun(fn_split), arg0: r_content, arg1: r_bar });
            a.jump(JNull { reg: r_parts, offset: 0 }, legacy);
            a.op(Field { dst: r_k, obj: r_parts, field: RefField(0) });
            a.op(Int { dst: r_jlen, ptr: parts_count });
            a.jump(JSLt { a: r_k, b: r_jlen, offset: 0 }, legacy);
            // Stamped record. The run save only reaches disk when a realm is won, and winning
            // raises save.step first. So a record stamped with an older step was saved with
            // the run: done. One stamped with the current step was granted in an attempt at
            // this realm that was never saved (lost, retried, reloaded): unless the player
            // somehow still has it, grant the very same bonus again, keeping the record.
            part(&mut a, 0, r_part);
            str_eq(&mut a, r_part, r_step, handled);
            part(&mut a, 1, r_id);
            a.jump(JNull { reg: r_id, offset: 0 }, handled);
            a.op(Field { dst: r_la, obj: r_id, field: env.field_length });
            a.jump(JSLte { a: r_la, b: r_zero, offset: 0 }, handled);
            match &cat.source {
                BonusSource::Building => {
                    a.op(Call2 { dst: sc.boolean, fun: RefFun(fn_building_allowed), arg0: r_bf, arg1: r_id });
                    a.jump(JTrue { cond: sc.boolean, offset: 0 }, handled);
                    a.op(Call2 { dst: sc.void, fun: RefFun(fn_add_building), arg0: Reg(0), arg1: r_id });
                }
                BonusSource::Fixed(_) | BonusSource::Pool(_) => {
                    owns_id(&mut a, r_id, handled);
                    a.op(New { dst: r_v });
                    a.op(SetField { obj: r_v, field: field_id, src: r_id });
                    for (i, field) in [(2, field_unit), (3, field_bld), (4, field_res)] {
                        let skip = a.label();
                        part(&mut a, i, r_part);
                        a.jump(JNull { reg: r_part, offset: 0 }, skip);
                        a.op(Field { dst: r_la, obj: r_part, field: env.field_length });
                        a.jump(JSLte { a: r_la, b: r_zero, offset: 0 }, skip);
                        a.op(SetField { obj: r_v, field, src: r_part });
                        a.bind(skip);
                    }
                    let adv_done = a.label();
                    let not_true = a.label();
                    part(&mut a, 5, r_part);
                    emit_str(&mut a, env, &sc, r_tmp, flag_true);
                    str_eq(&mut a, r_part, r_tmp, not_true);
                    a.op(SetField { obj: r_v, field: field_adv, src: r_nb_true });
                    a.jump(JAlways { offset: 0 }, adv_done);
                    a.bind(not_true);
                    emit_str(&mut a, env, &sc, r_tmp, flag_false);
                    str_eq(&mut a, r_part, r_tmp, adv_done);
                    a.op(SetField { obj: r_v, field: field_adv, src: r_nb });
                    a.bind(adv_done);
                    a.op(Call2 { dst: sc.void, fun: RefFun(fn_apply), arg0: Reg(0), arg1: r_v });
                }
            }
            a.jump(JAlways { offset: 0 }, handled);
            // Legacy record (patch v17 and older): just the bonus id, no step, so it can't tell
            // whether the run save kept the grant. Decided by what the player owns instead.
            a.bind(legacy);
            a.op(Mov { dst: r_id, src: r_content });
            match &cat.source {
                BonusSource::Fixed(_) => {
                    // Fixed ids have no unit/resource/building, so hasBonus is exact. Still
                    // owned: done. Lost: grant the same id again, keeping the record.
                    a.op(Field { dst: r_la, obj: r_id, field: env.field_length });
                    a.jump(JSLte { a: r_la, b: r_zero, offset: 0 }, handled);
                    a.op(CallN { dst: sc.boolean, fun: RefFun(fn_has), args: vec![Reg(0), r_id, r_null, r_null, r_null] });
                    a.jump(JTrue { cond: sc.boolean, offset: 0 }, handled);
                    a.op(New { dst: r_v });
                    a.op(SetField { obj: r_v, field: field_id, src: r_id });
                    a.op(SetField { obj: r_v, field: field_adv, src: r_nb });
                    a.op(Call2 { dst: sc.void, fun: RefFun(fn_apply), arg0: Reg(0), arg1: r_v });
                    a.jump(JAlways { offset: 0 }, handled);
                }
                BonusSource::Pool(_) => {
                    // The record lacks the unit/resource/building, so the same bonus can't be
                    // rebuilt. Player has a bonus with this id: done. None at all: the grant was
                    // lost, so roll a fresh one (which rewrites the record in the new format).
                    // Recovers copies older patches lost to a failed realm, with no manual step.
                    a.op(Field { dst: r_la, obj: r_id, field: env.field_length });
                    a.jump(JSLte { a: r_la, b: r_zero, offset: 0 }, handled);
                    owns_id(&mut a, r_id, handled);
                    a.jump(JAlways { offset: 0 }, fresh);
                }
                BonusSource::Building => {
                    // Still unlocked: done. Lost after a reload: unlock the same building again.
                    a.op(Field { dst: r_la, obj: r_id, field: env.field_length });
                    a.jump(JSLte { a: r_la, b: r_zero, offset: 0 }, handled);
                    a.op(Call2 { dst: sc.boolean, fun: RefFun(fn_building_allowed), arg0: r_bf, arg1: r_id });
                    a.jump(JTrue { cond: sc.boolean, offset: 0 }, handled);
                    a.op(Call2 { dst: sc.void, fun: RefFun(fn_add_building), arg0: Reg(0), arg1: r_id });
                    a.jump(JAlways { offset: 0 }, handled);
                }
            }
            a.bind(fresh);
            match &cat.source {
                BonusSource::Fixed(_) => {
                    // Pass 1: count the ids this player doesn't own yet.
                    a.op(Int { dst: r_count, ptr: zero });
                    for k in &ids[ci] {
                        let owned = a.label();
                        emit_str(&mut a, env, &sc, r_id, *k);
                        a.op(CallN { dst: sc.boolean, fun: RefFun(fn_has), args: vec![Reg(0), r_id, r_null, r_null, r_null] });
                        a.jump(JTrue { cond: sc.boolean, offset: 0 }, owned);
                        a.op(Add { dst: r_count, a: r_count, b: r_one });
                        a.bind(owned);
                    }
                    a.jump(JSLte { a: r_count, b: r_zero, offset: 0 }, handled); // nothing left in this category
                    // Pass 2: grant the r_roll-th unowned id, r_roll random in 0..count.
                    a.op(Call1 { dst: r_roll, fun: RefFun(fn_std_random), arg0: r_count });
                    for k in &ids[ci] {
                        let next_id = a.label();
                        emit_str(&mut a, env, &sc, r_id, *k);
                        a.op(CallN { dst: sc.boolean, fun: RefFun(fn_has), args: vec![Reg(0), r_id, r_null, r_null, r_null] });
                        a.jump(JTrue { cond: sc.boolean, offset: 0 }, next_id);
                        let this_one = a.label();
                        a.jump(JSLte { a: r_roll, b: r_zero, offset: 0 }, this_one);
                        a.op(Sub { dst: r_roll, a: r_roll, b: r_one });
                        a.jump(JAlways { offset: 0 }, next_id);
                        a.bind(this_one);
                        a.op(New { dst: r_v });
                        a.op(SetField { obj: r_v, field: field_id, src: r_id });
                        a.op(SetField { obj: r_v, field: field_adv, src: r_nb });
                        a.jump(JAlways { offset: 0 }, grant);
                        a.bind(next_id);
                    }
                    a.jump(JAlways { offset: 0 }, handled);
                }
                BonusSource::Pool(_) => {
                    a.op(StaticClosure { dst: r_clos, fun: RefFun(always_true) });
                    a.op(Call3 { dst: r_list, fun: RefFun(fn_compat), arg0: r_bm, arg1: Reg(0), arg2: r_clos });
                    a.jump(JNull { reg: r_list, offset: 0 }, handled);
                    // Two passes over the list: count entries whose id is listed, roll one, take it.
                    let no_match = a.label();
                    for pass in 0..2 {
                        let lp = a.label();
                        let next_item = a.label();
                        let pass_done = a.label();
                        if pass == 0 {
                            a.op(Int { dst: r_count, ptr: zero });
                        }
                        a.op(Int { dst: r_i, ptr: zero });
                        a.bind_loop(lp);
                        a.op(Field { dst: r_len, obj: r_list, field: RefField(0) });
                        a.jump(JSGte { a: r_i, b: r_len, offset: 0 }, pass_done);
                        a.op(Field { dst: r_raw, obj: r_list, field: RefField(1) });
                        a.op(GetArray { dst: r_dyn, array: r_raw, index: r_i });
                        a.op(ToVirtual { dst: r_v, src: r_dyn });
                        a.op(Incr { dst: r_i });
                        a.jump(JNull { reg: r_v, offset: 0 }, next_item);
                        a.op(Field { dst: r_vid, obj: r_v, field: field_id });
                        a.jump(JNull { reg: r_vid, offset: 0 }, next_item);
                        a.op(Field { dst: r_la, obj: r_vid, field: env.field_length });
                        a.op(Field { dst: r_ba, obj: r_vid, field: env.field_bytes });
                        let matched = a.label();
                        for k in &ids[ci] {
                            let differ = a.label();
                            emit_str(&mut a, env, &sc, r_id, *k);
                            a.op(Field { dst: r_lb, obj: r_id, field: env.field_length });
                            a.jump(JNotEq { a: r_la, b: r_lb, offset: 0 }, differ);
                            a.op(Field { dst: r_bb, obj: r_id, field: env.field_bytes });
                            a.op(Call3 { dst: r_cmp, fun: RefFun(fn_string_compare), arg0: r_ba, arg1: r_bb, arg2: r_la });
                            a.jump(JEq { a: r_cmp, b: r_zero, offset: 0 }, matched);
                            a.bind(differ);
                        }
                        a.jump(JAlways { offset: 0 }, next_item);
                        a.bind(matched);
                        if pass == 0 {
                            a.op(Add { dst: r_count, a: r_count, b: r_one });
                        } else {
                            a.jump(JSLte { a: r_roll, b: r_zero, offset: 0 }, grant);
                            a.op(Sub { dst: r_roll, a: r_roll, b: r_one });
                        }
                        a.bind(next_item);
                        a.jump(JAlways { offset: 0 }, lp);
                        a.bind(pass_done);
                        if pass == 0 {
                            a.jump(JSLte { a: r_count, b: r_zero, offset: 0 }, no_match);
                            a.op(Call1 { dst: r_roll, fun: RefFun(fn_std_random), arg0: r_count });
                        }
                    }
                    a.jump(JAlways { offset: 0 }, handled);
                    // No match in the game's shared list: the fallback below.
                    a.bind(no_match);
                    // The game's list skips perks ANY player owns (vanilla never gives two players
                    // the same perk). When it has nothing left for this player, grant a perk they
                    // don't own yet from the category's variant-free fallback list, even if a
                    // teammate has it, so every player gets every copy. bonusIsAllowed keeps the
                    // clan rules (forClan, notForClans, clan type).
                    let nothing_left = a.label();
                    if !fallback_ids[ci].is_empty() {
                        let adv = if cat.key == "Clan" { r_nb } else { r_nb_true };
                        a.op(GetThis { dst: r_clan, field: field_clan });
                        for pass in 0..2 {
                            if pass == 0 {
                                a.op(Int { dst: r_count, ptr: zero });
                            }
                            for k in &fallback_ids[ci] {
                                let skip = a.label();
                                emit_str(&mut a, env, &sc, r_id, *k);
                                a.op(CallN { dst: sc.boolean, fun: RefFun(fn_has), args: vec![Reg(0), r_id, r_null, r_null, r_null] });
                                a.jump(JTrue { cond: sc.boolean, offset: 0 }, skip);
                                a.op(New { dst: r_v });
                                a.op(SetField { obj: r_v, field: field_id, src: r_id });
                                a.op(SetField { obj: r_v, field: field_adv, src: adv });
                                a.op(Call3 { dst: sc.boolean, fun: RefFun(fn_allowed), arg0: r_v, arg1: r_clan, arg2: r_true });
                                a.jump(JFalse { cond: sc.boolean, offset: 0 }, skip);
                                if pass == 0 {
                                    a.op(Add { dst: r_count, a: r_count, b: r_one });
                                } else {
                                    a.jump(JSLte { a: r_roll, b: r_zero, offset: 0 }, grant);
                                    a.op(Sub { dst: r_roll, a: r_roll, b: r_one });
                                }
                                a.bind(skip);
                            }
                            if pass == 0 {
                                a.jump(JSLte { a: r_count, b: r_zero, offset: 0 }, nothing_left);
                                a.op(Call1 { dst: r_roll, fun: RefFun(fn_std_random), arg0: r_count });
                            }
                        }
                        a.jump(JAlways { offset: 0 }, handled);
                    }
                    a.bind(nothing_left);
                    a.jump(JAlways { offset: 0 }, handled);
                }
                BonusSource::Building => {
                    // Random clan-compatible building that vanilla Bifrost can offer (weight > 0),
                    // unlocked for the run through a network call (every machine's run save gets
                    // it); the record holds its id. Two passes: count eligible, roll, take it.
                    a.op(Call2 { dst: r_list, fun: RefFun(fn_buildings), arg0: r_bm, arg1: Reg(0) });
                    a.jump(JNull { reg: r_list, offset: 0 }, handled);
                    let picked = a.label();
                    for pass in 0..2 {
                        let lp = a.label();
                        let next_b = a.label();
                        let pass_done = a.label();
                        if pass == 0 {
                            a.op(Int { dst: r_count, ptr: zero });
                        }
                        a.op(Int { dst: r_i, ptr: zero });
                        a.bind_loop(lp);
                        a.op(Field { dst: r_len, obj: r_list, field: RefField(0) });
                        a.jump(JSGte { a: r_i, b: r_len, offset: 0 }, pass_done);
                        a.op(Field { dst: r_raw, obj: r_list, field: RefField(1) });
                        a.op(GetArray { dst: r_dyn, array: r_raw, index: r_i });
                        a.op(ToVirtual { dst: r_bld, src: r_dyn });
                        a.op(Incr { dst: r_i });
                        a.jump(JNull { reg: r_bld, offset: 0 }, next_b);
                        a.op(Field { dst: r_bprops, obj: r_bld, field: field_building_props });
                        a.jump(JNull { reg: r_bprops, offset: 0 }, next_b);
                        a.op(Field { dst: r_bweight_n, obj: r_bprops, field: field_building_weight });
                        a.jump(JNull { reg: r_bweight_n, offset: 0 }, next_b);
                        a.op(SafeCast { dst: r_bweight, src: r_bweight_n });
                        a.jump(JSLte { a: r_bweight, b: r_zero, offset: 0 }, next_b);
                        if pass == 0 {
                            a.op(Add { dst: r_count, a: r_count, b: r_one });
                        } else {
                            a.jump(JSLte { a: r_roll, b: r_zero, offset: 0 }, picked);
                            a.op(Sub { dst: r_roll, a: r_roll, b: r_one });
                        }
                        a.bind(next_b);
                        a.jump(JAlways { offset: 0 }, lp);
                        a.bind(pass_done);
                        if pass == 0 {
                            a.jump(JSLte { a: r_count, b: r_zero, offset: 0 }, handled);
                            a.op(Call1 { dst: r_roll, fun: RefFun(fn_std_random), arg0: r_count });
                        }
                    }
                    a.jump(JAlways { offset: 0 }, handled);
                    a.bind(picked);
                    a.op(Field { dst: r_vid, obj: r_bld, field: field_building_id });
                    a.jump(JNull { reg: r_vid, offset: 0 }, handled);
                    a.op(Call2 { dst: sc.void, fun: RefFun(fn_add_building), arg0: Reg(0), arg1: r_vid });
                    write_building_record(&mut a, r_vid);
                    a.jump(JAlways { offset: 0 }, handled);
                }
            }
            // r_v holds the bonus: grant it (every machine's run save + host apply), then write
            // the granted record.
            a.bind(grant);
            a.op(Call2 { dst: sc.void, fun: RefFun(fn_apply), arg0: Reg(0), arg1: r_v });
            write_bonus_record(&mut a);
            a.bind(handled);
            a.op(SetThis { field: slot.sentinel, src: r_true });
            a.bind(next_copy);
        }
        a.bind(cat_end);
    }
    let n = a.len();
    a.bind_external(original, n);
    let mut ops = a.finish()?;
    ops.extend(o);
    install(code, update, ops, regs.new, |i| i.saturating_sub(n))
}

/// Fields in a granted record: step, id, unitId, buildingId, resId, isAdvanced.
const RECORD_PARTS: usize = 6;

/// Haxe's String.split(String) -> Array<String>. Looked up by shape: the std String methods
/// carry no owning class in this build, so find_method_findex can't see them.
fn find_string_split(code: &Bytecode, env: &Env) -> Result<usize> {
    let found: Vec<usize> = code
        .functions
        .iter()
        .filter(|f| f.name(code) == "split" && f.regs.len() >= 2 && f.regs[0] == env.string_ty && f.regs[1] == env.string_ty)
        .filter(|f| matches!(code.get(f.t), Type::Fun(tf) if tf.args.len() == 2))
        .map(|f| f.findex.0)
        .collect();
    match found.as_slice() {
        [one] => Ok(*one),
        other => bail!("expected exactly one String.split, found {other:?}. Northgard build mismatch?"),
    }
}

/// Variant-free fallbacks per pool category (no unit/resource/building choice, no building
/// prerequisite), used when the game's shared list has nothing left for a player. See
/// patch_bonus_items.
fn fallback_ids_for(key: &str) -> &'static [&'static str] {
    match key {
        "Military" => &["BWarband", "BWarchief", "BWarchiefVsUndead", "BWarchiefCooldown"],
        "Economy" => &["BHappyBonus", "BWinter", "BPopGrowth"],
        "Special" => &["BFoesAttack", "BRuinsExploration", "BMoreSheeps"],
        "Clan" => &[
            "BSkalds", "BStagFame2", "BGoatSheeps", "BBarricades", "BFeast", "BFoodAnimals",
            "BBerserkerBetter", "BMilitaryHappy", "BHomeland", "BColonizeKrowns", "BKaija",
            "BShieldBearerUpgraded", "BRelicBear", "BBuildingUpgrade", "BSigny", "BScorchedEarth",
            "BDragonmen", "BSacrifice", "BSlaves", "BWarchiefRegen", "BWarchiefProd", "BNorn",
            "BSpectralWarrior", "BCoastalHurt", "BRecycling", "BHeartyLife", "BOxShadow", "BHomeDeer",
            "BWilderHunt", "BMultipleMeal", "BCook", "BSuperMeal", "BDeathDisease", "BOverwork",
            "BMoreHousing", "BOffensiveCivilian", "BFarColonize", "BGlacialWinds", "BHildegardHeal",
            "BUnlimitedRelics", "BDuchyHouse", "BNoNornCap", "BThreeWarchief",
        ],
        _ => &[],
    }
}

/// `ChooseBifrostBonusContent.update` (every frame while a perk screen is open, on every
/// machine): turns every card whose position is still an unchecked location into an Archipelago
/// card. The Archipelago logo (`bifrost\ap_card.png`, written by the client) replaces the card's
/// `back` background, sized and placed like it, and every art layer on top is hidden. Its tooltip
/// becomes the scouted item, read from `bifrost\scout_<Realm>_<Screen>_<K>.txt` (also written by
/// the client) each time the tooltip opens. Positions use the same rule as patch_perk_pick
/// (emit_screen_name / emit_card_key, K = index + 1 across the whole screen), so a marked card is
/// exactly a card whose pick sends a check. Every machine has the same markers (same slot), so
/// every screen agrees. Each card it converts also gets `done\seen_<Realm>_<Screen>_<K>`, which
/// the client turns into a server hint. Each card is converted once (an `apCardMarked` flag on the
/// button) and stays an AP card until the screen closes; on later visits, positions whose check
/// was sent show their real perk.
pub fn patch_ap_cards(code: &mut Bytecode, env: &Env) -> Result<()> {
    let update = find_method_findex(code, "ui.win.ChooseBifrostBonusContent", "update")?;
    let fn_buttons = find_method_findex(code, "ui.win.ChooseBifrostBonusContent", "getButtons")?;
    let fn_get_bifrost = find_method_findex(code, "Game", "get_bifrost")?;
    let fn_get_bytes = find_method_findex(code, "sys.io.$File", "getBytes")?;
    let fn_any_from_bytes = find_method_findex(code, "hxd.res.$Any", "fromBytes")?;
    let fn_any_to_tile = find_method_findex(code, "hxd.res.Any", "toTile")?;
    let fn_scale = find_method_findex(code, "h2d.Tile", "scaleToSize")?;
    let fn_set_tile = find_method_findex(code, "h2d.Bitmap", "set_tile")?;
    let fn_default_tip = find_method_findex(code, "ui.elt.TipElement", "defaultGetTip")?;
    let (field_window, window_ty) = find_field_by_name(code, "ui.win.ChooseBifrostBonusContent", "window")?;
    let (field_source, source_ty) = find_field_by_name(code, "ui.win.ChooseBifrostBonus", "bonusSource")?;
    let (field_icon, icon_ty) = find_field_by_name(code, "ui.elt.BifrostBonusButton", "mainIcon")?;
    // A card's art layers depend on its kind: perk cards draw backIcon (+ backSecondIcon), other
    // kinds draw mainIcon (+ secondIcon) over the `back` background. The logo always goes into
    // backIcon and every other art layer is hidden, so nothing shows through or on top.
    let (field_back, _) = find_field_by_name(code, "ui.elt.BifrostBonusButton", "back")?;
    let (field_back_icon, _) = find_field_by_name(code, "ui.elt.BifrostBonusButton", "backIcon")?;
    let (field_back_second, _) = find_field_by_name(code, "ui.elt.BifrostBonusButton", "backSecondIcon")?;
    let (field_second, _) = find_field_by_name(code, "ui.elt.BifrostBonusButton", "secondIcon")?;
    let fn_set_visible = find_method_findex(code, "h2d.Object", "set_visible")?;
    let (field_get_tip, get_tip_ty) = find_field_by_name(code, "ui.elt.BifrostBonusButton", "getTip")?;
    let (field_get_tip_text, get_tip_text_ty) = find_field_by_name(code, "ui.elt.BifrostBonusButton", "getTipText")?;
    let (field_tip_on, _) = find_field_by_name(code, "ui.elt.BifrostBonusButton", "tipActivated")?;
    let (field_bmp_tile, tile_ty) = find_field_by_name(code, "h2d.Bitmap", "tile")?;
    let (field_tile_w, _) = find_field_by_name(code, "h2d.Tile", "width")?;
    let (field_tile_h, _) = find_field_by_name(code, "h2d.Tile", "height")?;
    let (field_tile_dx, _) = find_field_by_name(code, "h2d.Tile", "dx")?;
    let (field_tile_dy, _) = find_field_by_name(code, "h2d.Tile", "dy")?;
    let (field_realm, _) = find_field_by_name(code, "gamesys.bifrost.BifrostManager", "realm")?;
    let bytes_obj_ty = function(code, fn_get_bytes, "File.getBytes")?.regs.get(1).copied()
        .context("File.getBytes has no locals")?;
    let any_ty = obj_type(code, "hxd.res.Any")?;
    let bytes_obj_ty = {
        // haxe.io.Bytes: the second argument type of Any.fromBytes.
        let f = function(code, fn_any_from_bytes, "Any.fromBytes")?;
        let _ = bytes_obj_ty;
        f.regs[1]
    };
    let button_ty = obj_type(code, "ui.elt.BifrostBonusButton")?;
    let bf_ty = obj_type(code, "gamesys.bifrost.BifrostManager")?;
    let array_obj_ty = obj_type(code, "hl.types.ArrayObj")?;
    let field_marked = add_field(code, "ui.elt.BifrostBonusButton", "apCardMarked", env.bool_ty)?;
    let grey = GreyStyle::find(code)?;
    let (field_dom, disabled_global, disabled_field, fn_remove_class) =
        (grey.field_dom, grey.global, grey.field, grey.fn_remove_class);
    let (global_game, field_inst, game_global_ty, inst_ty) = {
        let f = function(code, find_method_findex(code, "$HBifrost", "pickObjective")?, "pickObjective")?;
        match (&f.ops[21], &f.ops[22]) {
            (Opcode::GetGlobal { global, .. }, Opcode::Field { field, .. }) => (*global, *field, f.regs[5], f.regs[4]),
            _ => bail!("pickObjective ops 21-22 changed"),
        }
    };

    let keys = CardKeyConsts::new(code)?;
    let active = konst(code, &path_in(&env.bifrost_dir, ACTIVE_FLAG_NAME));
    let card_prefix = konst(code, &path_in(&env.bifrost_dir, "card_"));
    let scout_prefix = konst(code, &path_in(&env.bifrost_dir, "scout_"));
    let icon_path = konst(code, &path_in(&env.bifrost_dir, "ap_card.png"));
    let seen_prefix = konst(code, &path_in(&env.done_dir, "seen_"));
    let done_dir = konst(code, &env.done_dir);
    let empty = konst(code, "");
    let txt = konst(code, ".txt");
    let zero = int_konst(code, 0);
    let one = int_konst(code, 1);

    let f = function(code, update, "ChooseBifrostBonusContent.update")?;
    let o = f.ops.clone();
    let mut regs = Regs::new(f.regs.len());
    let sc = Scratch::new(env, &mut regs);
    let kr = CardKeyRegs::new(env, &mut regs);
    let r_window = regs.alloc(window_ty);
    let r_source = regs.alloc(source_ty);
    let r_gg = regs.alloc(game_global_ty);
    let r_inst = regs.alloc(inst_ty);
    let r_bf = regs.alloc(bf_ty);
    let r_realm = regs.alloc(env.string_ty);
    let r_screen = regs.alloc(env.string_ty);
    let r_i = regs.alloc(env.i32_ty);
    let r_n = regs.alloc(env.i32_ty);
    let r_k = regs.alloc(env.i32_ty);
    let r_c0 = regs.alloc(env.i32_ty);
    let r_c1 = regs.alloc(env.i32_ty);
    let r_buttons = regs.alloc(array_obj_ty);
    let r_raw = regs.alloc(env.array_ty);
    let r_dyn = regs.alloc(env.dyn_ty);
    let r_btn = regs.alloc(button_ty);
    let r_marked = regs.alloc(env.bool_ty);
    let r_true = regs.alloc(env.bool_ty);
    let r_tmp = regs.alloc(env.string_ty);
    let r_key = regs.alloc(env.string_ty); // "<realm>_<screen>_<K>"
    let r_path = regs.alloc(env.string_ty);
    let r_icon_path = regs.alloc(env.string_ty);
    let r_bytes = regs.alloc(bytes_obj_ty);
    let r_any = regs.alloc(any_ty);
    let r_tile = regs.alloc(tile_ty);
    let r_old_tile = regs.alloc(tile_ty);
    let r_icon = regs.alloc(icon_ty);
    let r_w = regs.alloc(env.f64_ty);
    let r_h = regs.alloc(env.f64_ty);
    let r_tip_text = regs.alloc(get_tip_text_ty);
    let r_tip = regs.alloc(get_tip_ty);
    let r_layer = regs.alloc(icon_ty);
    let r_false = regs.alloc(env.bool_ty);
    let r_dom = regs.alloc(grey.dom_ty);
    let r_style_global = regs.alloc(grey.global_ty);
    let r_style = regs.alloc(grey.style_ty);
    let r_dom_ret = regs.alloc(grey.ret_ty);

    use Opcode::*;
    let mut a = Assembler::new();
    let original = a.label();
    emit_require_active(&mut a, env, &sc, active, r_tmp, original);
    a.op(GetThis { dst: r_window, field: field_window });
    a.jump(JNull { reg: r_window, offset: 0 }, original);
    a.op(Field { dst: r_source, obj: r_window, field: field_source });
    a.jump(JNull { reg: r_source, offset: 0 }, original);
    a.op(GetGlobal { dst: r_gg, global: global_game });
    a.op(Field { dst: r_inst, obj: r_gg, field: field_inst });
    a.jump(JNull { reg: r_inst, offset: 0 }, original);
    a.op(Call1 { dst: r_bf, fun: RefFun(fn_get_bifrost), arg0: r_inst });
    a.jump(JNull { reg: r_bf, offset: 0 }, original);
    a.op(Field { dst: r_realm, obj: r_bf, field: field_realm });
    a.jump(JNull { reg: r_realm, offset: 0 }, original);
    a.op(Int { dst: r_c0, ptr: zero });
    a.op(Int { dst: r_c1, ptr: one });
    a.op(Bool { dst: r_true, value: true });
    emit_screen_name(&mut a, env, &sc, &kr, &keys, r_source, r_screen, original);
    // The screen's cards, numbered across the whole screen (see patch_perk_pick).
    a.op(Call1 { dst: r_buttons, fun: RefFun(fn_buttons), arg0: Reg(0) });
    a.jump(JNull { reg: r_buttons, offset: 0 }, original);
    a.op(Field { dst: r_n, obj: r_buttons, field: RefField(0) });

    a.op(Int { dst: r_i, ptr: zero });
    let lp = a.label();
    let next = a.label();
    a.bind_loop(lp);
    a.jump(JSGte { a: r_i, b: r_n, offset: 0 }, original);
    a.op(Field { dst: r_raw, obj: r_buttons, field: RefField(1) });
    a.op(GetArray { dst: r_dyn, array: r_raw, index: r_i });
    a.op(UnsafeCast { dst: r_btn, src: r_dyn });
    a.op(Mov { dst: r_k, src: r_i });
    a.op(Incr { dst: r_i });
    a.jump(JNull { reg: r_btn, offset: 0 }, next);
    a.op(Field { dst: r_marked, obj: r_btn, field: field_marked });
    a.jump(JTrue { cond: r_marked, offset: 0 }, next);
    emit_card_key(&mut a, env, &sc, &kr, &keys, r_realm, r_screen, r_k, r_key);
    emit_str(&mut a, env, &sc, r_tmp, card_prefix);
    emit_concat(&mut a, env, r_path, r_tmp, r_key);
    emit_exists(&mut a, env, sc.boolean, r_path);
    a.jump(JFalse { cond: sc.boolean, offset: 0 }, next);
    a.op(SetField { obj: r_btn, field: field_marked, src: r_true });
    // The game greys out a card it thinks can't be picked (e.g. a perk the player already got
    // from a bonus item). An AP card is always pickable (see patch_ap_card_selectable), so drop
    // the grey style it may have been built with.
    a.op(Field { dst: r_dom, obj: r_btn, field: field_dom });
    let no_dom = a.label();
    a.jump(JNull { reg: r_dom, offset: 0 }, no_dom);
    a.op(GetGlobal { dst: r_style_global, global: disabled_global });
    a.op(Field { dst: r_style, obj: r_style_global, field: disabled_field });
    a.op(Call2 { dst: r_dom_ret, fun: RefFun(fn_remove_class), arg0: r_dom, arg1: r_style });
    a.bind(no_dom);
    // "This AP card was seen": the client hints its item on the server (new hints only).
    emit_str(&mut a, env, &sc, r_tmp, seen_prefix);
    emit_concat(&mut a, env, r_path, r_tmp, r_key);
    emit_write_marker(&mut a, env, &sc, done_dir, empty, r_path, r_tmp);
    // Icon: the Archipelago logo as the card's `back` (its full-size background inside the
    // frame), sized and placed like the original background; every art layer drawn on top of it
    // (backIcon, backSecondIcon, mainIcon, secondIcon) is hidden. Same look for every card kind.
    let no_icon = a.label();
    emit_str(&mut a, env, &sc, r_icon_path, icon_path);
    emit_exists(&mut a, env, sc.boolean, r_icon_path);
    a.jump(JFalse { cond: sc.boolean, offset: 0 }, no_icon);
    a.op(Field { dst: r_icon, obj: r_btn, field: field_back });
    a.jump(JNull { reg: r_icon, offset: 0 }, no_icon);
    a.op(Field { dst: r_old_tile, obj: r_icon, field: field_bmp_tile });
    a.jump(JNull { reg: r_old_tile, offset: 0 }, no_icon);
    a.op(Call1 { dst: r_bytes, fun: RefFun(fn_get_bytes), arg0: r_icon_path });
    a.jump(JNull { reg: r_bytes, offset: 0 }, no_icon);
    a.op(Call2 { dst: r_any, fun: RefFun(fn_any_from_bytes), arg0: r_icon_path, arg1: r_bytes });
    a.jump(JNull { reg: r_any, offset: 0 }, no_icon);
    a.op(Call1 { dst: r_tile, fun: RefFun(fn_any_to_tile), arg0: r_any });
    a.jump(JNull { reg: r_tile, offset: 0 }, no_icon);
    a.op(Field { dst: r_w, obj: r_old_tile, field: field_tile_w });
    a.op(Field { dst: r_h, obj: r_old_tile, field: field_tile_h });
    a.op(Call3 { dst: sc.void, fun: RefFun(fn_scale), arg0: r_tile, arg1: r_w, arg2: r_h });
    a.op(Field { dst: r_w, obj: r_old_tile, field: field_tile_dx });
    a.op(SetField { obj: r_tile, field: field_tile_dx, src: r_w });
    a.op(Field { dst: r_h, obj: r_old_tile, field: field_tile_dy });
    a.op(SetField { obj: r_tile, field: field_tile_dy, src: r_h });
    a.op(Call2 { dst: r_old_tile, fun: RefFun(fn_set_tile), arg0: r_icon, arg1: r_tile });
    a.op(Call2 { dst: sc.boolean, fun: RefFun(fn_set_visible), arg0: r_icon, arg1: r_true });
    a.op(Bool { dst: r_false, value: false });
    for layer in [field_back_icon, field_back_second, field_icon, field_second] {
        let skip = a.label();
        a.op(Field { dst: r_layer, obj: r_btn, field: layer });
        a.jump(JNull { reg: r_layer, offset: 0 }, skip);
        a.op(Call2 { dst: sc.boolean, fun: RefFun(fn_set_visible), arg0: r_layer, arg1: r_false });
        a.bind(skip);
    }
    a.bind(no_icon);
    // Tooltip: the scouted item, read from its file whenever the tooltip opens.
    emit_str(&mut a, env, &sc, r_tmp, scout_prefix);
    emit_concat(&mut a, env, r_path, r_tmp, r_key);
    emit_str(&mut a, env, &sc, r_tmp, txt);
    emit_concat(&mut a, env, r_path, r_path, r_tmp);
    emit_exists(&mut a, env, sc.boolean, r_path);
    a.jump(JFalse { cond: sc.boolean, offset: 0 }, next);
    a.op(InstanceClosure { dst: r_tip_text, fun: RefFun(env.fn_get_content), obj: r_path });
    a.op(SetField { obj: r_btn, field: field_get_tip_text, src: r_tip_text });
    a.op(InstanceClosure { dst: r_tip, fun: RefFun(fn_default_tip), obj: r_btn });
    a.op(SetField { obj: r_btn, field: field_get_tip, src: r_tip });
    a.op(SetField { obj: r_btn, field: field_tip_on, src: r_true });
    a.bind(next);
    a.jump(JAlways { offset: 0 }, lp);
    let n = a.len();
    a.bind_external(original, n);
    let mut ops = a.finish()?;
    ops.extend(o);
    install(code, update, ops, regs.new, |i| i.saturating_sub(n))
}

/// The pieces of `BifrostBonusButton.greyOut(false)` that drop a card's grey "disabled" style:
/// `this.dom.removeClass(<global>.DISABLED_STYLE)` (its ops 12-16), with the register types.
struct GreyStyle {
    field_dom: RefField,
    global: hlbc::types::RefGlobal,
    field: RefField,
    fn_remove_class: usize,
    dom_ty: RefType,
    global_ty: RefType,
    style_ty: RefType,
    ret_ty: RefType,
}

impl GreyStyle {
    fn find(code: &Bytecode) -> Result<Self> {
        let findex = find_method_findex(code, "ui.elt.BifrostBonusButton", "greyOut")?;
        let f = function(code, findex, "BifrostBonusButton.greyOut")?;
        match (f.ops.get(12), f.ops.get(14), f.ops.get(15), f.ops.get(16)) {
            (
                Some(Opcode::GetThis { dst: dom, field: field_dom }),
                Some(Opcode::GetGlobal { dst: g, global }),
                Some(Opcode::Field { dst: s, field, .. }),
                Some(Opcode::Call2 { dst: ret, fun, .. }),
            ) => Ok(GreyStyle {
                field_dom: *field_dom,
                global: *global,
                field: *field,
                fn_remove_class: fun.0,
                dom_ty: f.regs[dom.0 as usize],
                global_ty: f.regs[g.0 as usize],
                style_ty: f.regs[s.0 as usize],
                ret_ty: f.regs[ret.0 as usize],
            }),
            _ => bail!("BifrostBonusButton.greyOut ops 12-16 changed. Northgard build mismatch?"),
        }
    }
}

/// `BifrostBonusButton.canBeSelected`: an AP card can always be picked. The game rejects a card
/// whose bonus the player already owns or can't use, which an AP card's hidden perk can be (most
/// often a perk the player already got from a bonus item): the card was greyed out and clicking
/// it did nothing, so its location could never be checked. After the "already picked by someone"
/// check (ops 0-13, kept so a single card still can't be picked twice in co-op), a card marked by
/// patch_ap_cards returns true (op 76) instead of checking its bonus. Must run after
/// patch_ap_cards, which adds the `apCardMarked` field.
///
/// Safety net (Bifrost mode active only): a perk card the local player already owns stays
/// pickable too (ConquestBonus branch, op 33), and patch_perk_pick turns that pick into
/// "nothing". Objective offers already leave out perks anyone owns (see rewards.rs), so this
/// only matters if a player gets the perk while the screen is open or every pool ran dry: the
/// screen can then still be confirmed instead of soft locking.
pub fn patch_ap_card_selectable(code: &mut Bytecode, env: &Env) -> Result<()> {
    let findex = find_method_findex(code, "ui.elt.BifrostBonusButton", "canBeSelected")?;
    let (field_marked, marked_ty) = find_field_by_name(code, "ui.elt.BifrostBonusButton", "apCardMarked")?;
    let f = function(code, findex, "BifrostBonusButton.canBeSelected")?;
    let o = f.ops.clone();
    use Opcode::*;
    let shape_ok = o.len() == 80
        && matches!(o[14], GetThis { .. })
        && matches!(o[17], Switch { .. })
        && matches!(o[76], Bool { value: true, .. })
        && matches!(o[77], Ret { .. })
        && matches!(o[32], Call4 { dst, arg0, arg1, arg2, .. } if dst == Reg(3) && arg0 == Reg(10) && arg1 == Reg(9) && arg2 == Reg(13))
        && matches!(o[33], Ret { ret } if ret == Reg(3));
    if !shape_ok {
        bail!("canBeSelected shape changed. Northgard build mismatch?");
    }
    let fn_has = find_method_findex(code, "gamesys.bifrost.BifrostBonusManager", "playerHasBonus")?;
    let nregs = f.regs.len();
    let active = konst(code, &path_in(&env.bifrost_dir, ACTIVE_FLAG_NAME));
    let mut regs = Regs::new(nregs);
    let r_marked = regs.alloc(marked_ty);
    let sc = Scratch::new(env, &mut regs);
    let r_tmp = regs.alloc(env.string_ty);
    let r_has = regs.alloc(env.bool_ty);
    let owned_block = {
        let mut a = Assembler::new();
        let ret = a.label();
        a.jump(JTrue { cond: Reg(3), offset: 0 }, ret);
        emit_require_active(&mut a, env, &sc, active, r_tmp, ret);
        a.op(Call3 { dst: r_has, fun: RefFun(fn_has), arg0: Reg(10), arg1: Reg(9), arg2: Reg(13) });
        a.jump(JFalse { cond: r_has, offset: 0 }, ret);
        a.op(Bool { dst: Reg(3), value: true });
        a.bind(ret);
        let mut ops = a.finish()?;
        ops.push(o[33].clone());
        ops
    };
    let mut a = Assembler::new();
    a.op(GetThis { dst: r_marked, field: field_marked });
    a.jump_original(JTrue { cond: r_marked, offset: 0 }, 76);
    let (mut block, external) = a.finish_with_externals()?;
    block.push(o[14].clone());
    let (ops, old_of_new) = splice(
        &o,
        vec![
            Replacement { at: 14, block, external },
            Replacement { at: 33, block: owned_block, external: Vec::new() },
        ],
    )?;
    install(code, findex, ops, regs.new, |i| old_of_new[i])
}
