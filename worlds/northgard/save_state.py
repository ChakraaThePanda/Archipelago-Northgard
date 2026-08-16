"""
Reads Northgard's Conquest-mode save state to figure out which "Chapter" (in the sense
of Archipelago-Manual-Northgard's region tree) the player has most recently completed.

Confirmed against a real save (see project notes): a per-run file in save/conquest/*.sav
decodes (via haxe_serializer) to a dict with, among others:
    'map':  a jagged grid, one row per tree depth, e.g. dims [1, 2, 2, 1, 2, 2, 1] --
            exactly matching the 7-Chapter / Top-Bottom tree already modeled in the
            Manual project. Each cell is {'infId': <map name>, 'template': ..., ...}.
    'path': ordered list of infId strings, one appended each time a battle is won,
            in the same order as tree depth (path[0] is always the row-0 battle, etc).

CHAPTER_ROWS below encodes the tree exactly as Regions.py does, in row order, so
path index -> row index -> Chapter name is a direct lookup.
"""
from __future__ import annotations

import glob
import os
from dataclasses import dataclass

from .haxe_serializer import decode

# Row-by-row, matching worlds/northgard/Regions.py's CHAPTER_CONNECTIONS exactly.
# A row with one name is a single Chapter; a row with two is [Top, Bottom].
CHAPTER_ROWS: list[list[str]] = [
    ["Chapter 01"],
    ["Chapter 02 - Top", "Chapter 02 - Bottom"],
    ["Chapter 03 - Top", "Chapter 03 - Bottom"],
    ["Chapter 04"],
    ["Chapter 05 - Top", "Chapter 05 - Bottom"],
    ["Chapter 06 - Top", "Chapter 06 - Bottom"],
    ["Chapter 07"],
]


@dataclass
class ConquestRunState:
    save_path: str
    clan: str
    partner_clan: str | None  # Conquest is co-op against the AI, not PvP -- None for a solo run
    completed_chapters: list[str]  # in tree order, e.g. ["Chapter 01", "Chapter 02 - Bottom", ...]
    map_rows: list[list[str]]  # this save's own randomized infId grid -- see infid_in_map.
    # Kept here (rather than re-decoding the save) so a single read_conquest_run_state call
    # per poll tick can serve both check-sending and infid_in_map lookups.


@dataclass
class ConquestSaveSummary:
    """Enough info to show a human which save is which, to disambiguate between
    multiple in-progress Conquest runs -- there is no shared ID between an Archipelago
    room and a Northgard Conquest run, so this can never be inferred automatically."""
    filename: str  # stable for the life of a run: "<Clan1>-<Clan2>-<difficulty>-<seed>.sav"
    path: str
    clan: str
    partner_clan: str | None  # Conquest is co-op against the AI, not PvP -- None for a solo run
    difficulty: int
    seed: int
    chapters_completed: int
    last_modified: float


def list_conquest_saves(northgard_save_dir: str) -> list[ConquestSaveSummary]:
    """All readable saves in save/conquest/, most recently played first."""
    pattern = os.path.join(northgard_save_dir, "conquest", "*.sav")
    summaries: list[ConquestSaveSummary] = []
    for path in glob.glob(pattern):
        try:
            with open(path, "r", encoding="utf-8") as f:
                obj = decode(f.read())
            players = obj.get("players", [])
            clan = players[0]["clan"] if players else obj.get("clan", "?")
            partner = players[1]["clan"] if len(players) > 1 else None
            summaries.append(ConquestSaveSummary(
                filename=os.path.basename(path),
                path=path,
                clan=clan,
                partner_clan=partner,
                difficulty=obj.get("difficulty", -1),
                seed=obj.get("seed", -1),
                chapters_completed=len(obj.get("path", [])),
                last_modified=os.path.getmtime(path),
            ))
        except Exception:
            continue  # unreadable/unexpected file -- skip rather than fail the whole listing
    summaries.sort(key=lambda s: s.last_modified, reverse=True)
    return summaries


