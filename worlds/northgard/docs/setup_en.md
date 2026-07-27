# Northgard Setup Guide

## Required Software

- [Archipelago](https://github.com/ArchipelagoMW/Archipelago/releases)
- Northgard (Steam), Conquest mode
- The `NorthgardClient` from this project's `client/` folder

## Installation

1. Place `northgard.apworld` in your Archipelago `custom_worlds` (or `worlds`) folder.
2. Generate or join a multiworld using your Northgard YAML.
3. Run `NorthgardClient.py` (Python 3.11+, with the Archipelago core repo's
   `CommonClient.py`/`NetUtils.py`/`Utils.py` importable) and connect it to your room
   the same way you would any other Archipelago client -- server address, slot name,
   optional password.
4. Leave the client running in the background while you play. It watches your Northgard
   save folder directly; you do not need to manually report anything in-game.

## Notes

- The client writes to your Northgard save files to reflect which Chapters you've
  unlocked. It takes a timestamped backup of any file before it modifies it.
- Consider disabling Steam Cloud sync for Northgard while playing with the client
  active, to avoid the cloud sync racing with the client's writes.
