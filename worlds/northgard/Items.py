from dataclasses import dataclass

from BaseClasses import Item, ItemClassification

# Arbitrary but distinctive base -- check https://archipelago.gg (or the community ID
# registry) for collisions with any other apworld you have installed before generating
# alongside other games.
BASE_ID = 39190000


@dataclass(frozen=True)
class ItemData:
    code: int
    classification: ItemClassification


class NorthgardItem(Item):
    game: str = "Northgard"


# One unlock token per Chapter. Receiving "Chapter 02 - Top" is what should make that
# Conquest node selectable in-game; "Chapter 01" is always given as a starting item.
CHAPTER_ITEMS: list[str] = [
    "Chapter 01",
    "Chapter 02 - Top",
    "Chapter 02 - Bottom",
    "Chapter 03 - Top",
    "Chapter 03 - Bottom",
    "Chapter 04",
    "Chapter 05 - Top",
    "Chapter 05 - Bottom",
    "Chapter 06 - Top",
    "Chapter 06 - Bottom",
    "Chapter 07",
]

FILLER_ITEM_NAME = "Krown"

item_table: dict[str, ItemData] = {
    name: ItemData(BASE_ID + i, ItemClassification.progression)
    for i, name in enumerate(CHAPTER_ITEMS)
}
item_table[FILLER_ITEM_NAME] = ItemData(BASE_ID + 900, ItemClassification.filler)

# The event item awarded for beating Chapter 07 -- never placed in the pool, never sent
# over the network; it only exists so multiworld.completion_condition has something to check.
VICTORY_ITEM_NAME = "Victory"
