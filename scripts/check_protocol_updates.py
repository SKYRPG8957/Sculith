#!/usr/bin/env python3
"""Compare upstream release references without claiming or enabling compatibility."""

import json
from pathlib import Path
import sys
import urllib.request


ROOT = Path(__file__).resolve().parents[1]
SOURCES = {
    "java": "https://raw.githubusercontent.com/PrismarineJS/minecraft-data/master/data/pc/common/protocolVersions.json",
    "bedrock": "https://raw.githubusercontent.com/PrismarineJS/minecraft-data/master/data/bedrock/common/protocolVersions.json",
}


def main():
    reference = json.loads((ROOT / "protocol-targets.json").read_text(encoding="utf-8"))
    changed = False
    for edition, url in SOURCES.items():
        request = urllib.request.Request(url, headers={"User-Agent": "Sculith-protocol-reference-check"})
        with urllib.request.urlopen(request, timeout=20) as response:
            entries = json.load(response)
        # Upstream lists newest releases first. Snapshots/previews are intentionally excluded.
        latest = next(entry for entry in entries if entry.get("releaseType") == "release")
        current = reference[edition]
        different = current["version"] != latest["minecraftVersion"] or current["protocol"] != latest["version"]
        changed |= different
        print(f"{edition}: recorded {current['version']}/{current['protocol']}, "
              f"upstream {latest['minecraftVersion']}/{latest['version']} "
              f"({'update needed' if different else 'unchanged'})")
    print("No files changed. Gameplay support requires adapter implementation and client validation.")
    return 1 if changed else 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, ValueError, KeyError, StopIteration) as error:
        print(f"Could not verify protocol references: {error}", file=sys.stderr)
        sys.exit(2)
