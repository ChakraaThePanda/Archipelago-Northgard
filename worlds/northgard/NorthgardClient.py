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

That pin IS remembered across restarts, and separately per Archipelago room: it's keyed
by the room's `seed_name` (a unique id the server sends in its "RoomInfo" packet, one per
generated multiworld), not just a single global "last used save". So running two rooms at
once, each in its own Launcher/client window, each remembers its own pinned save and
they won't stomp on each other. See _room_key / _apply_room_pin below.
"""
from __future__ import annotations

import asyncio
import json
import os
import re
import sys

import Utils
from CommonClient import gui_enabled, logger, get_base_parser, CommonContext, ClientCommandProcessor, server_loop
from NetUtils import ClientStatus

from .Locations import location_table as northgard_location_table
from .save_state import list_conquest_saves, read_conquest_run_state, CHAPTER_ROWS, ConquestSaveSummary

# All Chapter names in tree order, matching Regions.py.
ALL_CHAPTERS: list[str] = [name for row in CHAPTER_ROWS for name in row]

# Resolved once at startup by _resolve_save_dir() (see main()) -- every install has this
# somewhere different, so it's never safe to hardcode. Empty string means "not configured
# yet"; every reader below has to tolerate that instead of assuming a real path.
NORTHGARD_SAVE_DIR: str = ""
POLL_INTERVAL_SECONDS = 5.0


def _known_folder_path(guid_str: str, fallback: str) -> str:
    """Resolves a Windows known-folder GUID via SHGetKnownFolderPath. Deliberately not a
    plain os.path.join(~, "...") -- that would miss it if the user has ever redirected the
    folder to another drive (Windows supports this for Saved Games, same as
    Documents/Pictures), which this machine's owner is exactly the kind of person to have
    done given how spread out their Steam libraries already are."""
    try:
        import ctypes
        from ctypes import wintypes

        class GUID(ctypes.Structure):
            _fields_ = [
                ("Data1", wintypes.DWORD), ("Data2", wintypes.WORD), ("Data3", wintypes.WORD),
                ("Data4", ctypes.c_byte * 8),
            ]

        guid = GUID()
        # ole32's IIDFromString wants the braced "{...}" form; guid_str is passed bare.
        if ctypes.windll.ole32.IIDFromString(ctypes.c_wchar_p(f"{{{guid_str}}}"), ctypes.byref(guid)) != 0:
            return fallback

        path_ptr = ctypes.c_wchar_p()
        result = ctypes.windll.shell32.SHGetKnownFolderPath(ctypes.byref(guid), 0, 0, ctypes.byref(path_ptr))
        if result != 0 or not path_ptr.value:
            return fallback
        path = path_ptr.value
        ctypes.windll.ole32.CoTaskMemFree(path_ptr)
        return path
    except Exception:
        return fallback


_FOLDERID_SAVED_GAMES = "4C5C32FF-BB9D-43b0-B5B4-2D72E54EAAA4"

# Local-only, not part of Northgard's own save data. Two independent things live here:
# - "save_dir": this machine's Northgard save folder (installs land in wildly different
#   places -- different drive, different Steam library, non-Steam copy -- so it's detected
#   once per machine, not assumed, and remembered so you're not asked again).
# - "pins": which conquest save filename you pinned, per Archipelago room, so running
#   several rooms at once doesn't mix up which save belongs to which.
# File shape: {"save_dir": "...", "pins": {"<room_key>": "<filename>", ...}}
#
# Lives under the real Windows "Saved Games" folder (like every other game's save data on
# this machine -- CD Projekt Red, Enshrouded, etc.), not inside Northgard's own install:
# it has to survive a Northgard reinstall/relocation (that's the whole reason /savedir and
# auto-detection exist), and Saved Games isn't something Steam Cloud sync touches the way
# Northgard's own save/ folder is.
_CONFIG_DIR = os.path.join(
    _known_folder_path(_FOLDERID_SAVED_GAMES, os.path.join(os.path.expanduser("~"), "Saved Games")),
    "Archipelago", "Northgard",
)
_CONFIG_FILE = os.path.join(_CONFIG_DIR, "client_config.json")

# One-time migration from the old pre-"Saved Games" location, so upgrading doesn't lose an
# already-detected save_dir or accumulated per-room pins.
_OLD_CONFIG_FILE = os.path.join(os.path.expanduser("~"), ".northgard_ap_client_pin.json")


def _load_config() -> dict:
    try:
        with open(_CONFIG_FILE, "r", encoding="utf-8") as f:
            config = json.load(f)
        return config if isinstance(config, dict) else {}
    except (OSError, json.JSONDecodeError):
        pass

    if os.path.exists(_OLD_CONFIG_FILE):
        try:
            with open(_OLD_CONFIG_FILE, "r", encoding="utf-8") as f:
                config = json.load(f)
            if isinstance(config, dict):
                logger.info(f"[Northgard] Migrating client config from {_OLD_CONFIG_FILE} to {_CONFIG_FILE}")
                _save_config(config)
                return config
        except (OSError, json.JSONDecodeError):
            pass
    return {}


def _save_config(config: dict) -> None:
    try:
        os.makedirs(_CONFIG_DIR, exist_ok=True)
        with open(_CONFIG_FILE, "w", encoding="utf-8") as f:
            json.dump(config, f, indent=2)
    except OSError:
        logger.warning(f"[Northgard] Couldn't persist client config to {_CONFIG_FILE}")


def _room_key(ctx: "NorthgardContext") -> str | None:
    """A stable identifier for 'this Archipelago room', so the save pin can be kept
    separate per room. Prefers seed_name -- a unique id the server sends once per
    generated multiworld (RoomInfo packet), unrelated to Northgard's own Conquest seed --
    since it stays correct even if the same room gets rehosted on a different
    address/port. Falls back to the server address if we haven't seen seed_name yet
    (e.g. /conquest run in the brief window before RoomInfo arrives)."""
    if ctx.seed_name:
        return f"seed:{ctx.seed_name}"
    if ctx.server_address:
        return f"addr:{ctx.server_address}"
    return None


def _load_pins() -> dict[str, str]:
    pins = _load_config().get("pins", {})
    return pins if isinstance(pins, dict) else {}


def _save_pin(room_key: str, filename: str) -> None:
    config = _load_config()
    pins = config.get("pins", {})
    if not isinstance(pins, dict):
        pins = {}
    pins[room_key] = filename
    config["pins"] = pins
    _save_config(config)


def _candidate_steam_library_roots() -> list[str]:
    """Every Steam library folder registered on this machine, best-effort. Reads the
    Steam install path from the registry, then that install's libraryfolders.vdf, which
    lists every additional drive/folder the user has added as a Steam library -- this is
    the same mechanism Steam itself uses, so it finds non-default installs without
    guessing across drive letters."""
    roots: list[str] = []
    try:
        import winreg
        with winreg.OpenKey(winreg.HKEY_CURRENT_USER, r"Software\Valve\Steam") as key:
            steam_path = winreg.QueryValueEx(key, "SteamPath")[0]
    except (ImportError, OSError, FileNotFoundError):
        return roots

    steam_path = os.path.normpath(steam_path)
    roots.append(steam_path)

    vdf_path = os.path.join(steam_path, "steamapps", "libraryfolders.vdf")
    try:
        with open(vdf_path, "r", encoding="utf-8") as f:
            vdf_text = f.read()
    except OSError:
        return roots

    for match in re.finditer(r'"path"\s*"([^"]+)"', vdf_text):
        # libraryfolders.vdf escapes backslashes as \\ -- undo that before using the path.
        roots.append(os.path.normpath(match.group(1).replace("\\\\", "\\")))
    return roots


def _detect_northgard_save_dir() -> str | None:
    for root in _candidate_steam_library_roots():
        candidate = os.path.join(root, "steamapps", "common", "Northgard", "save")
        if os.path.isdir(candidate):
            return candidate
    return None


def _resolve_save_dir() -> str:
    """Figures out this machine's Northgard save folder, in order: previously-configured
    (fastest, and correct even if auto-detect would now guess wrong), then Steam-library
    auto-detection, then a native folder-picker as a last resort so a first-time user is
    never stuck typing a path by hand. Always persists whatever it lands on."""
    config = _load_config()
    configured = config.get("save_dir")
    if configured and os.path.isdir(configured):
        return configured

    detected = _detect_northgard_save_dir()
    if detected:
        logger.info(f"[Northgard] Auto-detected Northgard save folder: {detected}")
        config["save_dir"] = detected
        _save_config(config)
        return detected

    logger.info("[Northgard] Couldn't auto-detect your Northgard install -- opening a folder picker "
                "(pick the 'save' folder inside your Northgard install, e.g. .../Northgard/save).")
    try:
        picked = Utils.open_directory("Select your Northgard 'save' folder")
    except Exception:
        logger.exception("[Northgard] Folder picker failed")
        picked = None

    if picked and os.path.isdir(picked):
        config["save_dir"] = picked
        _save_config(config)
        logger.info(f"[Northgard] Using {picked}. Change it later with /savedir <path>.")
        return picked

    logger.warning(
        "[Northgard] No save folder configured -- checks can't be detected until you set one. "
        "Use /savedir <path to your Northgard 'save' folder>."
    )
    return ""


def _clan_description(clan: str, partner_clan: str | None) -> str:
    """Conquest is co-op against the AI, not PvP -- a second clan is a teammate, joined
    with '&', not an opponent. Solo runs (no second player) show just the one clan."""
    return clan if partner_clan is None else f"{clan} & {partner_clan}"


def _format_age(mtime: float) -> str:
    """Short, human-readable 'when was this last written', precise enough to tell apart
    saves created moments apart (seconds/minutes), but not fussy about older ones."""
    import time
    from datetime import datetime

    delta = time.time() - mtime
    dt = datetime.fromtimestamp(mtime)
    if delta < 120:
        return f"{max(int(delta), 0)}s ago"
    if delta < 3600:
        return f"{int(delta // 60)}m ago"
    if dt.date() == datetime.now().date():
        return dt.strftime("%H:%M today")
    return dt.strftime("%Y-%m-%d %H:%M")


def _format_save_line(index: int, s: ConquestSaveSummary, pinned_filename: str | None) -> str:
    marker = "  <- currently pinned" if s.filename == pinned_filename else ""
    clans = _clan_description(s.clan, s.partner_clan)
    age = _format_age(s.last_modified)
    return f"  [{index}] {s.filename} -- {clans}, {s.chapters_completed} chapters completed, last saved {age}{marker}"


class NorthgardCommandProcessor(ClientCommandProcessor):
    def _cmd_savedir(self, path: str = "") -> bool:
        """Show, pick, or set this machine's Northgard 'save' folder: /savedir with no
        argument opens a folder-picker (or shows the current path if the picker isn't
        available); /savedir <path> sets it directly. This is a per-machine setting, not
        per-room -- set it once and every room reuses it."""
        global NORTHGARD_SAVE_DIR
        path = path.strip().strip('"')

        if not path:
            logger.info(f"[Northgard] Current save folder: {NORTHGARD_SAVE_DIR or '(not set)'}")
            try:
                picked = Utils.open_directory("Select your Northgard 'save' folder", suggest=NORTHGARD_SAVE_DIR)
            except Exception:
                logger.exception("[Northgard] Folder picker failed")
                return False
            if not picked:
                return False
            path = picked

        if not os.path.isdir(path):
            logger.info(f"[Northgard] Not a folder: {path}")
            return False

        NORTHGARD_SAVE_DIR = path
        config = _load_config()
        config["save_dir"] = path
        _save_config(config)
        logger.info(f"[Northgard] Save folder set to {path}")
        return True

    def _cmd_conquest(self, choice: str = "") -> bool:
        """List in-progress Conquest saves, or pin one by number: /conquest 2.
        Required if you have more than one Conquest run going -- there's no way to
        tell them apart automatically. The pin is remembered per-room, so running
        multiple rooms at once (each in its own client window) keeps each one's
        pinned save separate."""
        ctx: NorthgardContext = self.ctx
        if not NORTHGARD_SAVE_DIR:
            logger.info("[Northgard] No save folder configured yet -- run /savedir first.")
            return False
        saves = list_conquest_saves(NORTHGARD_SAVE_DIR)
        if not saves:
            logger.info(f"[Northgard] No conquest saves found under {NORTHGARD_SAVE_DIR}\\conquest")
            return False

        room_key = _room_key(ctx)

        choice = choice.strip()
        if choice:
            if room_key is None:
                logger.info("[Northgard] Not connected to a room yet -- pins are saved per-room, connect first.")
                return False
            if not choice.isdigit() or not (0 <= int(choice) < len(saves)):
                logger.info(f"[Northgard] '{choice}' isn't a valid choice -- run /conquest with no argument to see the list")
                return False
            chosen = saves[int(choice)]
            ctx.pinned_save_path = chosen.path
            ctx.sent_chapters.clear()
            _save_pin(room_key, chosen.filename)
            logger.info(
                f"[Northgard] Pinned to {chosen.filename} ({_clan_description(chosen.clan, chosen.partner_clan)}, "
                f"{chosen.chapters_completed} chapters already completed there) for this room."
            )
            return True

        pinned_filename = os.path.basename(ctx.pinned_save_path) if ctx.pinned_save_path else None
        room_desc = f"{ctx.server_address} ({room_key})" if room_key else "not connected yet"
        logger.info(f"[Northgard] This room: {room_desc}")
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
        self._known_room_key: str | None = None  # room_key we last resolved a pin for

        logger.info("[Northgard] Waiting to connect before resolving this room's pinned save (pins are per-room).")

    def _apply_room_pin(self) -> None:
        """Called once we know this connection's room_key (see RoomInfo handling below).
        Looks up whether we've pinned a save to this specific room before and, if so,
        silently resumes it -- otherwise prompts to use /conquest. Also handles the case
        of reconnecting to a *different* room mid-session (clears sent_chapters so a
        stale in-memory 'already sent' set from a previous room doesn't suppress checks
        in the new one)."""
        room_key = _room_key(self)
        if room_key is None or room_key == self._known_room_key:
            return
        self._known_room_key = room_key
        self.sent_chapters.clear()

        pinned_filename = _load_pins().get(room_key)
        if not pinned_filename:
            self.pinned_save_path = None
            logger.info(f"[Northgard] No conquest save pinned yet for this room ({room_key}) -- use /conquest to pick one.")
            return

        if not NORTHGARD_SAVE_DIR:
            self.pinned_save_path = None
            logger.info(
                f"[Northgard] This room is pinned to {pinned_filename!r}, but no save folder is configured "
                f"yet -- run /savedir, then /conquest to re-pick it."
            )
            return

        match = next((s for s in list_conquest_saves(NORTHGARD_SAVE_DIR) if s.filename == pinned_filename), None)
        if match is not None:
            self.pinned_save_path = match.path
            logger.info(
                f"[Northgard] Resuming this room's pinned save: {match.filename} "
                f"({_clan_description(match.clan, match.partner_clan)}). Use /conquest to change it."
            )
        else:
            self.pinned_save_path = None
            logger.info(
                f"[Northgard] This room was previously pinned to {pinned_filename!r}, but that save "
                f"is no longer found. Use /conquest to pick one."
            )

    async def server_auth(self, password_requested: bool = False):
        if password_requested and not self.password:
            await super().server_auth(password_requested)
        await self.get_username()
        await self.send_connect()

    def on_package(self, cmd: str, args: dict):
        if cmd == "RoomInfo":
            self.seed_name = args.get("seed_name")
            self._apply_room_pin()
        elif cmd == "Connected":
            self.amount_of_locations = args.get("slot_data", {}).get("amount_of_locations", 4)
        elif cmd == "ReceivedItems":
            # NOTE: this is where a received "Chapter N [- Top/Bottom]" item should be
            # applied back to Northgard so that node becomes selectable in-game. Not yet
            # implemented -- see project notes on why this needs confirming against how
            # the Conquest node-select screen actually decides what's choosable before
            # writing anything here. Deliberately silent otherwise: the tree is
            # permissive/simple enough that this doesn't need per-item console noise.
            pass

    def location_ids_for_chapter(self, chapter_name: str) -> list[int]:
        ids = []
        for i in range(1, self.amount_of_locations + 1):
            loc_name = f"{chapter_name} - Item {i:02d}"
            data = northgard_location_table.get(loc_name)
            if data is not None:
                ids.append(data.id)
        return ids


async def save_watcher(ctx: NorthgardContext):
    # No "waiting for a pinned save" nag here: before connecting, there's no room to pin
    # against yet (see _room_key), so any such message would be telling you to run
    # /conquest before that's actually possible. _apply_room_pin already logs the correct,
    # room-aware version of this ("no save pinned for this room -- use /conquest") right
    # when a connection's RoomInfo arrives.
    while not ctx.exit_event.is_set():
        try:
            if ctx.pinned_save_path is None:
                pass
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
                        await ctx.send_msgs([{"cmd": "LocationChecks", "locations": location_ids}])

                    if not ctx.finished_game and "Chapter 07" in ctx.sent_chapters:
                        ctx.finished_game = True
                        await ctx.send_msgs([{"cmd": "StatusUpdate", "status": ClientStatus.CLIENT_GOAL}])
        except Exception:
            logger.exception("[Northgard] save_watcher iteration failed")

        await asyncio.sleep(POLL_INTERVAL_SECONDS)


async def main(args):
    global NORTHGARD_SAVE_DIR
    NORTHGARD_SAVE_DIR = _resolve_save_dir()

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
