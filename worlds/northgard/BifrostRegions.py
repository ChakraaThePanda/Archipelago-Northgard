"""Bifrost-mode regions, items and logic, called from NorthgardWorld when game_mode is bifrost.

Logic model (see the Bifrost design doc):
- Midgard is a starting item and always reachable.
- Each middle realm needs its realm item.
- Vanaheim needs its own realm item and 2 middle realms (a run passes through 2 before Vanaheim
  is offered). Bonus items are Useful: they make runs easier but never gate anything.
- Every location in a realm is reachable once the realm is: objectives spawn at random, but
  the patched game favours ones that still hold a location, and a new run rerolls the map.
"""
from __future__ import annotations

from typing import TYPE_CHECKING

from BaseClasses import Region, ItemClassification
from worlds.generic.Rules import set_rule

from .Bifrost import (
    ALL_REALMS, STARTING_REALM, FINAL_REALM, MIDDLE_REALMS, MIDDLE_REALMS_PER_RUN,
    MIDDLE_REALM_ITEMS, FINAL_REALM_ITEM, BONUS_CATEGORIES, MAX_BONUS_ITEMS, BIFROST_LOCATIONS,
    BIFROST_VICTORY_LOCATION,
    realm_item_name, location_enabled,
)
from .Items import NorthgardItem, FILLER_ITEM_NAMES, USEFUL_ITEM_NAMES, VICTORY_ITEM_NAME
from .Locations import NorthgardLocation, location_table

if TYPE_CHECKING:
    from . import NorthgardWorld

# Share of the free (non-realm, non-useful) item slots that become bonus items; the rest are
# instant resource fillers. Bonus items are what make Vanaheim beatable, so they dominate.
BONUS_SHARE = 0.6


def create_bifrost_regions(world: "NorthgardWorld") -> None:
    player = world.player
    multiworld = world.multiworld
    perk_setting = world.options.ap_perk_cards.value

    menu = Region("Menu", player, multiworld)
    multiworld.regions.append(menu)
    regions: dict[str, Region] = {}
    for realm in ALL_REALMS:
        region = Region(realm, player, multiworld)
        multiworld.regions.append(region)
        regions[realm] = region

    menu.connect(regions[STARTING_REALM])
    for realm in MIDDLE_REALMS:
        entrance = menu.connect(regions[realm], f"Menu -> {realm}")
        set_rule(entrance, lambda state, item=realm_item_name(realm): state.has(item, player))

    vanaheim = menu.connect(regions[FINAL_REALM], f"Menu -> {FINAL_REALM}")
    set_rule(
        vanaheim,
        lambda state: state.has(FINAL_REALM_ITEM, player)
        and state.has_from_list(MIDDLE_REALM_ITEMS, player, MIDDLE_REALMS_PER_RUN),
    )

    for loc in BIFROST_LOCATIONS:
        if not location_enabled(loc, perk_setting):
            continue
        region = regions[loc.realm]
        region.locations.append(NorthgardLocation(player, loc.name, location_table[loc.name].id, region))

    victory = NorthgardLocation(player, BIFROST_VICTORY_LOCATION, None, regions[FINAL_REALM])
    victory.place_locked_item(NorthgardItem(VICTORY_ITEM_NAME, ItemClassification.progression, None, player))
    regions[FINAL_REALM].locations.append(victory)


def create_bifrost_items(world: "NorthgardWorld") -> None:
    multiworld = world.multiworld
    world.multiworld.push_precollected(world.create_item(realm_item_name(STARTING_REALM)))

    pool = [world.create_item(name) for name in [*MIDDLE_REALM_ITEMS, FINAL_REALM_ITEM]]
    total = len(multiworld.get_unfilled_locations(world.player))

    useful = list(USEFUL_ITEM_NAMES)
    room = max(0, total - len(pool))
    if room < len(useful):
        useful = world.random.sample(useful, room)
    pool += [world.create_item(name) for name in useful]

    free = max(0, total - len(pool))
    bonus_count = min(free, MAX_BONUS_ITEMS, round(free * BONUS_SHARE))
    # Round-robin across categories, skipping any that reached its cap.
    placed = {c.name: 0 for c in BONUS_CATEGORIES}
    while sum(placed.values()) < bonus_count:
        for c in BONUS_CATEGORIES:
            if placed[c.name] < c.cap and sum(placed.values()) < bonus_count:
                placed[c.name] += 1
                pool.append(world.create_item(c.name))

    while len(pool) < total:
        pool.append(world.create_item(FILLER_ITEM_NAMES[len(pool) % len(FILLER_ITEM_NAMES)]))

    multiworld.itempool += pool

