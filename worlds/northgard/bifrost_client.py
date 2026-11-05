"""Bifrost-mode half of NorthgardClient.py: keeps the patched game's marker files in sync with
this slot's items and checked locations, and turns the game's "done" markers into checks.

The game side lives in tools/patch_northgard/src/bifrost.rs; the marker protocol is documented
there. In short, under <Saved Games>\\Archipelago\\Northgard\\bifrost\\ this client WRITES:
  active.flag             Bifrost mode is active (patches stay vanilla without it)
  realm_<Realm>           realm unlocked (Midgard always, the others by item)
  loc_<Realm>             realm still has an unchecked location (realm offer priority)
  obj_<Realm>_<ObjId>     objective still has an unchecked location there (spawn priority)
  card_<Realm>_<Screen>_<K>  perk card position K of that screen is an unchecked location
  bonus_<Key>_<N>         the slot owns at least N copies of that bonus item
  scout_<Realm>_<Screen>_<K>.txt  tooltip text for that AP card: the item it holds
  ap_card.png             the Archipelago logo the game shows on AP cards
and READS + deletes, under bifrost\\done\\, whatever the game wrote: obj_..., realm_..., card_...,
and seen_<Realm>_<Screen>_<K> (an AP card was shown on a perk screen: hint its item).

Every state marker is rewritten from this slot's own state (seed:team:slot) on every poll, and
stray ones are removed, so switching rooms or slots can never leak unlocks between them. Done
markers are kept until their own slot sends them: progress made while the client was closed is
sent on the next connect to that slot, and set aside if a different room connects first (see
switch_room_records).
"""
from __future__ import annotations

import os
import time
from typing import TYPE_CHECKING, Iterable

from .Bifrost import (
    ALL_REALMS, STARTING_REALM, FINAL_REALM, MIDDLE_REALMS, REALM_OBJECTIVES, BONUS_CATEGORIES,
    BIFROST_LOCATIONS, BIFROST_LOCATIONS_BY_MARKER, location_enabled, realm_item_name,
)
from .Locations import location_table

if TYPE_CHECKING:
    from .NorthgardClient import NorthgardContext

STATE_PREFIXES = ("realm_", "loc_", "obj_", "card_", "bonus_", "scout_")
ACTIVE_FLAG = "active.flag"


def bifrost_dir(config_dir: str) -> str:
    return os.path.join(config_dir, "bifrost")


def done_dir(config_dir: str) -> str:
    return os.path.join(bifrost_dir(config_dir), "done")


def set_active(config_dir: str, active: bool) -> None:
    path = os.path.join(bifrost_dir(config_dir), ACTIVE_FLAG)
    if active:
        os.makedirs(done_dir(config_dir), exist_ok=True)
        if not os.path.exists(path):
            open(path, "w", encoding="utf-8").close()
    else:
        try:
            os.remove(path)
        except OSError:
            pass


def clear_state_markers(config_dir: str) -> None:
    """Called when the connected room or slot changes: removes every state marker (not
    active.flag, not done markers). They are rewritten from the new slot's state next poll."""
    folder = bifrost_dir(config_dir)
    try:
        names = os.listdir(folder)
    except OSError:
        return
    for name in names:
        if name.startswith(STATE_PREFIXES):
            try:
                os.remove(os.path.join(folder, name))
            except OSError:
                pass


def _room_folder(config_dir: str, room_key: str) -> str:
    safe = "".join(c if c.isalnum() else "_" for c in room_key)
    return os.path.join(bifrost_dir(config_dir), "rooms", safe)


# The game names granted records "<runSeed>_<playerUID>_<Key>_<n>" and never deletes any, so
# without cleanup they pile up by a few dozen files every run. A run needs its records exactly as
# long as it can still be loaded, i.e. while its save (save\bifrost\<Clan>-<runSeed>.sav, deleted
# by the game when the run is won) exists; see live_bifrost_runs. Records touched this recently
# are always kept: a brand new run may not have written its save yet, and deleting a record the
# running game still checks would grant that bonus again.
GRANTED_GRACE_SECONDS = 2 * 86400


def live_bifrost_runs(northgard_save_dir: str) -> set[str] | None:
    """Run seeds that still have a Bifrost save, from the save file names. None when the save
    folder can't be read, which means "don't know": nothing is pruned then."""
    if not northgard_save_dir:
        return None
    try:
        names = os.listdir(os.path.join(northgard_save_dir, "bifrost"))
    except OSError:
        return None
    runs: set[str] = set()
    for name in names:
        stem, ext = os.path.splitext(name)
        seed = stem.rsplit("-", 1)[-1]
        if ext.lower() == ".sav" and seed.isdigit():
            runs.add(seed)
    return runs


