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
use hlbc::types::{Reg, RefField, RefFun, RefInt, RefString, RefType};
use hlbc::Bytecode;
use std::env;
use std::fs;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

const GATE_FINDEX: usize = 8760; // Conquest.getBattleState
const STRING_ADD_FINDEX: usize = 21; // String.__add__
const GET_PATH_FINDEX: usize = 16543; // sys.FileSystem's path-normalizing helper
const SYS_EXISTS_FINDEX: usize = 16542; // native sys_exists

// Live-refresh patch (see patch_map_auto_refresh below): every frame, re-check and
// re-render every node on the open Conquest map instead of only at construction time.
const UPDATE_FINDEX: usize = 28486; // ConquestMapContent.update(dt)
const CHANGE_STATE_FINDEX: usize = 28498; // MapButton.changeState(state, onDone)
const HAS_NEXT_TO_UNLOCK_FINDEX: usize = 28481; // ConquestMapContent.hasNextToUnlock -- source of the Conquest/MapContainer/enum<BattleState> reg types and the `conquest`/`container` field indices, all on the same class as `update`
const GET_BUTTON_FINDEX: usize = 28512; // MapContainer.getButton -- source of the ArrayObj/dynamic/array/MapButton reg types and the `buttons` field index
const GET_BUTTON_BY_ID_FINDEX: usize = 28513; // MapContainer.getButtonById -- source of the Battle/virtual/String reg types and the `data`/`infId` field indices

// Post-battle "reveal" animation fix: ConquestMapContent.animateNewBattlePlots hardcodes
// every next-column node to Unlocked (global(6241)) regardless of whether it's actually
// allowed -- see patch_reveal_uses_real_state below.
const ANIMATE_NEW_BATTLE_PLOTS_FINDEX: usize = 28476;
const ANIMATE_NEW_BATTLE_PLOTS_HARDCODED_UNLOCKED_OP: usize = 46;
const GLOBAL_UNLOCKED: usize = 6241;

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
        "status" => cmd_status(&live_path, &backup_path),
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

