# Archipelago-Northgard

Goal: complete Chapter 07 of a Conquest run. Each Chapter you finish sends location
checks; each "Chapter N [- Top/Bottom]" item you receive from the multiworld is
*supposed* to unlock that node in Northgard's own UI (not implemented yet -- see Status).

---

## How it works

### The apworld (`worlds/northgard/`)
- **Items**: one unlock token per Chapter position (`Chapter 01`, `Chapter 02 - Top`,
  `Chapter 02 - Bottom`, ... `Chapter 07` -- 11 total), plus a `Krown` filler. `Chapter 01`
  is always a starting item.
- **Locations**: up to 10 per Chapter (`Chapter 01 - Item 01` .. `Item 10`), configurable in the YAML options.
- **Regions**: mirror Northgard's actual fixed Conquest tree -- `Chapter 01` branches to a
  Top/Bottom pair, each of those to another pair or a single Chapter, etc. Entering a
  Chapter's region requires having received that Chapter's item.
- **Victory**: completing Chapter 07.

### The save format
Northgard's `.sav` files are **Haxe's `haxe.Serializer`/`Unserializer` text format** (the
game is built on Haxe/Heaps/Hashlink). `haxe_serializer.py` is a from-scratch decoder +
encoder for it, verified to fully parse real save files with zero unhandled tags, and to
round-trip exactly (`decode(encode(x)) == x`). Two things worth remembering if you touch
this file again:
- Several top-level fields (`$INV`, `$ACH`, `$SID` in `udat_*.sav`) are **double-serialized**
  -- their decoded value is itself a string that needs a second `decode()` call.
- The encoder always rebuilds string/object cache indices from scratch in traversal
  order. That's valid (Unserializer doesn't care about specific index numbers, only that
  references resolve), but it means re-encoded output is never byte-identical to an
  original file -- verify by round-tripping, not by diffing bytes.

### Chapter-completion detection (`save_state.py`)
A per-run save in `save/conquest/*.sav` decodes to a dict with a `map` grid (one row per
tree depth -- confirmed `[1, 2, 2, 1, 2, 2, 1]`, an exact match to the 7-Chapter/Top-Bottom
tree) and a `path` list (ordered battle names, one appended per win). `path[i]` maps
directly to tree row `i`; comparing the infId against that row's two possible node names
tells you Top vs. Bottom. This was confirmed correct against a live, in-progress save.

