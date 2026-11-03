"""Static tables for Bifrost mode (game_mode: bifrost).

Everything here mirrors Northgard's own data tables (the `realm` and `objective` CastleDB
sheets inside res.client.pak), read from the installed game during design. The internal ids
(realm names, objective ids) are exactly the strings the patched game writes into its marker
files, so they must never be renamed; the display names are free to change.

Item and location ids are derived from these tables in a fixed order, so the order of every
list below is part of the save/room compatibility contract: append only, never reorder.
"""
from __future__ import annotations

from dataclasses import dataclass
from math import ceil

BIFROST_ITEM_BASE = 39190000 + 2000
BIFROST_LOCATION_BASE = 39190000 + 3000

STARTING_REALM = "Midgard"
FINAL_REALM = "Vanaheim"
MIDDLE_REALMS: list[str] = ["Muspelheim", "Niflheim", "Nidavellir", "Idavoll"]
ALL_REALMS: list[str] = [STARTING_REALM, *MIDDLE_REALMS, FINAL_REALM]

# How many of the middle realms a single run passes through before Vanaheim is offered.
MIDDLE_REALMS_PER_RUN = 2


def realm_item_name(realm: str) -> str:
    """A realm's unlock item is named after the realm itself."""
    return realm


# Vanaheim is last so the earlier realm items keep their ids. Every realm but Midgard is an
# item; Vanaheim, the final realm, is offered by the game only as a run's last step, and stays
# greyed out there until its item arrives.
REALM_ITEMS: list[str] = [realm_item_name(r) for r in [STARTING_REALM, *MIDDLE_REALMS, FINAL_REALM]]
MIDDLE_REALM_ITEMS: list[str] = [realm_item_name(r) for r in MIDDLE_REALMS]
FINAL_REALM_ITEM: str = realm_item_name(FINAL_REALM)


@dataclass(frozen=True)
class BonusCategory:
    """One category bonus item. `key` is what the client writes into the game's marker files
    (`bonus_<key>_<copy>`) and must match patch_northgard's BONUS_CATEGORIES table; `name` is
    the item's display name. `cap` is how many copies can ever be placed: a copy beyond the
    bonuses that exist for it would do nothing in game."""
    key: str
    name: str
    cap: int


# Resolved in game, per human player, into a random perk (or building) compatible with that
# player's clan that they don't have yet (patch_northgard's patch_bonus_items). Groups follow the
# game's own conquestBonus groups; "Objective" is the Pillar of God, Wyvern and Beacon reward
# pools combined. Tech picks are never items: they can replace a lore node mid-run.
BONUS_CATEGORIES: list[BonusCategory] = [
    BonusCategory("Military", "Military Bonus", 6),
    BonusCategory("Economy", "Economy Bonus", 8),
    BonusCategory("Special", "Special Bonus", 3),
    BonusCategory("Clan", "Clan Bonus", 3),
    BonusCategory("Objective", "Objective Bonus", 8),
    BonusCategory("Muspelheim", "Muspelheim Bonus", 4),
    BonusCategory("Niflheim", "Niflheim Bonus", 4),
    BonusCategory("Idavoll", "Idavoll Bonus", 4),
    BonusCategory("Nidavellir", "Nidavellir Bonus", 4),
    BonusCategory("Vanaheim", "Vanaheim Bonus", 4),
    BonusCategory("Building", "Building Unlock", 6),
]
BONUS_ITEMS: list[str] = [c.name for c in BONUS_CATEGORIES]
MAX_BONUS_ITEMS = sum(c.cap for c in BONUS_CATEGORIES)

# Realm-flavoured categories only make sense once their realm's portal or realm is reachable,
# but they are all safe to receive at any time: the game simply grants the realm-themed perk.


@dataclass(frozen=True)
class ObjectiveInfo:
    id: str  # the game's objective id, as written in markers
    name: str  # display name used in location names
    reward_cards: int  # card positions on its reward screen that can be locations; 0 = none


PORTAL_IDS: dict[str, str] = {realm: f"{realm}Portal" for realm in [*MIDDLE_REALMS, FINAL_REALM]}

OBJECTIVES: dict[str, ObjectiveInfo] = {
    **{
        # A portal reward screen offers 4 shared cards (one pick per player), but a map only ever
        # has one portal, so each extra card position would mean another visit to that realm.
        # Only position 1 is a location; the other 3 cards are always real perks. Reachable with
        # any player count: every player picks one of the 4, so position 1 is always available.
        oid: ObjectiveInfo(oid, f"{realm} Portal", 1)
        for realm, oid in PORTAL_IDS.items()
    },
    # One-card "take all" rewards: the card itself is the location.
    "Wyvern": ObjectiveInfo("Wyvern", "Wyvern", 1),
    "NidavellirBeacon": ObjectiveInfo("NidavellirBeacon", "Nidavellir Beacon", 1),
    "PillarOfGod": ObjectiveInfo("PillarOfGod", "Pillar of God", 1),
    # Befriend rewards (a recruit camp) are deliberately not locations.
    "GiantBefriend": ObjectiveInfo("GiantBefriend", "Befriend the Giants", 0),
    "DwarfBefriend": ObjectiveInfo("DwarfBefriend", "Befriend the Dwarves", 0),
    "KoboldBefriend": ObjectiveInfo("KoboldBefriend", "Befriend the Kobolds", 0),
    "MyrkalfarBefriend": ObjectiveInfo("MyrkalfarBefriend", "Befriend the Myrkalfar", 0),
}

