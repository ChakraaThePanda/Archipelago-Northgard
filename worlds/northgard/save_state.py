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
    opponent_clan: str
    completed_chapters: list[str]  # in tree order, e.g. ["Chapter 01", "Chapter 02 - Bottom", ...]


@dataclass
class ConquestSaveSummary:
    """Enough info to show a human which save is which, to disambiguate between
    multiple in-progress Conquest runs -- there is no shared ID between an Archipelago
    room and a Northgard Conquest run, so this can never be inferred automatically."""
    filename: str  # stable for the life of a run: "<Clan1>-<Clan2>-<difficulty>-<seed>.sav"
    path: str
    clan: str
    opponent_clan: str
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
            opponent = players[1]["clan"] if len(players) > 1 else "?"
            summaries.append(ConquestSaveSummary(
                filename=os.path.basename(path),
                path=path,
                clan=clan,
                opponent_clan=opponent,
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


def read_conquest_run_state(save_file: str) -> ConquestRunState:
    with open(save_file, "r", encoding="utf-8") as f:
        text = f.read()
    obj = decode(text)

    map_rows = _row_node_names(obj["map"])
    path: list[str] = obj["path"]

    if len(map_rows) != len(CHAPTER_ROWS):
        raise ValueError(
            f"Unexpected conquest map shape {[len(r) for r in map_rows]} in {save_file!r}; "
            f"expected {[len(r) for r in CHAPTER_ROWS]}. The tree layout may have changed -- "
            f"do not assume path-index mapping is still valid until this is re-checked."
        )

    completed: list[str] = []
    for depth, inf_id in enumerate(path):
        row_names = map_rows[depth]
        chapter_names = CHAPTER_ROWS[depth]
        if inf_id not in row_names:
            raise ValueError(
                f"path[{depth}]={inf_id!r} not found in map row {depth} ({row_names!r}) of {save_file!r}"
            )
        completed.append(chapter_names[row_names.index(inf_id)])

    players = obj.get("players", [])
    clan = players[0]["clan"] if players else obj.get("clan", "?")
    opponent = players[1]["clan"] if len(players) > 1 else "?"

    return ConquestRunState(
        save_path=save_file,
        clan=clan,
        opponent_clan=opponent,
        completed_chapters=completed,
    )
