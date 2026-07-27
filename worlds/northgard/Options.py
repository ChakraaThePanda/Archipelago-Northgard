from dataclasses import dataclass

from Options import Range, PerGameCommonOptions


class AmountOfLocations(Range):
    """How many location checks each Chapter sends when you complete its battle (1-10)."""
    display_name = "Items per Chapter"
    range_start = 1
    range_end = 10
    default = 4


@dataclass
class NorthgardOptions(PerGameCommonOptions):
    amount_of_locations: AmountOfLocations