fn print_status_line(live_path: &Path, backup_path: &Path) {
    if !live_path.exists() {
        println!("Status: no hlboot.dat found here -- is this really the Northgard install folder?");
    } else if !backup_path.exists() {
        println!("Status: NOT patched (no backup present yet)");
    } else {
        match (fs::read(live_path), fs::read(backup_path)) {
            (Ok(live), Ok(backup)) if live == backup => println!("Status: NOT patched (matches the pristine backup)"),
            (Ok(_), Ok(_)) => println!("Status: PATCHED (Chapter locks are enforced in-game)"),
            _ => println!("Status: unknown (couldn't read one of the files)"),
        }
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
        print_status_line(&live_path, &backup_path);
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

fn cmd_status(live_path: &Path, backup_path: &Path) -> Result<()> {
    // Exit code is the machine-readable contract other tools (the Python client) rely on:
    // 0 = currently patched, 1 = not currently patched (apply needed), 2 = error (can't
    // tell at all). Stdout text is for humans only -- don't rely on it elsewhere.
    if !live_path.exists() {
        println!("No hlboot.dat found at {}", live_path.display());
        std::process::exit(2);
    }
    if !backup_path.exists() {
        println!("Not yet patched (no backup present at {}) -- run 'apply'.", backup_path.display());
        std::process::exit(1);
    }
    if fs::read(live_path)? == fs::read(backup_path)? {
        println!("hlboot.dat matches the pristine backup -- not currently patched.");
        std::process::exit(1);
    } else {
        println!("hlboot.dat differs from the pristine backup -- currently patched.");
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

fn cmd_apply(live_path: &Path, backup_path: &Path) -> Result<()> {
    if !live_path.exists() {
        bail!("No hlboot.dat found at {}", live_path.display());
    }
    if !backup_path.exists() {
        fs::copy(live_path, backup_path).context("failed to create pristine backup before patching")?;
        println!("Backed up pristine hlboot.dat to {}", backup_path.display());
    }

    let backup_str = backup_path.to_str().context("install dir path isn't valid UTF-8")?;
    let mut code = Bytecode::from_file(backup_str).context(
        "failed to parse hlboot.dat -- this Northgard build may not match what this tool \
         was written against; see docs/DEVELOPMENT.md before trusting this patch",
    )?;

    let config_dir = config_dir_path()?;
    let unlock_dir = config_dir.join(UNLOCK_SUBDIR);
    fs::create_dir_all(&unlock_dir).context("failed to create the unlock-marker directory")?;
    // Trailing separator so the game only needs to concatenate the battle id onto this.
    let marker_prefix = format!("{}\\", unlock_dir.display());
    let non_linear_flag_path = config_dir.join(NON_LINEAR_FLAG_NAME).display().to_string();

    patch_get_battle_state(&mut code, &marker_prefix, &non_linear_flag_path)?;
    patch_map_auto_refresh(&mut code)?;
    patch_reveal_uses_real_state(&mut code)?;

    let out = File::create(live_path).context("failed to open hlboot.dat for writing")?;
    let mut writer = BufWriter::new(out);
    code.serialize(&mut writer).context("failed to write patched bytecode")?;

    println!("Patched hlboot.dat installed at {}", live_path.display());
    println!("Marker directory: {unlock_dir_display}", unlock_dir_display = unlock_dir.display());
    println!("Non-linear-mode flag file (its mere presence skips the adjacency check): {non_linear_flag_path}");
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

fn patch_get_battle_state(code: &mut Bytecode, marker_prefix: &str, non_linear_flag_path: &str) -> Result<()> {
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
        .find(|f| f.findex.0 == STRING_ADD_FINDEX)
        .context("could not find String.__add__ -- Northgard build mismatch?")?;
    let field_length = match &add_fn.ops[7] {
        Opcode::Field { field, .. } => *field,
        other => bail!("__add__ op7 shape changed (expected Field), got {other:?} -- Northgard build mismatch?"),
    };
    let field_bytes = match &add_fn.ops[27] {
        Opcode::Field { field, .. } => *field,
        other => bail!("__add__ op27 shape changed (expected Field), got {other:?} -- Northgard build mismatch?"),
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
        .find(|f| f.findex.0 == GATE_FINDEX)
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
        /*14*/ Call1 { dst: reg_nl_pathbytes, fun: RefFun(GET_PATH_FINDEX), arg0: reg_nl_str },
        /*15*/ Call1 { dst: reg_nl_exists, fun: RefFun(SYS_EXISTS_FINDEX), arg0: reg_nl_pathbytes },
        /*16*/ JTrue { cond: reg_nl_exists, offset: 1 }, // -> 18 (skip adjacency gate)
        /*17*/ JFalse { cond: Reg(5), offset: 11 }, // -> 29 (Locked) if the game's own check already says no
        /*18*/ String { dst: reg_prefix_bytes, ptr: prefix_ref },
        /*19*/ Int { dst: reg_len, ptr: len_ref },
        /*20*/ New { dst: reg_prefix_str },
        /*21*/ SetField { obj: reg_prefix_str, field: field_bytes, src: reg_prefix_bytes },
        /*22*/ SetField { obj: reg_prefix_str, field: field_length, src: reg_len },
        /*23*/ Call2 { dst: reg_concat, fun: RefFun(STRING_ADD_FINDEX), arg0: reg_prefix_str, arg1: Reg(1) },
        /*24*/ Call1 { dst: reg_pathbytes, fun: RefFun(GET_PATH_FINDEX), arg0: reg_concat },
        /*25*/ Call1 { dst: reg_exists, fun: RefFun(SYS_EXISTS_FINDEX), arg0: reg_pathbytes },
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
fn patch_map_auto_refresh(code: &mut Bytecode) -> Result<()> {
    let conquest_ty = reg_type(code, HAS_NEXT_TO_UNLOCK_FINDEX, 3, "gamesys.conquest.Conquest")?;
    let map_container_ty = reg_type(code, HAS_NEXT_TO_UNLOCK_FINDEX, 14, "ui.menus.conquest.MapContainer")?;
    let battle_state_ty = reg_type(code, HAS_NEXT_TO_UNLOCK_FINDEX, 15, "enum<BattleState>")?;
    let array_obj_ty = reg_type(code, GET_BUTTON_FINDEX, 4, "hl.types.ArrayObj")?;
    let dynamic_ty = reg_type(code, GET_BUTTON_FINDEX, 8, "dynamic")?;
    let raw_array_ty = reg_type(code, GET_BUTTON_FINDEX, 9, "array")?;
    let map_button_ty = reg_type(code, GET_BUTTON_FINDEX, 11, "ui.menus.conquest.MapButton")?;
    let battle_ty = reg_type(code, GET_BUTTON_BY_ID_FINDEX, 14, "gamesys.conquest.Battle")?;
    let battle_data_ty = reg_type(code, GET_BUTTON_BY_ID_FINDEX, 13, "Battle.data virtual")?;
    let string_ty = reg_type(code, GET_BUTTON_BY_ID_FINDEX, 1, "String")?;
    let callback_ty = reg_type(code, CHANGE_STATE_FINDEX, 2, "() -> void callback")?;

    let f = code
        .functions
        .iter_mut()
        .find(|f| f.findex.0 == UPDATE_FINDEX)
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
        /*36*/ Call2 { dst: r_new_state, fun: RefFun(GATE_FINDEX), arg0: r_conquest, arg1: r_infid },
        /*37*/ Null { dst: r_null_cb },
        /*38*/ Call3 { dst: void_reg, fun: RefFun(CHANGE_STATE_FINDEX), arg0: r_button, arg1: r_new_state, arg2: r_null_cb },
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
fn patch_reveal_uses_real_state(code: &mut Bytecode) -> Result<()> {
    let f = code
        .functions
        .iter_mut()
        .find(|f| f.findex.0 == ANIMATE_NEW_BATTLE_PLOTS_FINDEX)
        .context("could not find ConquestMapContent.animateNewBattlePlots -- Northgard build mismatch?")?;

    let op = f
        .ops
        .get_mut(ANIMATE_NEW_BATTLE_PLOTS_HARDCODED_UNLOCKED_OP)
        .context("animateNewBattlePlots is shorter than expected -- Northgard build mismatch?")?;
    match op {
        Opcode::GetGlobal { dst, global } if global.0 == GLOBAL_UNLOCKED => {
            let dst = *dst;
            *op = Opcode::Call2 { dst, fun: RefFun(GATE_FINDEX), arg0: Reg(6), arg1: Reg(4) };
        }
        other => bail!(
            "animateNewBattlePlots op{ANIMATE_NEW_BATTLE_PLOTS_HARDCODED_UNLOCKED_OP} doesn't \
             match the expected hardcoded GetGlobal(Unlocked) (got {other:?}) -- Northgard build \
             mismatch? Re-verify with `dump` before trusting this patch."
        ),
    }

    Ok(())
}
