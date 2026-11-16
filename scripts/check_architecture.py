#!/usr/bin/env python3
"""Check dependency direction between engine crates and plugin examples."""
import json
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parents[1]
ALLOWED = {
    "sculith-core": set(),
    "sculith-world": set(),
    "sculith-packs": {"sculith-core"},
    "sculith-plugin-api": {"sculith-core"},
    "sculith-game": {"sculith-core", "sculith-world"},
    "sculith-runtime": {"sculith-core", "sculith-world", "sculith-game"},
    "sculith-plugin-host": {
        "sculith-core", "sculith-world", "sculith-game",
        "sculith-runtime", "sculith-plugin-api",
    },
}
ALLOWED["sculith-server"] = set(ALLOWED)
GAME_FORBIDDEN = {"bedrock", "raknet", "nethernet", "tokio", "hyper", "ureq"}


def main():
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--no-deps", "--format-version", "1", "--locked"], cwd=ROOT))
    members = set(metadata["workspace_members"])
    packages = [p for p in metadata["packages"] if p["id"] in members]
    names = {p["name"] for p in packages}
    errors = []
    for package in packages:
        name = package["name"]
        if Path(package["manifest_path"]).is_relative_to(ROOT / "crates") and name not in ALLOWED:
            errors.append(f"{name}: define its dependency boundary before adding an engine crate")
        allowed = ALLOWED.get(name, {"sculith-plugin-api"})
        dependencies = {d["name"] for d in package["dependencies"]}
        unexpected = (dependencies & names) - allowed
        if unexpected:
            errors.append(f"{name}: forbidden workspace dependencies: {', '.join(sorted(unexpected))}")
        if name == "sculith-game" and dependencies & GAME_FORBIDDEN:
            errors.append("sculith-game: game decisions must remain independent of transport/runtime libraries")
    if errors:
        raise SystemExit("\n".join(errors))
    print(f"Architecture OK: {len(ALLOWED)} engine crates; plugin examples depend on the SDK")


if __name__ == "__main__":
    main()