def _move_all(src: str, dst: str) -> None:
    """Moves every file in src into dst, creating dst only when there is something to move
    (emptied parked folders are removed by _sweep_parked_rooms)."""
    try:
        names = os.listdir(src)
    except OSError:
        return
    if names:
        os.makedirs(dst, exist_ok=True)
    for name in names:
        try:
            os.replace(os.path.join(src, name), os.path.join(dst, name))
        except OSError:
            pass


def _remove_if_empty(path: str) -> None:
    try:
        os.rmdir(path)  # only succeeds on an empty folder
    except OSError:
        pass


def _prune_granted_runs(granted: str, live_runs: set[str] | None) -> None:
    """Deletes the granted records of runs whose save is gone (see GRANTED_GRACE_SECONDS)."""
    if live_runs is None:
        return
    try:
        names = os.listdir(granted)
    except OSError:
        return
    newest: dict[str, float] = {}  # run seed -> newest record time
    for name in names:
        parts = name.rsplit("_", 3)
        if len(parts) != 4:
            continue  # not a granted record: leave it alone
        try:
            mtime = os.path.getmtime(os.path.join(granted, name))
        except OSError:
            continue
        newest[parts[0]] = max(newest.get(parts[0], 0.0), mtime)
    cutoff = time.time() - GRANTED_GRACE_SECONDS
    stale = {run for run, mtime in newest.items() if run not in live_runs and mtime < cutoff}
    if not stale:
        return
    for name in names:
        parts = name.rsplit("_", 3)
        if len(parts) == 4 and parts[0] in stale:
            try:
                os.remove(os.path.join(granted, name))
            except OSError:
                pass


def _sweep_parked_rooms(config_dir: str, live_runs: set[str] | None) -> None:
    """Prunes every parked room's granted records and removes folders left empty, including
    ones from rooms never reconnected to (or created empty by older versions)."""
    rooms = os.path.join(bifrost_dir(config_dir), "rooms")
    try:
        entries = os.listdir(rooms)
    except OSError:
        return
    for entry in entries:
        room = os.path.join(rooms, entry)
        if not os.path.isdir(room):
            continue
        _prune_granted_runs(os.path.join(room, "granted"), live_runs)
        for sub in ("granted", "done"):
            _remove_if_empty(os.path.join(room, sub))
        _remove_if_empty(room)
    _remove_if_empty(rooms)


def switch_room_records(config_dir: str, room_key: str, live_runs: set[str] | None) -> None:
    """Swaps the per-room files the game writes when the connected room changes. Both kinds
    belong to the room that was connected when they were written (the owner), so the current
    set is parked under bifrost/rooms/<owner>/ and the new room's own set is moved back in:
      granted/  The game's per-run "granted" records (one per bonus item copy it has already
                given a player in a run, see patch_bonus_items). Going room A -> B -> A mid-run
                then never grants A's bonuses twice.
      done/     Done markers not sent yet, e.g. progress made with the client closed. Without
                this, the next Bifrost slot to connect would send another slot's leftover
                checks as its own (and its Vanaheim marker as its goal, releasing it).
    Conquest rooms take part too (with nothing to park): the game only writes either kind while
    active.flag is set, i.e. after a Bifrost room connected, so whatever is live always belongs
    to the last room connected.

    Also keeps the folder from growing forever: granted records of runs whose save is gone are
    deleted (see GRANTED_GRACE_SECONDS; live_runs from live_bifrost_runs, None skips that),
    and a room folder left with nothing parked in it is removed."""
    folder = bifrost_dir(config_dir)
    live_granted = os.path.join(folder, "granted")
    owner_file = os.path.join(folder, "granted_owner.txt")
    try:
        with open(owner_file, "r", encoding="utf-8") as f:
            owner = f.read().strip()
    except OSError:
        owner = ""
    _prune_granted_runs(live_granted, live_runs)  # live set belongs to `owner`, whichever way this goes
    if owner != room_key:
        os.makedirs(live_granted, exist_ok=True)
        if owner:
            parked = _room_folder(config_dir, owner)
            _move_all(live_granted, os.path.join(parked, "granted"))
            _move_all(done_dir(config_dir), os.path.join(parked, "done"))
        else:
            # Granted records with no known owner (written before this existed) can't be
            # attributed. Done markers are left for this room to send, as before.
            for name in os.listdir(live_granted):
                try:
                    os.remove(os.path.join(live_granted, name))
                except OSError:
                    pass
        incoming = _room_folder(config_dir, room_key)
        _move_all(os.path.join(incoming, "granted"), live_granted)
        _move_all(os.path.join(incoming, "done"), done_dir(config_dir))
        with open(owner_file, "w", encoding="utf-8") as f:
            f.write(room_key)
    # Every call, not only on a real switch: also removes the folders just emptied above, and
    # cleans up rooms nobody reconnects to even when the same room keeps reconnecting.
    _sweep_parked_rooms(config_dir, live_runs)


