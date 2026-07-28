from dataclasses import dataclass

from Options import Range, Choice, PerGameCommonOptions


class AmountOfLocations(Range):
    """How many location checks each Chapter sends when you complete its battle (1-10)."""
    display_name = "Items per Chapter"
    range_start = 1
    range_end = 10
    default = 4


class ProgressionMode(Choice):
    """Linear (default): Northgard's own adjacency rule still applies on top of owning a
    Chapter's item -- you must have beaten a connected Chapter before the next one opens
    up, same as vanilla Conquest.

    Non-Linear: a Chapter becomes selectable as soon as you receive its item, with no
    requirement to have already beaten an adjacent Chapter first -- you can jump straight
    to a later branch of the tree out of order."""
    display_name = "Progression Mode"
    option_linear = 0
    option_non_linear = 1
    default = 0


class Chapter7Requirement(Range):
    """Non-Linear Mode only (ignored in Linear Mode): how many *other* battles must
    actually be won in-game before Chapter 07 (the final battle) becomes selectable, even
    after you've received its item. Prevents rushing straight to the end the moment you
    happen to receive Chapter 07's item. 0 means no extra requirement -- Chapter 07 unlocks
    as soon as its item arrives, same as every other Chapter in Non-Linear Mode.

    10 is the most other battles there are to win (every Top and Bottom battle across
    Chapters 1-6) -- Non-Linear Mode lets you win both a Top and a Bottom battle instead of
    just one, so this counts individual battles won, not Chapters."""
    display_name = "Chapter 7 Requirement (Non-Linear Mode only)"
    range_start = 0
    range_end = 10
    default = 5


@dataclass
class NorthgardOptions(PerGameCommonOptions):
    amount_of_locations: AmountOfLocations
    progression_mode: ProgressionMode
    chapter7_requirement: Chapter7Requirement