### The client (`NorthgardClient.py`)
Ships **inside** the apworld zip (like Manual's `ManualClient.py`) and registers itself
via `worlds.LauncherComponents`, so it shows up as "Northgard Client" in the Archipelago
Launcher -- it cannot run as a loose standalone script, because the real install
(`D:\Games\Archipelago`) is a compiled release with no loose `CommonClient.py` to import.

- Polls the pinned save every 5s **unconditionally** -- it re-reads and re-diffs the save
  on every tick regardless of *how* the file changed, so a real win and a hand/script edit
  (see `tools/simulate_chapter_win.py` below) are indistinguishable to it. When new
  Chapters show up completed, sends `LocationChecks` for that Chapter's `Item 01..N`
  locations all at once, then a `CLIENT_GOAL` status update once Chapter 07 is done.
- **Client config** (`save_dir` + room pins) lives at
  `%UserProfile%\Saved Games\Archipelago\Northgard\client_config.json` -- resolved via the
  real Windows known-folder API (`SHGetKnownFolderPath`), so it's still correct even if
  you've redirected "Saved Games" to another drive. Deliberately *not* inside Northgard's
  own install: it has to survive a Northgard reinstall/relocation (see `/savedir` below),
  and Northgard's own `save/` folder is where Steam Cloud sync operates, which isn't
  somewhere to drop unrelated app state. A one-time migration picks up anything from the
  old pre-this-change location (`%UserProfile%\.northgard_ap_client_pin.json`) if found.
- **`/savedir` command**: this machine's Northgard `save` folder (every install lands
  somewhere different -- different drive, different Steam library, non-Steam copy -- so
  it's never hardcoded). Resolved automatically on first launch, in order: previously
  configured -> Steam-library auto-detection (reads the install path from the registry,
  then every library folder from that install's `libraryfolders.vdf`, and checks each for
  `steamapps/common/Northgard/save`) -> a native folder-picker dialog as a last resort.
  Run `/savedir` any time to see the current path, re-open the picker, or `/savedir <path>`
  to set it directly. Machine-level setting, not per-room -- set once, every room reuses
  it.
- **`/conquest` command**: there is no shared ID between an Archipelago room and a
  Northgard Conquest run's internal seed, so which save file belongs to a given
  playthrough can never be inferred automatically -- especially with several Conquests
  in flight (confirmed 8+ real ones present at once during dev). Run `/conquest` with no
  argument to list them; `/conquest 2` to pin one. The pin is remembered **per
  Archipelago room**, keyed by the room's `seed_name` (a unique id the server sends once
  per generated multiworld, unrelated to Northgard's own Conquest seed) -- so running two
  rooms at once, each in its own Launcher/client window, each remembers its own pinned
  save independently instead of sharing one global "last used" slot.
  - Each listed save shows clan(s), chapters completed, and when it was last written
    (`"64s ago"`, `"8m ago"`, `"00:13 today"`, or a full date once it's old) -- useful
    since loading a save in Northgard doesn't touch its timestamp, only an actual write
    does (a battle win, a perk pick), so two saves born seconds apart look identical
    until one of them progresses. There is no reliable signal anywhere (file lock, OS
    access time, save contents) for "which save is currently open in-game" independent of
    that -- confirmed by directly testing for one during dev, which is also why this
    stays a manual pin rather than something auto-detected.
  - Conquest is **co-op against the AI**, not PvP -- a second clan shown is a teammate
    (`"Wolf & Bear"`), not an opponent. Solo runs show just the one clan, no dangling
    `"vs ?"`.
- **Received items currently do nothing in-game** -- see Status.
- **`tools/simulate_chapter_win.py`**: dev/test helper, not part of the apworld. Appends
  the next Conquest node as "won" directly in a save file (backing up the pristine file
  first), so you can prove the whole detect -> send-checks pipeline works without playing
  a real game out. Usage: `python tools/simulate_chapter_win.py <path-to-save.sav>
  [--branch top|bottom] [--dry-run]`. Run it while the client is pinned to that save and
  watch its log -- checks should send within ~5s.

---

## Setup / how to run this

Paths below are this machine's actual locations.

**Source**: `D:\Documents\Github\Archipelago-Northgard`
**Real Archipelago install**: `D:\Games\Archipelago` (compiled release, not a source checkout)
**Real Northgard saves**: `D:\Games\Steam\steamapps\common\Northgard\save`

1. **Build the apworld zip** (re-run this any time you edit files under `worlds/northgard/`;
   `tools/` is dev-only and deliberately excluded from the zip):
   ```python
   import zipfile, os
   src_root = r"D:\Documents\Github\Archipelago-Northgard\worlds\northgard"
   dest = r"D:\Games\Archipelago\custom_worlds\northgard.apworld"
   with zipfile.ZipFile(dest, "w", zipfile.ZIP_DEFLATED) as z:
       for dirpath, dirnames, filenames in os.walk(src_root):
           dirnames[:] = [d for d in dirnames if d not in ("__pycache__", "tools")]
           for fn in filenames:
               if fn.endswith(".pyc"):
                   continue
               full = os.path.join(dirpath, fn)
               z.write(full, os.path.join("northgard", os.path.relpath(full, src_root)))
   ```
2. It's already sitting in `D:\Games\Archipelago\custom_worlds\northgard.apworld`.
3. Drop your YAML(s) in `D:\Games\Archipelago\Players\` like any other game. (Earlier dev
   testing used an isolated `Players\NorthgardApworldTest\` subfolder to avoid mixing into
   real rooms -- keep doing that for throwaway test generations if you don't want them
   picked up alongside your real ones.)
4. Generate:
   ```
   cd D:\Games\Archipelago
   .\ArchipelagoGenerate.exe --player_files_path "Players" --seed 1
   ```
5. Host the output `.zip` (via `ArchipelagoServer.exe` or however you normally host).
6. Open the Archipelago Launcher, click "Northgard Client" (after closing/reopening the
   Launcher so it picks up the newly-installed apworld -- it only scans `custom_worlds/`
   at its own startup).
7. Connect with server address / slot name / password like any client.
8. First time on this machine: it auto-detects your Northgard `save` folder (or opens a
   folder-picker if it can't). Use `/savedir` any time to check or change it.
9. Run `/conquest` to see your in-progress Conquest saves and pin the right one. This pin
   is remembered for *this specific room* -- running a second room at the same time (a
   second Launcher/client window) keeps its own pin separately, so they won't collide.
10. Play normally. Checks send automatically. To test without playing a full run, see
    `tools/simulate_chapter_win.py` above.

---

## Status

**Verified, not just written:**
- Haxe-Serializer codec: full decode of `udat_*.sav` and per-run `conquest/*.sav`, zero
  unhandled tags, exact round-trip confirmed on real files.
- Chapter-completion detection: confirmed correct against a live, in-progress save.
- The apworld: generated multiple times via `ArchipelagoGenerate.exe` against the real
  install, zero errors, correct item/location counts, spoiler log shows a fully solvable
  7-sphere playthrough ending in `Chapter 07 - Victory: Victory`.
- Client packaging: moved inside the apworld + Launcher-component registration (the
  original standalone-script design would never have run against the real compiled
  install). Re-verified generation still passes with this structure in place.
- Save disambiguation (`/conquest`): listing tested against all 8 real concurrent
  Conquest saves on this machine -- correct clan pairs, difficulty, progress for each.
- **Full live end-to-end test, done**: hosted a real generated room, launched the actual
  "Northgard Client" component (headless `--nogui` mode) from the real
  `D:\Games\Archipelago` install, connected, confirmed `RoomInfo`'s `seed_name` is what
  `/conquest`'s per-room pin is keyed on, pinned a save, then used
  `tools/simulate_chapter_win.py` to append wins to that save with the client already
  running -- it detected them on its normal 5s poll and sent the correct location count
  (`amount_of_locations: 4` x chapters completed) with no manual intervention. Real
  Conquest saves were never touched -- testing used a throwaway copy under a distinct
  filename, deleted afterward.
- Fixed a real bug found during that test: `launch_client()` in `__init__.py` took zero
  arguments, but the Launcher's `Component.run()` always calls `self.func(*args)` --
  passing any args at all (a url, extra CLI tokens) crashed it with a `TypeError` before
  the client even opened. Now accepts `*args` (unused, matching Manual's own client, which
  has the same shape) so it can't crash regardless of how it's invoked.
- Save-folder auto-detection (`/savedir`): registry + `libraryfolders.vdf` parsing tested
  against this machine's real, non-default Steam library (`D:\Games\Steam`, not the
  default `C:\Program Files (x86)\Steam`) -- correctly found it without being told.

**Not yet tested (real gaps):**
- Only tried `amount_of_locations: 4`, seed `1`, solo generation. Other option values,
  seeds, and multi-game/multi-player rooms untested.
- No `manifest.json` -- currently just a future-compatibility warning ("will stop working
  with Archipelago 0.7.0"), not a current blocker. Your existing Manual apworld has the
  same gap, so this isn't new.
- The folder-picker fallback path in `/savedir` / first-launch auto-detection (used only
  when Steam-library detection fails) exercises `Utils.open_directory`, a tkinter dialog --
  not exercised in the headless `--nogui` test above (this machine's real Steam library was
  found automatically, and `--nogui` mode doesn't launch inside a GUI display). It's the
  same mechanism the Launcher's own "Install APWorld" button already relies on, so it's a
  known-working pattern in this exact environment, but worth a manual click-through once.

**Not resolved at all -- lock/gate enforcement:**
The intent was: receiving "Chapter 02 - Top" should make that node selectable in
Northgard's own Conquest UI; un-received Chapters should stay unselectable. Still unknown
whether the node-select screen reads a writable per-node "unlocked" flag from the save, or
purely computes selectability from "adjacent to an already-completed node" (no flag to
control -- in which case a hard in-game lock isn't achievable through save-editing alone,
and the fallback is a softer reminder/overlay instead). `NorthgardClient.py`'s
`ReceivedItems` handler is stubbed and currently only logs what it received -- it does not
touch the game state.

---

## What's left to do (next session)

Roughly in the order it makes sense to tackle them:

1. **Resolve the lock-enforcement question.** Needs either: (a) diffing a conquest save
   at the exact moment of a branch choice (just beat the parent, haven't picked Top or
   Bottom yet) against one from after picking, to look for a controllable flag, or (b)
   deciding to ship v1 with no hard lock and figure out a softer alternative
   (client-side reminder of what's "really" unlocked, honor system).
2. **Only once lock enforcement is resolved**: implement the actual write-back in
   `ReceivedItems` -- writing the unlock into the save (with automatic backups before
   every write, given how delicate the Haxe-Serializer round-trip is).
3. **Cleanup/polish**: add a `manifest.json` to the apworld; test other
   `amount_of_locations` values and a multi-player room; a manual click-through of the
   `/savedir` folder-picker path (GUI dialog, not exercised by the headless test); consider
   whether `NorthgardWeb`'s tutorial doc needs updating now that the real setup flow is
   proven out.