def _location_id(name: str) -> int:
    return location_table[name].id


def desired_markers(ctx: "NorthgardContext") -> set[str]:
    """Every state marker this slot should have right now."""
    perk_setting = ctx.bifrost_perk_setting
    missing: set[int] = set(ctx.missing_locations)
    received = [n for n in ctx.received_item_names if n]
    wanted: set[str] = {f"realm_{STARTING_REALM}"}
    for realm in [*MIDDLE_REALMS, FINAL_REALM]:
        if realm_item_name(realm) in received:
            wanted.add(f"realm_{realm}")

    realms_with_locations: set[str] = set()
    objectives_with_locations: set[tuple[str, str]] = set()
    for loc in BIFROST_LOCATIONS:
        if not location_enabled(loc, perk_setting) or _location_id(loc.name) not in missing:
            continue
        realms_with_locations.add(loc.realm)
        if loc.kind == "objective":
            objectives_with_locations.add((loc.realm, loc.objective))
        elif loc.kind == "card":
            wanted.add(loc.marker)
            # A reward screen with unchecked cards also makes its objective worth spawning.
            realm, _, screen = loc.screen.partition("_")
            if screen in REALM_OBJECTIVES.get(realm, []):
                objectives_with_locations.add((realm, screen))
    for realm in realms_with_locations:
        wanted.add(f"loc_{realm}")
    for realm, oid in objectives_with_locations:
        wanted.add(f"obj_{realm}_{oid}")

    for cat in BONUS_CATEGORIES:
        owned = min(received.count(cat.name), cat.cap)
        for n in range(1, owned + 1):
            wanted.add(f"bonus_{cat.key}_{n}")
    return wanted


def scout_file_name(card_marker: str) -> str:
    """card_<Realm>_<Screen>_<K> -> scout_<Realm>_<Screen>_<K>.txt (the game builds the same)."""
    return "scout_" + card_marker[len("card_"):] + ".txt"


def card_location_ids(ctx: "NorthgardContext") -> list[int]:
    """This slot's unchecked perk card locations, for scouting their items."""
    missing: set[int] = set(ctx.missing_locations)
    return [
        _location_id(loc.name) for loc in BIFROST_LOCATIONS
        if loc.kind == "card" and location_enabled(loc, ctx.bifrost_perk_setting)
        and _location_id(loc.name) in missing
    ]


def scout_text(ctx: "NorthgardContext", item) -> str:
    """Tooltip text for an AP card, from a scouted NetworkItem."""
    try:
        item_name = ctx.item_names.lookup_in_slot(item.item, item.player)
    except Exception:
        item_name = f"Item {item.item}"
    player = ctx.player_names.get(item.player, f"Player {item.player}")
    try:
        game = ctx.slot_info[item.player].game
    except Exception:
        game = ""
    kind = "Progression" if item.flags & 1 else "Useful" if item.flags & 2 else "Trap" if item.flags & 4 else "Filler"
    owner = "your" if item.player == ctx.slot else f"{player}'s"
    where = f" ({game})" if game and item.player != ctx.slot else ""
    return f"{owner[0].upper()}{owner[1:]} {item_name}{where}, {kind}"


def write_card_icon(config_dir: str) -> None:
    """Copies the bundled Archipelago logo next to the markers, where the game loads it."""
    path = os.path.join(bifrost_dir(config_dir), "ap_card.png")
    try:
        import importlib.resources
        data = importlib.resources.files(__package__).joinpath("data", "ap_card.png").read_bytes()
    except (FileNotFoundError, ModuleNotFoundError, OSError):
        return
    try:
        with open(path, "rb") as f:
            if f.read() == data:
                return  # already this version's icon
    except OSError:
        pass
    os.makedirs(bifrost_dir(config_dir), exist_ok=True)
    with open(path, "wb") as f:
        f.write(data)