_DEFAULT_OBJECTIVES = ["Wyvern", "NidavellirBeacon", "PillarOfGod"]

# Which objectives can spawn in each realm (the realm sheet's mandatoryObjectives list, plus
# every standard objective whose onlyForRealm allows it).
REALM_OBJECTIVES: dict[str, list[str]] = {
    "Midgard": [PORTAL_IDS[r] for r in [*MIDDLE_REALMS, FINAL_REALM]] + _DEFAULT_OBJECTIVES + ["GiantBefriend"],
    "Muspelheim": [PORTAL_IDS[r] for r in ["Niflheim", "Nidavellir", "Idavoll", FINAL_REALM]] + _DEFAULT_OBJECTIVES + ["MyrkalfarBefriend"],
    "Niflheim": [PORTAL_IDS[r] for r in ["Muspelheim", "Nidavellir", "Idavoll", FINAL_REALM]] + _DEFAULT_OBJECTIVES + ["KoboldBefriend"],
    "Nidavellir": [PORTAL_IDS[r] for r in ["Muspelheim", "Niflheim", "Idavoll", FINAL_REALM]] + _DEFAULT_OBJECTIVES + ["DwarfBefriend"],
    "Idavoll": [PORTAL_IDS[r] for r in ["Muspelheim", "Niflheim", "Nidavellir", FINAL_REALM]] + _DEFAULT_OBJECTIVES,
    "Vanaheim": list(_DEFAULT_OBJECTIVES),
}

# Perk screens from defeated waves: waves 1-4 give one (`noBonusWave: 4` in every realm).
# Positions count across the whole screen: solo shows 6 cards, co-op 3 per player laid out
# player by player (2 players = 6, 3-4 players = 9-12), so positions 1-6 exist with any player
# count. Positions past 6 always show real perks.
WAVE_REWARDS_PER_REALM = 4
WAVE_REWARD_CARDS = 6


@dataclass(frozen=True)
class BifrostLocation:
    name: str
    realm: str
    kind: str  # "realm", "objective", "card"
    marker: str  # the marker name the game writes into the done folder
    # For card locations: which screen and position, used to apply the ap_perk_cards option.
    screen: str = ""
    card: int = 0
    cards_on_screen: int = 0
    # For objective locations: the objective id (for the spawn-priority markers).
    objective: str = ""


def complete_realm_location(realm: str) -> str:
    return f"Complete {realm}"


def _screen_key_wave(realm: str, n: int) -> str:
    return f"{realm}_Wave{n}"


def _screen_key_objective(realm: str, oid: str) -> str:
    return f"{realm}_{oid}"


def _build_locations() -> list[BifrostLocation]:
    locs: list[BifrostLocation] = []
    for realm in ALL_REALMS:
        if realm != FINAL_REALM:  # Vanaheim's completion is the goal event, not a location
            locs.append(BifrostLocation(complete_realm_location(realm), realm, "realm", f"realm_{realm}"))
    for realm in ALL_REALMS:
        for oid in REALM_OBJECTIVES[realm]:
            info = OBJECTIVES[oid]
            locs.append(BifrostLocation(
                f"{realm} - {info.name}", realm, "objective", f"obj_{realm}_{oid}", objective=oid,
            ))
    for realm in ALL_REALMS:
        for n in range(1, WAVE_REWARDS_PER_REALM + 1):
            screen = _screen_key_wave(realm, n)
            for k in range(1, WAVE_REWARD_CARDS + 1):
                locs.append(BifrostLocation(
                    f"{realm} - Wave {n} Reward - Card {k}", realm, "card", f"card_{screen}_{k}",
                    screen=screen, card=k, cards_on_screen=WAVE_REWARD_CARDS,
                ))
        for oid in REALM_OBJECTIVES[realm]:
            info = OBJECTIVES[oid]
            if info.reward_cards == 0:
                continue
            screen = _screen_key_objective(realm, oid)
            for k in range(1, info.reward_cards + 1):
                locs.append(BifrostLocation(
                    f"{realm} - {info.name} Reward - Card {k}", realm, "card", f"card_{screen}_{k}",
                    screen=screen, card=k, cards_on_screen=info.reward_cards,
                ))
    return locs


BIFROST_LOCATIONS: list[BifrostLocation] = _build_locations()
BIFROST_LOCATIONS_BY_MARKER: dict[str, BifrostLocation] = {loc.marker: loc for loc in BIFROST_LOCATIONS}

BIFROST_VICTORY_LOCATION = f"Complete {FINAL_REALM}"

# ap_perk_cards option values (see Options.ApPerkCards).
PERK_CARDS_MINIMUM = 0
PERK_CARDS_HALF = 1
PERK_CARDS_MAXIMUM = 2


def cards_used(cards_on_screen: int, setting: int) -> int:
    if setting == PERK_CARDS_MINIMUM:
        return 1
    if setting == PERK_CARDS_MAXIMUM:
        return cards_on_screen
    return ceil(cards_on_screen / 2)


def location_enabled(loc: BifrostLocation, perk_setting: int) -> bool:
    if loc.kind != "card":
        return True
    return loc.card <= cards_used(loc.cards_on_screen, perk_setting)
