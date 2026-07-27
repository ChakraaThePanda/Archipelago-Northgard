"""
Archipelago client for Northgard. Watches a specific, user-pinned Conquest save for
newly-completed Chapters and reports them as location checks; applies received
Chapter-unlock items back (see NOTE in on_package below -- the enforcement side of this
is not yet resolved).

Ships inside the northgard.apworld package (like Manual's ManualClient.py) and is
launched via the Archipelago Launcher's component registration in __init__.py -- it is
not meant to be run as a loose standalone script against a source checkout.

IMPORTANT: there is no shared ID between an Archipelago room and a Northgard Conquest
run's internal seed, so which save file belongs to this playthrough can never be
inferred automatically -- if you have more than one Conquest in progress, use the
in-client `/conquest` command to tell it which one. See _cmd_conquest below.
"""
from __future__ import annotations

import asyncio
import json
import os
import sys

import Utils
from CommonClient import gui_enabled, logger, get_base_parser, CommonContext, ClientCommandProcessor, server_loop
from NetUtils import ClientStatus

from .Items import item_table as northgard_item_table
from .Locations import location_table as northgard_location_table
from .save_state import list_conquest_saves, read_conquest_run_state, CHAPTER_ROWS, ConquestSaveSummary

# All Chapter names in tree order, matching Regions.py.
ALL_CHAPTERS: list[str] = [name for row in CHAPTER_ROWS for name in row]

# Reuse the apworld's own name<->id tables directly rather than resolving names over the
# network -- this client only ever talks to this one game, so there's no need for the
# generic multi-game reverse-lookup dance CommonContext otherwise provides.
_ITEM_ID_TO_NAME: dict[int, str] = {data.code: name for name, data in northgard_item_table.items()}

NORTHGARD_SAVE_DIR = r"D:\Games\Steam\steamapps\common\Northgard\save"
POLL_INTERVAL_SECONDS = 5.0

# Local-only, not part of Northgard's own save data -- just remembers which conquest
# save filename you pinned last time, so you aren't asked again every launch. Filenames
# are stable for a run's entire lifetime (clan pair + difficulty + seed never change).
_PIN_FILE = os.path.join(os.path.expanduser("~"), ".northgard_ap_client_pin.json")


def _load_pinned_filename() -> str | None:
    try:
        with open(_PIN_FILE, "r", encoding="utf-8") as f:
            return json.load(f).get("pinned_filename")
    except (OSError, json.JSONDecodeError, AttributeError):
        return None


def _save_pinned_filename(filename: str) -> None:
    try:
        with open(_PIN_FILE, "w", encoding="utf-8") as f:
            json.dump({"pinned_filename": filename}, f)
    except OSError:
        logger.warning(f"[Northgard] Couldn't persist pinned save choice to {_PIN_FILE}")


def _format_save_line(index: int, s: ConquestSaveSummary, pinned_filename: str | None) -> str:
    marker = "  <- currently pinned" if s.filename == pinned_filename else ""
    return f"  [{index}] {s.filename} -- {s.clan} vs {s.opponent_clan}, {s.chapters_completed} chapters completed{marker}"


class NorthgardCommandProcessor(ClientCommandProcessor):
    def _cmd_conquest(self, choice: str = "") -> bool:
        """List in-progress Conquest saves, or pin one by number: /conquest 2.
        Required if you have more than one Conquest run going -- there's no way to
        tell them apart automatically."""
        ctx: NorthgardContext = self.ctx
        saves = list_conquest_saves(NORTHGARD_SAVE_DIR)
        if not saves:
            logger.info(f"[Northgard] No conquest saves found under {NORTHGARD_SAVE_DIR}\\conquest")
            return False

        choice = choice.strip()
        if choice:
            if not choice.isdigit() or not (0 <= int(choice) < len(saves)):
                logger.info(f"[Northgard] '{choice}' isn't a valid choice -- run /conquest with no argument to see the list")
                return False
            chosen = saves[int(choice)]
            ctx.pinned_save_path = chosen.path
            ctx.sent_chapters.clear()
            _save_pinned_filename(chosen.filename)
            logger.info(
                f"[Northgard] Pinned to {chosen.filename} ({chosen.clan} vs {chosen.opponent_clan}, "
                f"{chosen.chapters_completed} chapters already completed there)."
            )
            return True

        pinned_filename = os.path.basename(ctx.pinned_save_path) if ctx.pinned_save_path else _load_pinned_filename()
        logger.info("[Northgard] In-progress Conquest saves (use '/conquest <number>' to pin one):")
        for i, s in enumerate(saves):
            logger.info(_format_save_line(i, s, pinned_filename))
        return True


