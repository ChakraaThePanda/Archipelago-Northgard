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
use hlbc::types::{Reg, RefFun, RefInt, RefString};
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
