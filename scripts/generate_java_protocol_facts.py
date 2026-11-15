#!/usr/bin/env python3
"""Extract Java 26.3 wire identifiers from the checksum-pinned official report tool.

Development only. Never bundles the Minecraft JAR, implementation, textures or
registry element definitions. Runtime uses the client's negotiated vanilla core.
"""
import argparse
import hashlib
import io
import json
from pathlib import Path
import zipfile

VERSION = "26.3"
PROTOCOL = 777
SERVER_SHA1 = "33680f5f2ac32864d6d7cf5e56a705fdb3e05f4c"
# Synchronized registries cross-checked with MIT MCProtocolLib 65efb9da's
# protocol-777 configuration codec/networkCodec fixture; names only are facts.
SYNCED = """
banner_pattern block_transformer cat_sound_variant cat_variant chat_type
chicken_sound_variant chicken_variant cow_sound_variant cow_variant damage_type
decorated_pot_pattern dialog dimension_type enchantment frog_variant instrument
jukebox_song painting_variant pig_sound_variant pig_variant sulfur_cube_archetype
test_environment test_instance timeline trim_material trim_pattern
wolf_sound_variant wolf_variant world_clock worldgen/biome
worldgen/block_state_provider zombie_nautilus_variant
""".split()


def generate(server, reports):
    with server.open("rb") as source:
        if hashlib.file_digest(source, "sha1").hexdigest() != SERVER_SHA1:
            raise ValueError("Expected the pinned official Java 26.3 server report tool")
    with zipfile.ZipFile(server) as wrapper:
        inner = wrapper.read("META-INF/versions/26.3/server-26.3.jar")
    with zipfile.ZipFile(io.BytesIO(inner)) as core:
        paths = core.namelist()
        registries = {}
        ids = {}
        for kind in SYNCED:
            prefix = "data/minecraft/" + kind + "/"
            names = sorted("minecraft:" + path[len(prefix):-5] for path in paths
                           if path.startswith(prefix) and path.endswith(".json"))
            if not names or len(names) > 4096:
                raise ValueError("Unexpected synchronized registry: " + kind)
            key = "minecraft:" + kind
            registries[key] = names
            ids[key] = {name: i for i, name in enumerate(names)}
        static = json.loads((reports / "registries.json").read_text(encoding="utf-8"))
        for key, value in static.items():
            ids[key] = {name: entry["protocol_id"] for name, entry in value["entries"].items()}
        tags = {}
        for key, entries in ids.items():
            prefix = "data/minecraft/tags/" + key.split(":", 1)[1] + "/"
            definitions = {
                "minecraft:" + p[len(prefix):-5]: json.loads(core.read(p))
                for p in paths if p.startswith(prefix) and p.endswith(".json")
            }
            resolved = {}

            def resolve(name, active):
                if name in resolved:
                    return resolved[name]
                if name in active:
                    raise ValueError("Cyclic core tag: " + name)
                values = set()
                for item in definitions[name]["values"]:
                    required = True
                    if isinstance(item, dict):
                        required = item.get("required", True)
                        item = item["id"]
                    if item.startswith("#"):
                        target = item[1:]
                        if target in definitions:
                            values.update(resolve(target, active | {name}))
                        elif required:
                            raise ValueError("Unresolved required core tag: " + item)
                    elif item in entries:
                        values.add(entries[item])
                    elif required:
                        raise ValueError("Unresolved core element: " + key + "/" + item)
                resolved[name] = sorted(values)
                return resolved[name]

            for name in sorted(definitions):
                resolve(name, set())
            if resolved:
                tags[key] = resolved
    blocks_report = json.loads((reports / "blocks.json").read_text(encoding="utf-8"))
    blocks = {}
    for name in ["air", "bedrock", "stone", "dirt", "grass_block"]:
        key = "minecraft:" + name
        state = next(s["id"] for s in blocks_report[key]["states"] if s.get("default"))
        blocks[name] = {"state": state, "item": ids["minecraft:item"][key]}
    packets = json.loads((reports / "packets.json").read_text(encoding="utf-8"))
    packets = {state: {direction: {name: v["protocol_id"] for name, v in values.items()}
                       for direction, values in directions.items()}
               for state, directions in packets.items()}
    return {"version": VERSION, "protocol": PROTOCOL, "server_jar_sha1": SERVER_SHA1,
            "registries": registries, "tags": tags, "blocks": blocks, "packets": packets}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--server-jar", type=Path, required=True)
    parser.add_argument("--reports", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    facts = generate(args.server_jar, args.reports)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(facts, sort_keys=True, separators=(",", ":")) + "\n", encoding="utf-8")
    print(f"Generated {VERSION}/{PROTOCOL}: {len(facts['registries'])} registries, "
          f"{sum(len(t) for t in facts['tags'].values())} tags; {args.output.stat().st_size} bytes")
