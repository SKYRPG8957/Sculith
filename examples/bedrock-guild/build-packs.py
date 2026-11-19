"""Build the native Bedrock pack and a Java transport pack carrying the identical Bedrock UI."""
from pathlib import Path
import argparse
import hashlib
import json
from zipfile import ZIP_DEFLATED, ZipFile, ZipInfo


def archive(path, entries):
    with ZipFile(path, "w", compression=ZIP_DEFLATED) as output:
        for name, data in sorted(entries.items()):
            entry = ZipInfo(name, (2026, 1, 1, 0, 0, 0))
            entry.compress_type = ZIP_DEFLATED
            output.writestr(entry, data)


def build(source, destination, vanilla=False):
    destination.mkdir(parents=True, exist_ok=True)
    bedrock = {f.relative_to(source).as_posix(): f.read_bytes() for f in source.rglob("*") if f.is_file()}
    form = json.loads(bedrock["ui/server_form.json"])
    if form["namespace"] != "server_form":
        raise ValueError("Expected a Bedrock server_form namespace")
    java = {"pack.mcmeta": json.dumps({"pack": {"description": "Sculith Bedrock UI", "min_format": [97, 1], "max_format": [97, 1]}}).encode()}
    for name, data in bedrock.items():
        if name.startswith("ui/") and name.endswith(".json"):
            java["assets/sculith/bedrock_ui/" + name.removeprefix("ui/")] = data
        elif name.startswith("textures/") and name.endswith(".png"):
            java["assets/sculith/" + name] = data
    if vanilla:
        from vanilla_form import compile_form
        assets, dialog = compile_form(bedrock, ["의뢰 수락", "내 의뢰", "닫기"])
        java.update(assets)
        (destination / "guild-dialog.json").write_text(dialog, encoding="utf-8")
        # Keep native UI art and Java bitmap atlases in one source for the built-in pack host.
        stage = destination / "shared" / "bedrock"
        mapping = json.loads((Path(__file__).parent / "mapping.json").read_text(encoding="utf-8"))
        mapping["form_fonts"] = ["guild_vanilla"]
        shared_files = dict(bedrock)
        for path, data in assets.items():
            resource = path.removeprefix("assets/sculith/")
            if resource.startswith("textures/"):
                mapping["textures"].append({"bedrock": resource, "java": "sculith:" + resource[9:-4]})
            else:
                resource = "sculith/fonts/guild_vanilla.json"
            shared_files[resource] = data
        for file, data in shared_files.items():
            target = stage / file
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(data)
        (stage.parent / "mapping.json").write_text(json.dumps(mapping, indent=2) + "\n", encoding="utf-8")
    manifest = json.loads(bedrock["manifest.json"])["header"]
    be = destination / "guild.mcpack"
    je = destination / "guild-java.zip"
    archive(be, bedrock)
    archive(je, java)
    info = {"uuid": manifest["uuid"], "version": ".".join(map(str, manifest["version"])), "bedrock_bytes": be.stat().st_size,
            "bedrock_sha256": hashlib.sha256(be.read_bytes()).hexdigest(), "java_sha1": hashlib.sha1(je.read_bytes()).hexdigest(),
            "java_sha256": hashlib.sha256(je.read_bytes()).hexdigest()}
    (destination / "packs.json").write_text(json.dumps(info, indent=2) + "\n")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("destination", type=Path)
    parser.add_argument("--source", type=Path, default=Path(__file__).resolve().parent / "bedrock")
    parser.add_argument("--vanilla", action="store_true", help="Also compile the Java 26.3 bitmap menu (requires Pillow)")
    args = parser.parse_args()
    build(args.source, args.destination, args.vanilla)
