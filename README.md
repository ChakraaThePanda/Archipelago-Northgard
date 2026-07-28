# Archipelago Northgard

An [Archipelago](https://archipelago.gg) randomizer for **Northgard**, played in **Conquest
mode**. Complete your Conquest map -- solo or in co-op -- by winning Chapter 7.

See [how randomization works](worlds/northgard/docs/en_Northgard.md) (goal, what gets
shuffled, Progression Modes) and the [setup guide](worlds/northgard/docs/setup_en.md) (full
install/connect walkthrough) for details. Quick version below.

## Quick start

1. Get `northgard.apworld` and drop it into your Archipelago install's `custom_worlds`
   folder. Every player with a Northgard slot needs to do this on their own machine, even
   if someone else is generating/hosting the room.
2. Add a Northgard entry to your player YAML (see `Northgard.yaml` for a ready-to-use
   template) and generate or join a multiworld like any other game.
3. Open the Archipelago Launcher, click **Northgard Client**, and connect (server address,
   slot name, password). The first time you connect, it automatically patches your
   Northgard install so locked Chapters are genuinely unselectable in-game -- no manual
   step needed, and it keeps healing itself on every future connect (e.g. if a Steam
   update ever reverts it).
4. Run `/conquest` to pick your Conquest save for this playthrough, then play normally --
   checks send and Chapters unlock automatically.
   - Make sure you have created your Conquest save first before using the `/conquest` command.
  
## Uninstall

Want to play vanilla Northgard sometimes? Grab `patch_northgard.exe` and just double-click
it -- it finds your Northgard install automatically and gives you a simple menu to check
status or switch back to vanilla. The client only ever applies the patch automatically,
never reverts it, so switching back to vanilla is always a deliberate, manual step.

## Known Issues

- If you're already sitting on the Conquest map when a Chapter unlocks (right after
  pinning a save, or right after receiving an item), back out to the main menu and back in
  (or reload the save) to see it reflected -- the map only checks what's unlocked when it
  first opens.
- After finishing a battle, you might see the animation where the next Chapter gets unlocked. It will show a Flag like you can play it, even if you haven't unlocked it yet. Going back to the main menu and back in will properly show the lock again.
