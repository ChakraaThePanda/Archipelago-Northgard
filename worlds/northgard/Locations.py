from dataclasses import dataclass

from BaseClasses import Location

from .Regions import CHAPTERS

BASE_ID = 39190000
MAX_ITEMS_PER_CHAPTER = 10


class NorthgardLocation(Location):
    game: str = "Northgard"


@dataclass(frozen=True)
class LocationData:
    id: int


# Every possible "Chapter N [- Top/Bottom] - Item NN" slot (up to 10 per chapter) is
# pre-declared here so location_name_to_id is stable regardless of what amount_of_locations
# is set to in any given seed -- only a subset actually gets placed into the world at
# generation time (see NorthgardWorld.create_regions).
location_table: dict[str, LocationData] = {}

_next_id = BASE_ID + 1000
for _chapter in CHAPTERS:
    for _i in range(1, MAX_ITEMS_PER_CHAPTER + 1):
        _name = f"{_chapter} - Item {_i:02d}"
        location_table[_name] = LocationData(_next_id)
        _next_id += 1

VICTORY_LOCATION = "Chapter 07 - Victory"

# --- Bifrost mode (game_mode: bifrost) ---------------------------------------------------
# Every possible Bifrost location, pre-declared in Bifrost.py's fixed order so ids stay
# stable whatever the ap_perk_cards setting is.
from .Bifrost import BIFROST_LOCATION_BASE, BIFROST_LOCATIONS  # noqa: E402

for _i, _loc in enumerate(BIFROST_LOCATIONS):
    location_table[_loc.name] = LocationData(BIFROST_LOCATION_BASE + _i)
