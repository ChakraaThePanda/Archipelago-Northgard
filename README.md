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

- Polls the pinned save every 5s; when new Chapters show up completed, sends
  `LocationChecks` for that Chapter's `Item 01..N` locations all at once, then a
  `CLIENT_GOAL` status update once Chapter 07 is done.
- **`/conquest` command**: there is no shared ID between an Archipelago room and a
  Northgard Conquest run's internal seed, so which save file belongs to a given
  playthrough can never be inferred automatically -- especially with several Conquests
  in flight (confirmed 8 real ones present at once during dev). Run `/conquest` with no
  argument to list them (clan pair, difficulty, chapters completed so far); `/conquest 2`
  to pin one. The choice is remembered across restarts, keyed by filename (stable for a
  run's whole lifetime -- clan pair + difficulty + seed never change once a Conquest
  starts).
- **Received items currently do nothing in-game** -- see Status.

---

## Setup / how to run this

Paths below are this machine's actual locations.

**Source**: `D:\Documents\Github\Archipelago-Northgard`
**Real Archipelago install**: `D:\Games\Archipelago` (compiled release, not a source checkout)
**Real Northgard saves**: `D:\Games\Steam\steamapps\common\Northgard\save`

1. **Build the apworld zip** (re-run this any time you edit files under `worlds/northgard/`):
   ```python
   import zipfile, os
   src_root = r"D:\Documents\Github\Archipelago-Northgard\worlds\northgard"
   dest = r"D:\Games\Archipelago\custom_worlds\northgard.apworld"
   with zipfile.ZipFile(dest, "w", zipfile.ZIP_DEFLATED) as z:
       for dirpath, dirnames, filenames in os.walk(src_root):
           dirnames[:] = [d for d in dirnames if d != "__pycache__"]
           for fn in filenames:
               if fn.endswith(".pyc"):
                   continue
               full = os.path.join(dirpath, fn)
               z.write(full, os.path.join("northgard", os.path.relpath(full, src_root)))
   ```
2. It's already sitting in `D:\Games\Archipelago\custom_worlds\northgard.apworld`.
3. A test YAML already exists at `D:\Games\Archipelago\Players\NorthgardApworldTest\northgard_test.yaml`
   -- deliberately in its own subfolder, not mixed into your real "Chaos Async"/"Mixmania"
   room folders, so it never gets swept into a real generation by accident.
4. Generate (isolated test, doesn't touch real rooms):
   ```
   cd D:\Games\Archipelago
   .\ArchipelagoGenerate.exe --player_files_path "Players\NorthgardApworldTest" --seed 1
   ```
5. Host the output `.zip` (via `ArchipelagoServer.exe` or however you normally host).
6. Open the Archipelago Launcher, click "Northgard Client" (after closing/reopening the
   Launcher so it picks up the newly-installed apworld -- it only scans `custom_worlds/`
   at its own startup).
7. Connect with server address / slot name / password like any client.
8. Run `/conquest` to see your in-progress Conquest saves and pin the right one.
9. Play normally. Checks send automatically.

---

## Status

**Verified, not just written:**
- Haxe-Serializer codec: full decode of `udat_*.sav` and per-run `conquest/*.sav`, zero
  unhandled tags, exact round-trip confirmed on real files.
- Chapter-completion detection: confirmed correct against a live, in-progress save.
- The apworld: generated twice via `ArchipelagoGenerate.exe` against the real install,
  zero errors, correct item/location counts, spoiler log shows a fully solvable 7-sphere
  playthrough ending in `Chapter 07 - Victory: Victory`.
- Client packaging: moved inside the apworld + Launcher-component registration (the
  original standalone-script design would never have run against the real compiled
  install). Re-verified generation still passes with this structure in place.
- Save disambiguation (`/conquest`): listing tested against all 8 real concurrent
  Conquest saves on this machine -- correct clan pairs, difficulty, progress for each.

**Not yet tested (real gaps):**
- Never actually launched the client -- no observed proof "Northgard Client" appears in
  the Launcher's UI, and no live connect/check-send/receive test against a real server.
- Only tried `amount_of_locations: 4`, seed `1`, solo generation. Other option values,
  seeds, and multi-game/multi-player rooms untested.
- No `manifest.json` -- currently just a future-compatibility warning ("will stop working
  with Archipelago 0.7.0"), not a current blocker. Your existing Manual apworld has the
  same gap, so this isn't new.

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

1. **Actually launch the client for the first time.** Close and reopen the Archipelago
   Launcher, confirm "Northgard Client" appears in the list, click it, see what happens.
2. **Live end-to-end test.** Host the generated seed on a real (or local test) server,
   connect the client, use `/conquest` to pin one of your real in-progress saves, and
   confirm a check actually lands server-side when a Chapter completes.
3. **Resolve the lock-enforcement question.** Needs either: (a) diffing a conquest save
   at the exact moment of a branch choice (just beat the parent, haven't picked Top or
   Bottom yet) against one from after picking, to look for a controllable flag, or (b)
   deciding to ship v1 with no hard lock and figure out a softer alternative
   (client-side reminder of what's "really" unlocked, honor system).
4. **Only once lock enforcement is resolved**: implement the actual write-back in
   `ReceivedItems` -- writing the unlock into the save (with automatic backups before
   every write, given how delicate the Haxe-Serializer round-trip is).
5. **Cleanup/polish**: add a `manifest.json` to the apworld; test other
   `amount_of_locations` values and a multi-player room; consider whether `NorthgardWeb`'s
   tutorial doc needs updating once the real setup flow is proven out.
