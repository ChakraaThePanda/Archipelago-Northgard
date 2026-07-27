"""
Dev/test helper: simulates winning the next Conquest battle in a save file, without
actually playing it, so you can confirm the client's polling + check-sending pipeline
works end-to-end. The running Northgard Client re-reads its pinned save unconditionally
every 5s (see POLL_INTERVAL_SECONDS in NorthgardClient.py) -- it doesn't care whether a
change came from playing the game or from editing the file by hand, so this script's
edit is picked up exactly the same way a real win would be.

Usage:
    python simulate_chapter_win.py <path-to-save.sav> [--branch top|bottom]
                                    [--bonus-index N] [--dry-run]

Run this while the client is running and pinned to that same save (`/conquest` in the
client), then watch its log -- within POLL_INTERVAL_SECONDS you should see
"Completed: [...] -> sending N checks".

IMPORTANT -- this also picks a reward bonus for you, same as a real win would:
Winning a node doesn't just advance `path`; Northgard separately records a
`chosenBonuses` entry in `progress` for that node (one bonus per player). On the two
single-node rows (Chapter 01, Chapter 04, Chapter 07) there's only ever one possible
bonus, so this is a no-op formality. On the four branching rows (Chapter 02/03/05/06,
Top or Bottom) each player is normally offered a REAL choice of 3 bonuses -- for
simulation purposes this script just picks one at random (logged, so you can see what
you got) rather than making you choose. Pass --bonus-index if you want a specific one
instead.

Safety: the very first time this script touches a given .sav, it makes a sibling
"<name>.sav.bak" backup before writing (never overwritten on later runs against the same
file, so it always holds the pristine pre-simulation state). Restore with:
    copy save.sav.bak save.sav
"""
from __future__ import annotations

import argparse
import os
import random
import shutil
import sys

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "worlds", "northgard"))
from haxe_serializer import decode, encode  # noqa: E402  (sys.path must be set first)

# Must match CHAPTER_ROWS in worlds/northgard/save_state.py / Regions.py exactly.
CHAPTER_ROWS: list[list[str]] = [
    ["Chapter 01"],
    ["Chapter 02 - Top", "Chapter 02 - Bottom"],
    ["Chapter 03 - Top", "Chapter 03 - Bottom"],
    ["Chapter 04"],
    ["Chapter 05 - Top", "Chapter 05 - Bottom"],
    ["Chapter 06 - Top", "Chapter 06 - Bottom"],
    ["Chapter 07"],
]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("save_path", help="Path to the .sav file to edit")
    parser.add_argument("--branch", choices=["top", "bottom"], default="top",
                         help="Which node to win when the next row forks (default: top)")
    parser.add_argument("--bonus-index", type=int, default=None,
                         help="Which offered bonus (0-based) to grant each player, when the node "
                              "offers a real choice. Omit to pick one at random (fine for testing).")
    parser.add_argument("--dry-run", action="store_true", help="Print what would happen without writing anything")
    args = parser.parse_args()

    if not os.path.exists(args.save_path):
        print(f"No such file: {args.save_path}", file=sys.stderr)
        return 1

    with open(args.save_path, "r", encoding="utf-8") as f:
        original_text = f.read()
    obj = decode(original_text)

    map_rows = obj["map"]
    path: list = obj["path"]
    depth = len(path)

    if depth >= len(map_rows):
        print(f"Already at the final row ({len(path)} chapters completed) -- nothing left to simulate.")
        return 0

    row = map_rows[depth]
    branch_index = 0 if (args.branch == "top" or len(row) == 1) else 1
    if branch_index >= len(row):
        branch_index = 0
    next_cell = row[branch_index]
    next_inf_id = next_cell["infId"]
    chapter_name = CHAPTER_ROWS[depth][branch_index] if depth < len(CHAPTER_ROWS) else f"<row {depth}>"

    print(f"Next win: row {depth} -> {next_inf_id!r} ({chapter_name}, {args.branch} branch)")

    # Build the matching `progress` entry a real win+bonus-pick would produce. Northgard
    # tracks this separately from `path` -- without it, `path` and `progress` would go out
    # of sync in a way that never happens during real play, so how the game reacts to
    # loading that is untested territory.
    players = obj.get("players", [])
    available = next_cell.get("availableBonuses", [])
    has_real_choice = any(len(options) > 1 for options in available)

    chosen_bonuses = []
    for i, player in enumerate(players):
        options = available[i] if i < len(available) else []
        if not options:
            continue
        index = args.bonus_index if args.bonus_index is not None else random.randrange(len(options))
        chosen_bonuses.append({
            "bonus": options[min(index, len(options) - 1)],
            "uid": player.get("uid"),
        })

    if has_real_choice:
        picks = ", ".join(f"{players[i]['name']}={c['bonus']['id']}" for i, c in enumerate(chosen_bonuses))
        picked_how = "fixed --bonus-index" if args.bonus_index is not None else "random, for testing"
        print(f"This node offers a real bonus choice -- granting one ({picked_how}) to each player: {picks}")

    if args.dry_run:
        print("--dry-run: not writing anything.")
        return 0

    obj["path"].append(next_inf_id)
    obj.setdefault("progress", []).append({"id": next_inf_id, "chosenBonuses": chosen_bonuses})
    new_text = encode(obj)

    # Round-trip sanity check before touching the real file.
    reparsed = decode(new_text)
    if reparsed["path"] != obj["path"] or reparsed["progress"] != obj["progress"]:
        print("Round-trip check failed -- refusing to write a possibly-corrupt save.", file=sys.stderr)
        return 1

    backup_path = args.save_path + ".bak"
    if not os.path.exists(backup_path):
        shutil.copyfile(args.save_path, backup_path)
        print(f"Backed up pristine save to {backup_path}")

    with open(args.save_path, "w", encoding="utf-8") as f:
        f.write(new_text)

    print(f"Wrote {chapter_name} as completed to {args.save_path}")
    print("If the client is running and pinned to this save, it should send that chapter's "
          "checks within a few seconds.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