class NorthgardContext(CommonContext):
    game = "Northgard"
    items_handling = 0b111  # full remote: server is the source of truth for what we've received
    command_processor = NorthgardCommandProcessor

    def __init__(self, server_address, password):
        super().__init__(server_address, password)
        self.amount_of_locations: int = 4  # overwritten from slot_data on connect
        self.sent_chapters: set[str] = set()
        self.finished_game: bool = False
        self.pinned_save_path: str | None = None

        pinned_filename = _load_pinned_filename()
        if pinned_filename:
            match = next(
                (s for s in list_conquest_saves(NORTHGARD_SAVE_DIR) if s.filename == pinned_filename), None
            )
            if match is not None:
                self.pinned_save_path = match.path
                logger.info(
                    f"[Northgard] Resuming with previously-pinned save: {match.filename} "
                    f"({match.clan} vs {match.opponent_clan}). Use /conquest to change it."
                )
            else:
                logger.info(
                    f"[Northgard] Previously-pinned save {pinned_filename!r} no longer found. "
                    f"Use /conquest to pick one."
                )
        else:
            logger.info("[Northgard] No conquest save pinned yet -- use /conquest to see and pick from your in-progress runs.")

    async def server_auth(self, password_requested: bool = False):
        if password_requested and not self.password:
            await super().server_auth(password_requested)
        await self.get_username()
        await self.send_connect()

    def on_package(self, cmd: str, args: dict):
        if cmd == "Connected":
            self.amount_of_locations = args.get("slot_data", {}).get("amount_of_locations", 4)
        elif cmd == "ReceivedItems":
            for item in args["items"]:
                item_name = _ITEM_ID_TO_NAME.get(item.item, f"<unknown id {item.item}>")
                # NOTE: this is where a received "Chapter N [- Top/Bottom]" item should
                # be applied back to Northgard so that node becomes selectable in-game.
                # Not yet implemented -- see project notes on why this needs confirming
                # against how the Conquest node-select screen actually decides what's
                # choosable before writing anything here. For now this only logs.
                logger.info(f"[Northgard] Received item: {item_name} (lock enforcement not yet implemented)")

    def location_ids_for_chapter(self, chapter_name: str) -> list[int]:
        ids = []
        for i in range(1, self.amount_of_locations + 1):
            loc_name = f"{chapter_name} - Item {i:02d}"
            data = northgard_location_table.get(loc_name)
            if data is not None:
                ids.append(data.id)
        return ids


async def save_watcher(ctx: NorthgardContext):
    warned_no_pin = False
    while not ctx.exit_event.is_set():
        try:
            if ctx.pinned_save_path is None:
                if not warned_no_pin:
                    logger.info("[Northgard] Waiting for a pinned save -- run /conquest to pick one.")
                    warned_no_pin = True
            elif not os.path.exists(ctx.pinned_save_path):
                logger.warning(f"[Northgard] Pinned save no longer found: {ctx.pinned_save_path}. Run /conquest again.")
            else:
                state = read_conquest_run_state(ctx.pinned_save_path)

                new_chapters = [c for c in state.completed_chapters if c not in ctx.sent_chapters]
                if new_chapters:
                    location_ids: list[int] = []
                    for chapter in new_chapters:
                        location_ids.extend(ctx.location_ids_for_chapter(chapter))
                        ctx.sent_chapters.add(chapter)

                    if location_ids:
                        logger.info(f"[Northgard] Completed: {new_chapters} -> sending {len(location_ids)} checks")
                        await ctx.send_msgs([{"cmd": "LocationChecks", "locations": location_ids}])

                    if not ctx.finished_game and "Chapter 07" in ctx.sent_chapters:
                        ctx.finished_game = True
                        await ctx.send_msgs([{"cmd": "StatusUpdate", "status": ClientStatus.CLIENT_GOAL}])
        except Exception:
            logger.exception("[Northgard] save_watcher iteration failed")

        await asyncio.sleep(POLL_INTERVAL_SECONDS)


async def main(args):
    ctx = NorthgardContext(args.connect, args.password)
    ctx.server_task = asyncio.create_task(server_loop(ctx), name="server loop")
    if gui_enabled:
        ctx.run_gui()
    ctx.run_cli()

    watcher_task = asyncio.create_task(save_watcher(ctx), name="NorthgardSaveWatcher")

    await ctx.exit_event.wait()
    ctx.server_address = None
    await watcher_task
    await ctx.shutdown()


def launch() -> None:
    import colorama

    parser = get_base_parser(description="Northgard Client")
    cli_args = sys.argv[1:]
    if "Northgard Client" in cli_args:
        cli_args.remove("Northgard Client")
    parsed_args, _ = parser.parse_known_args(args=cli_args)

    colorama.init()
    asyncio.run(main(parsed_args))
    colorama.deinit()


if __name__ == "__main__":
    launch()
