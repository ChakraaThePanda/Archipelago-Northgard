from dataclasses import dataclass

from Options import Range, Choice, PerGameCommonOptions

# Every option is documented the same way: the first line says which mode it applies to and
# what it does; Choice options then list each value as "value: meaning". Northgard.yaml's
# comments are generated from these docstrings, so keep them in sync by editing them here.


class GameMode(Choice):
    """Which Northgard mode this slot plays. Options marked Conquest or Bifrost only apply to
    that mode.

    conquest: the Conquest campaign map.
    bifrost: the Bifrost roguelite."""
    display_name = "Game Mode"
    option_conquest = 0
    option_bifrost = 1
    default = 0


class AmountOfLocations(Range):
    """Conquest only. How many location checks each Chapter sends when you win its battle."""
    display_name = "Items per Chapter (Conquest)"
    range_start = 1
    range_end = 10
    default = 4


class ProgressionMode(Choice):
    """Conquest only. How a Chapter becomes selectable once you have its item.

    linear: you must also have beaten a connected Chapter, as in vanilla Conquest.
    non_linear: the item alone is enough, so you can jump ahead in the tree."""
    display_name = "Progression Mode (Conquest)"
    option_linear = 0
    option_non_linear = 1
    default = 0


class Chapter7Requirement(Range):
    """Conquest only, with non_linear Progression Mode. How many other battles you must win
    before Chapter 07, the final battle, becomes selectable, even with its item. 0: its item
    alone is enough. 10: every Top and Bottom battle of Chapters 1-6."""
    display_name = "Chapter 7 Requirement (Conquest)"
    range_start = 0
    range_end = 10
    default = 5


class ApPerkCards(Choice):
    """Bifrost only. How many cards on each perk screen are locations. Picking one sends a check
    instead of giving the perk; once sent, that card shows a real perk on later visits. Portal
    rewards always have one location card.

    minimum: 1 card per screen.
    half: half the cards, rounded up.
    maximum: every card."""
    display_name = "AP Perk Cards (Bifrost)"
    option_minimum = 0
    option_half = 1
    option_maximum = 2
    default = 1


@dataclass
class NorthgardOptions(PerGameCommonOptions):
    game_mode: GameMode
    # Conquest
    amount_of_locations: AmountOfLocations
    progression_mode: ProgressionMode
    chapter7_requirement: Chapter7Requirement
    # Bifrost
    ap_perk_cards: ApPerkCards
