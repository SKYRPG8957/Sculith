#!/usr/bin/env python3
"""Package the Windows preview and the licenses of its locked dependencies."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import zipfile

ROOT = Path(__file__).resolve().parents[1]
TARGET = "x86_64-pc-windows-msvc"


def dependency_notices():
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--locked", "--format-version", "1", "--filter-platform", TARGET],
        cwd=ROOT, text=True, encoding="utf-8",
    ))
    nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
    roots = {"sculith-server", "sculith-gameplay-plugin", "sculith-forms-plugin", "sculith-sound-plugin"}
    selected, pending = set(), [p["id"] for p in metadata["packages"] if p["name"] in roots]
    if len(pending) != len(roots):
        raise RuntimeError("Missing server or bundled plugin in the locked metadata")
    while pending:
        item = pending.pop()
        if item not in selected:
            selected.add(item)
            pending.extend(dep["pkg"] for dep in nodes[item]["deps"])
    records = ["Sculith Windows preview: locked dependency license texts.\n"
               "Includes build and test dependencies conservatively.\n"]
    for package in sorted(metadata["packages"], key=lambda p: (p["name"], p["version"], p["id"])):
        root = Path(package["manifest_path"]).parent
        vendored = root.is_relative_to(ROOT / "vendor")
        if package["id"] not in selected or (package["source"] is None and not vendored):
            continue
        source = package["source"] or package["metadata"].get("sculith", {}).get("upstream", "vendored source")
        parents = [root, *list(root.parents)[:2]] if source.startswith("git+") else [root]
        files = set()
        for parent in parents:
            files.update(p for p in parent.iterdir() if p.is_file() and p.name.upper().startswith(
                ("LICENSE", "LICENCE", "COPYING", "NOTICE", "COPYRIGHT", "AUTHORS")))
        if package["license_file"]:
            files.add(root / package["license_file"])
        selection = ""
        if not files:
            # Some crate tarballs declare a dual license but omit its text.
            # Select their Apache-2.0 option, whose unmodified text is retained.
            expression = (package["license"] or "").replace(" ", "")
            if expression in {"MITORApache-2.0", "Apache-2.0ORMIT", "MIT/Apache-2.0"}:
                files.add(ROOT / "LICENSES/Apache-2.0.txt")
                selection = "Distribution uses the declared Apache-2.0 option.\n"
            elif package["name"] == "mutants" and package["version"] == "0.0.3":
                files.add(ROOT / "LICENSES/package-fallbacks/cargo-mutants-MIT.txt")
                selection = "License retained from source commit 9685de346012afb9c379432eaf9c6960e22185df.\n"
            else:
                raise RuntimeError(f"Missing license text: {package['name']} {package['version']}")
        header = (f"\n{'=' * 72}\n{package['name']} {package['version']}\n"
                  f"Source: {source}\nRepository: {package['repository']}\n"
                  f"Declared license: {package['license']}\nAuthors: {', '.join(package['authors'])}\n{selection}")
        records.append(header)
        for path in sorted(files):
            if path.stat().st_size > 2 * 1024 * 1024:
                raise RuntimeError(f"Unexpectedly large license: {path}")
            records.append(f"\n--- {path.name} ---\n" + path.read_text(encoding="utf-8", errors="replace") + "\n")
    return "".join(records)


def package(binary, version, output, plugin=None, forms_plugin=None, sound_plugin=None):
    if not re.fullmatch(r"v\d+\.\d+\.\d+-preview\.\d+", version):
        raise ValueError("Expected vMAJOR.MINOR.PATCH-preview.NUMBER")
    binary = binary.resolve(strict=True)
    if binary.read_bytes()[:2] != b"MZ":
        raise ValueError("Expected a Windows executable")
    plugin = (plugin or binary.with_name("sculith-gameplay-plugin.exe")).resolve(strict=True)
    if plugin.read_bytes()[:2] != b"MZ":
        raise ValueError("Expected the Windows gameplay plugin executable")
    forms_plugin = (forms_plugin or binary.with_name("sculith-forms-plugin.exe")).resolve(strict=True)
    if forms_plugin.read_bytes()[:2] != b"MZ":
        raise ValueError("Expected the Windows forms plugin executable")
    sound_plugin = (sound_plugin or binary.with_name("sculith-sound-plugin.exe")).resolve(strict=True)
    if sound_plugin.read_bytes()[:2] != b"MZ":
        raise ValueError("Expected the Windows sound plugin executable")
    output.mkdir(parents=True, exist_ok=True)
    archive = output / f"sculith-{version}-windows-x86_64.zip"
    checksum = output / "SHA256SUMS.txt"
    if archive.exists() or checksum.exists():
        raise FileExistsError("Release output already exists; use a new output directory")
    commit = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
    with tempfile.TemporaryDirectory(prefix="sculith-package-") as temporary:
        stage = Path(temporary)
        shutil.copyfile(binary, stage / "sculith.exe")
        (stage / "plugins").mkdir()
        shutil.copyfile(plugin, stage / "plugins/sculith-gameplay-plugin.exe")
        shutil.copyfile(forms_plugin, stage / "plugins/sculith-forms-plugin.exe")
        shutil.copyfile(sound_plugin, stage / "plugins/sculith-sound-plugin.exe")
        shutil.copytree(ROOT / "examples/pack/bedrock", stage / "packs/sculith-demo")
        shutil.copyfile(ROOT / "examples/pack/textures.json", stage / "packs/sculith-demo-textures.json")
        for source in (ROOT / "distribution/windows").iterdir():
            if source.is_file():
                shutil.copyfile(source, stage / source.name)
        for name in ["LICENSE", "THIRD_PARTY_NOTICES.md", "Cargo.lock"]:
            shutil.copyfile(ROOT / name, stage / name)
        (stage / "THIRD_PARTY_LICENSES.txt").write_text(dependency_notices(), encoding="utf-8")
        (stage / "release.json").write_text(json.dumps({
            "version": version, "commit": commit, "target": TARGET,
            "stage": "development-preview", "official_client_verified": False,
        }, indent=2) + "\n", encoding="utf-8")
        with zipfile.ZipFile(archive, "x", compression=zipfile.ZIP_DEFLATED) as bundle:
            for path in sorted(stage.rglob("*")):
                if not path.is_file():
                    continue
                name = path.relative_to(stage).as_posix()
                if path.suffix == ".cmd":
                    bundle.writestr(name, path.read_text(encoding="utf-8").replace("\n", "\r\n"))
                else:
                    bundle.write(path, name)
    with archive.open("rb") as stream:
        digest = hashlib.file_digest(stream, "sha256").hexdigest()
    checksum.write_text(f"{digest}  {archive.name}\n", encoding="ascii")
    print(f"Packaged {archive.name}; commit {commit}; SHA-256 {digest}")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--plugin", type=Path, help="Defaults to the gameplay EXE beside the server")
    parser.add_argument("--forms-plugin", type=Path, help="Defaults to the forms EXE beside the server")
    parser.add_argument("--sound-plugin", type=Path, help="Defaults to the sound EXE beside the server")
    parser.add_argument("--version", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    package(args.binary, args.version, args.output, args.plugin, args.forms_plugin, args.sound_plugin)