def _row_node_names(map_grid: list) -> list[list[str]]:
    rows: list[list[str]] = []
    for row in map_grid:
        rows.append([cell["infId"] for cell in row])
    return rows


def all_infids(save_file: str) -> set[str]:
    """Every battle id this specific save's map randomized, across every tree position --
    used to scope marker-file cleanup to exactly one save when a room gets re-pinned to a
    different one, without touching a concurrent second room/save's own markers."""
    with open(save_file, "r", encoding="utf-8") as f:
        obj = decode(f.read())
    return {name for row in _row_node_names(obj["map"]) for name in row}


def infid_in_map(map_rows: list[list[str]], chapter_name: str, save_file: str = "<given map>") -> str:
    """Resolves a Chapter name to its battle id within an already-decoded map_rows (see
    read_conquest_run_state) -- no file I/O. save_file is only used to make error messages
    identify which save's map was being checked."""
    for depth, chapter_names in enumerate(CHAPTER_ROWS):
        if chapter_name not in chapter_names:
            continue
        col = chapter_names.index(chapter_name)
        if depth >= len(map_rows) or col >= len(map_rows[depth]):
            raise ValueError(
                f"Conquest map in {save_file!r} doesn't have a cell at row {depth} col {col} "
                f"for {chapter_name!r} -- map shape may have changed."
            )
        return map_rows[depth][col]

    raise ValueError(f"{chapter_name!r} is not a recognized Chapter name")


def read_conquest_run_state(save_file: str) -> ConquestRunState:
    with open(save_file, "r", encoding="utf-8") as f:
        text = f.read()
    obj = decode(text)

    map_rows = _row_node_names(obj["map"])
    path: list[str] = list(obj["path"])

    # Northgard's own Conquest.onBattleCompleted only appends a won battle's id to `path`
    # if no OTHER node in the same tree row already has an entry there -- a no-op guard in
    # vanilla Linear Mode (only one sibling per row is ever winnable) but one that silently
    # drops the SECOND sibling's win once Non-Linear Mode lets both be won, regardless of how
    # that second battle was won. `finishedBattleId` records the most recently finished
    # battle independently of that bug (the game only clears it once its own UI has consumed
    # it), so treat it as an authoritative addition to `path` whenever it names a battle not
    # already recorded there -- this self-heals the gap for already-affected saves with no
    # manual editing needed. patch_northgard's `onBattleCompleted` patch fixes the root cause
    # going forward; this covers saves that hit the bug before that patch was applied.
    finished_id = obj.get("finishedBattleId")
    if finished_id and finished_id not in path:
        path.append(finished_id)

    if len(map_rows) != len(CHAPTER_ROWS):
        raise ValueError(
            f"Unexpected conquest map shape {[len(r) for r in map_rows]} in {save_file!r}; "
            f"expected {[len(r) for r in CHAPTER_ROWS]}. The tree layout may have changed -- "
            f"do not assume path-index mapping is still valid until this is re-checked."
        )

    # Matched by content (which row/col this infId actually belongs to), not by position
    # in path -- path[i] is NOT guaranteed to be tree row i. In Non-Linear Mode a player can
    # win both the Top and Bottom battle of the same row (nothing stops them once both show
    # Unlocked), which appends two entries for what's structurally one row; a positional
    # depth->row assumption would then misalign every row after that one.
    completed: list[str] = []
    for inf_id in path:
        for row_names, chapter_names in zip(map_rows, CHAPTER_ROWS):
            if inf_id in row_names:
                completed.append(chapter_names[row_names.index(inf_id)])
                break
        else:
            raise ValueError(f"path entry {inf_id!r} not found in any map row of {save_file!r}")

    players = obj.get("players", [])
    clan = players[0]["clan"] if players else obj.get("clan", "?")
    partner = players[1]["clan"] if len(players) > 1 else None

    return ConquestRunState(
        save_path=save_file,
        clan=clan,
        partner_clan=partner,
        completed_chapters=completed,
        map_rows=map_rows,
    )
