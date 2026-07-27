from BaseClasses import MultiWorld

from .Items import VICTORY_ITEM_NAME


def set_completion_rule(multiworld: MultiWorld, player: int) -> None:
    multiworld.completion_condition[player] = lambda state: state.has(VICTORY_ITEM_NAME, player)
