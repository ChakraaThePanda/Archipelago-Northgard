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


def _fmt_amount(amount: float) -> str:
    return str(int(amount)) if amount == int(amount) else str(amount)


@dataclass(frozen=True)
class ResourceKind:
    """One live-grantable resource. `key` must exactly match one of
    `tools/patch_northgard/src/main.rs`'s `RESOURCE_CONFIGS` entries, since that's how
    NorthgardClient.py names the marker file the patched game is watching for (see
    `_resource_marker_path`). `filler_amount`/`useful_amount` are display-only here, baked
    into filler_name/useful_name so the item's own name tells you what you're getting (e.g.
    "100 Food"); the amount that's actually GRANTED in-game still comes solely from
    patch_northgard's own RESOURCE_CONFIGS. These two tables are maintained by hand in two
    different languages with no shared source of truth: if you change a number in one, you
    MUST change the matching one here too, or the item's name will lie about what it gives."""
    key: str
    noun: str  # the plain resource name, no amount, e.g. "Food", "Krowns", "Lore/Faith"
    filler_amount: float  # MUST match patch_northgard's RESOURCE_CONFIGS[key].filler_amount
    useful_amount: float  # MUST match patch_northgard's RESOURCE_CONFIGS[key].useful_amount

    @property
    def filler_name(self) -> str:
        return f"{_fmt_amount(self.filler_amount)} {self.noun}"

    @property
    def useful_name(self) -> str:
        return f"{_fmt_amount(self.useful_amount)} Starting {self.noun}"


# Order matches the table this was designed against; see patch_northgard's own RESOURCE_CONFIGS
# comment for why some entries carry a "/"-joined pair of names (a clan-renamed resource, e.g.
# Lore/Faith, Military Experience/Hunting Trophies) rather than one, and its RESOURCE_CONFIGS
# for the amounts, which these MUST match exactly (see ResourceKind's own docstring).
RESOURCE_KINDS: list[ResourceKind] = [
    ResourceKind("money", "Krowns", filler_amount=100, useful_amount=50),
    ResourceKind("food", "Food", filler_amount=100, useful_amount=50),
    ResourceKind("wood", "Wood", filler_amount=100, useful_amount=50),
    ResourceKind("lore", "Lore/Faith", filler_amount=200, useful_amount=40),
    ResourceKind("stone", "Stone", filler_amount=10, useful_amount=5),
    ResourceKind("iron", "Iron", filler_amount=10, useful_amount=5),
    ResourceKind("military_xp", "Military Experience/Hunting Trophies", filler_amount=200, useful_amount=25),
]

# The pre-Useful/Filler-redesign "Krown" filler item's own id (BASE_ID + 900) is preserved
# below for money's filler specifically, since it shipped under that id in an earlier release,
# before this file existed; its *name* changing to "100 Krowns" doesn't move its id.
_MONEY_FILLER_LEGACY_KEY = "money"

# Every filler item this world can generate. create_items cycles through this list to pad the
# pool (rather than repeating a single filler type) once every guaranteed/useful item is placed.
FILLER_ITEM_NAMES: list[str] = [kind.filler_name for kind in RESOURCE_KINDS]

# Every "<amount> Starting X" item. Exactly one copy of each is ever placed (see
# create_items); it's the "Useful" classification precisely because losing it isn't fatal to
# completion, but unlike filler it's never lost to being offline and never generated more
# than once.
USEFUL_ITEM_NAMES: list[str] = [kind.useful_name for kind in RESOURCE_KINDS]

item_table: dict[str, ItemData] = {
    name: ItemData(BASE_ID + i, ItemClassification.progression)
    for i, name in enumerate(CHAPTER_ITEMS)
}
for i, kind in enumerate(RESOURCE_KINDS):
    filler_id = BASE_ID + 900 if kind.key == _MONEY_FILLER_LEGACY_KEY else BASE_ID + 901 + i
    item_table[kind.filler_name] = ItemData(filler_id, ItemClassification.filler)
for i, name in enumerate(USEFUL_ITEM_NAMES):
    item_table[name] = ItemData(BASE_ID + 910 + i, ItemClassification.useful)

# The event item awarded for beating Chapter 07 -- never placed in the pool, never sent
# over the network; it only exists so multiworld.completion_condition has something to check.
VICTORY_ITEM_NAME = "Victory"
