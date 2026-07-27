from typing import Any

from BaseClasses import Region, ItemClassification, Tutorial
from worlds.AutoWorld import World, WebWorld
from worlds.generic.Rules import set_rule
from worlds.LauncherComponents import Component, Type, components, launch_subprocess

from .Items import item_table, NorthgardItem, CHAPTER_ITEMS, FILLER_ITEM_NAME, VICTORY_ITEM_NAME
from .Locations import location_table, NorthgardLocation, VICTORY_LOCATION
from .Regions import CHAPTERS, CHAPTER_CONNECTIONS, STARTING_CHAPTER, FINAL_CHAPTER
from .Options import NorthgardOptions
from .Rules import set_completion_rule


def launch_client(*args) -> None:
    # *args exists only so this matches the signature Component.run() always calls
    # (self.func(*args) -- args is non-empty when the Launcher is invoked with extra
    # CLI tokens after "--", e.g. a url). Not forwarded further: like Manual's own
    # launch_client, the client reads sys.argv itself (see NorthgardClient.launch).
    from CommonClient import gui_enabled
    from .NorthgardClient import launch as Main

    if gui_enabled:
        launch_subprocess(Main, name="Northgard Client")
    else:
        Main()


components.append(Component("Northgard Client", func=launch_client, component_type=Type.CLIENT))


class NorthgardWeb(WebWorld):
    tutorials = [Tutorial(
        "Multiworld Setup Guide",
        "A guide to setting up Northgard for Archipelago multiworld play.",
        "English",
        "setup_en.md",
        "setup/en",
        ["Chakraa"],
    )]
    theme = "grass"


class NorthgardWorld(World):
    """Play Northgard's Conquest mode across its branching tree of Chapters. Each Chapter
    beyond the first is gated behind an item you receive from the multiworld; completing a
    Chapter's battle sends its location checks."""

    game = "Northgard"
    author: str = "Chakraa"
    web = NorthgardWeb()

    options_dataclass = NorthgardOptions
    options: NorthgardOptions

    item_name_to_id = {name: data.code for name, data in item_table.items()}
    location_name_to_id = {name: data.id for name, data in location_table.items()}

    item_name_groups = {"Chapters": set(CHAPTER_ITEMS)}

    def create_item(self, name: str) -> NorthgardItem:
        data = item_table[name]
        return NorthgardItem(name, data.classification, data.code, self.player)

    def create_regions(self) -> None:
        menu = Region("Menu", self.player, self.multiworld)
        self.multiworld.regions.append(menu)

        regions: dict[str, Region] = {}
        for chapter in CHAPTERS:
            region = Region(chapter, self.player, self.multiworld)
            self.multiworld.regions.append(region)
            regions[chapter] = region

        menu.connect(regions[STARTING_CHAPTER])

        for chapter, targets in CHAPTER_CONNECTIONS.items():
            for target in targets:
                entrance = regions[chapter].connect(regions[target])
                set_rule(entrance, lambda state, item=target: state.has(item, self.player))

        amount = self.options.amount_of_locations.value
        for chapter in CHAPTERS:
            region = regions[chapter]
            for i in range(1, amount + 1):
                loc_name = f"{chapter} - Item {i:02d}"
                region.locations.append(
                    NorthgardLocation(self.player, loc_name, location_table[loc_name].id, region)
                )

        victory_location = NorthgardLocation(self.player, VICTORY_LOCATION, None, regions[FINAL_CHAPTER])
        victory_location.place_locked_item(
            NorthgardItem(VICTORY_ITEM_NAME, ItemClassification.progression, None, self.player)
        )
        regions[FINAL_CHAPTER].locations.append(victory_location)

    def create_items(self) -> None:
        # The player always starts able to enter the first node.
        self.multiworld.push_precollected(self.create_item(STARTING_CHAPTER))

        pool = [self.create_item(name) for name in CHAPTER_ITEMS if name != STARTING_CHAPTER]

        total_locations = len(self.multiworld.get_unfilled_locations(self.player))
        while len(pool) < total_locations:
            pool.append(self.create_item(FILLER_ITEM_NAME))

        self.multiworld.itempool += pool

    def set_rules(self) -> None:
        set_completion_rule(self.multiworld, self.player)

    def fill_slot_data(self) -> dict[str, Any]:
        return {
            "amount_of_locations": self.options.amount_of_locations.value,
        }
