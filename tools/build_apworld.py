"""
Packages worlds/northgard/ into northgard.apworld for a release.

worlds/northgard/archipelago.json is a template committed to the repo -- it carries
the fields that don't change release to release (game, authors, minimum_ap_version,
compatible_version). This script stamps in the one field that does change,
world_version, and writes the result into the zip; the template on disk is never
modified.

Usage:
    python tools/build_apworld.py <version>

Example:
    python tools/build_apworld.py 2026.07.29

world_version must be exactly 3 dot-separated integers (major.minor.build) --
Archipelago core's Version is a fixed 3-field tuple, and a 4th segment crashes every
player's client trying to load the world. The git tag you release under doesn't have
to match this value, only the embedded world_version is constrained.

Produces ./northgard.apworld (gitignored release artifact, matching the tag you'll
create for the release).
"""
from __future__ import annotations

import argparse
import json
import pathlib
import re
import zipfile

ROOT = pathlib.Path(__file__).resolve().parent.parent
WORLD_DIR = ROOT / "worlds" / "northgard"
MANIFEST_TEMPLATE = WORLD_DIR / "archipelago.json"
OUTPUT = ROOT / "northgard.apworld"

parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
parser.add_argument("version", help="world_version for this release -- exactly 3 dot-separated integers, e.g. 2026.07.29")
args = parser.parse_args()

if not re.fullmatch(r"\d+\.\d+\.\d+", args.version):
    parser.error(
        f"{args.version!r} must be exactly 3 dot-separated integers (major.minor.build) -- "
        "Archipelago core's Version tuple can't hold a 4th segment and will crash loading this world."
    )

manifest = json.loads(MANIFEST_TEMPLATE.read_text(encoding="utf-8"))
manifest["world_version"] = args.version

OUTPUT.unlink(missing_ok=True)
with zipfile.ZipFile(OUTPUT, "w", zipfile.ZIP_DEFLATED, compresslevel=9) as zf:
    for path in sorted(WORLD_DIR.rglob("*")):
        if path.is_dir() or "__pycache__" in path.parts or path == MANIFEST_TEMPLATE:
            continue
        zf.write(path, pathlib.Path("northgard") / path.relative_to(WORLD_DIR))
    zf.writestr("northgard/archipelago.json", json.dumps(manifest, indent=2))

print(f"Built {OUTPUT} ({OUTPUT.stat().st_size} bytes) with world_version={args.version}")