def sync_markers(ctx: "NorthgardContext", config_dir: str) -> None:
    folder = bifrost_dir(config_dir)
    os.makedirs(done_dir(config_dir), exist_ok=True)
    wanted = desired_markers(ctx)
    # Tooltip files for every unchecked AP card whose item has been scouted.
    scout_texts: dict[str, str] = {}
    for marker in wanted:
        if marker.startswith("card_"):
            loc = BIFROST_LOCATIONS_BY_MARKER.get(marker)
            item = ctx.bifrost_scouts.get(_location_id(loc.name)) if loc else None
            if item is not None:
                scout_texts[scout_file_name(marker)] = scout_text(ctx, item)
    wanted |= set(scout_texts)
    try:
        present = {n for n in os.listdir(folder) if n.startswith(STATE_PREFIXES)}
    except OSError:
        present = set()
    for name in wanted - present:
        with open(os.path.join(folder, name), "w", encoding="utf-8") as f:
            f.write(scout_texts.get(name, ""))
    for name, text in scout_texts.items():
        if name in present:  # keep existing tooltips current (e.g. after a wording change)
            path = os.path.join(folder, name)
            try:
                with open(path, "r", encoding="utf-8") as f:
                    if f.read() == text:
                        continue
            except OSError:
                pass
            with open(path, "w", encoding="utf-8") as f:
                f.write(text)
    for name in present - wanted:
        try:
            os.remove(os.path.join(folder, name))
        except OSError:
            pass


def collect_done(config_dir: str) -> list[str]:
    try:
        return sorted(os.listdir(done_dir(config_dir)))
    except OSError:
        return []


def remove_done(config_dir: str, names: Iterable[str]) -> None:
    for name in names:
        try:
            os.remove(os.path.join(done_dir(config_dir), name))
        except OSError:
            pass


def resolve_seen(names: Iterable[str]) -> list[int]:
    """seen_<Realm>_<Screen>_<K> markers (an AP card was shown) -> its card location ids."""
    ids: list[int] = []
    for name in names:
        if name.startswith("seen_"):
            loc = BIFROST_LOCATIONS_BY_MARKER.get("card_" + name[len("seen_"):])
            if loc is not None:
                ids.append(_location_id(loc.name))
    return ids


def resolve_done(names: Iterable[str]) -> tuple[list[int], bool, list[str]]:
    """Maps done markers to (location ids, goal reached, markers that matched nothing)."""
    ids: list[int] = []
    goal = False
    unknown: list[str] = []
    for name in names:
        if name.startswith("seen_"):
            continue  # see resolve_seen
        if name == f"realm_{FINAL_REALM}":
            goal = True
            continue
        loc = BIFROST_LOCATIONS_BY_MARKER.get(name)
        if loc is None:
            unknown.append(name)
            continue
        ids.append(_location_id(loc.name))
    return ids, goal, unknown


def describe(ctx: "NorthgardContext") -> list[str]:
    """Human-readable status for the /bifrost command."""
    perk_setting = ctx.bifrost_perk_setting
    missing: set[int] = set(ctx.missing_locations)
    received = [n for n in ctx.received_item_names if n]
    lines: list[str] = []
    middle_owned = sum(1 for r in MIDDLE_REALMS if realm_item_name(r) in received)
    for realm in ALL_REALMS:
        locs = [l for l in BIFROST_LOCATIONS if l.realm == realm and location_enabled(l, perk_setting)]
        left = [l for l in locs if _location_id(l.name) in missing]
        state = "unlocked" if realm == STARTING_REALM or realm_item_name(realm) in received else "LOCKED"
        if realm == FINAL_REALM:
            # The game offers it only as a run's last step, after 2 middle realms.
            state += f" (final realm, offered after 2 middle realms in a run; middle realms unlocked: {middle_owned}/2)"
        lines.append(f"  {realm}: {state}, {len(left)}/{len(locs)} locations left")
        cards: dict[str, list[int]] = {}
        for l in left:
            if l.kind == "card":
                cards.setdefault(l.screen.partition("_")[2], []).append(l.card)
        for screen, positions in sorted(cards.items()):
            lines.append(f"      {screen} reward: AP cards at positions {', '.join(map(str, sorted(positions)))}")
    for cat in BONUS_CATEGORIES:
        owned = min(received.count(cat.name), cat.cap)
        if owned:
            lines.append(f"  {cat.name}: {owned}/{cat.cap}")
    return lines
