"""Reject a Bedrock source directory before changing any world file."""
import hashlib
import os
from pathlib import Path
import subprocess
import sys
import tempfile

server = Path(sys.argv[1]).resolve()
with tempfile.TemporaryDirectory(prefix="sculith-world-guard-") as directory:
    root = Path(directory)
    world = root / "world"
    (world / "db").mkdir(parents=True)
    (world / "db" / "CURRENT").write_bytes(b"MANIFEST-000001\n")
    (world / "level.dat").write_bytes(b"BEDROCK_SOURCE_SENTINEL")

    def snapshot():
        return {
            str(p.relative_to(world)): hashlib.sha256(p.read_bytes()).hexdigest()
            for p in world.rglob("*")
            if p.is_file()
        }

    before = snapshot()
    imported = root / "imported"
    result = subprocess.run(
        [str(server), "--import-bedrock-terrain", str(world), str(imported)], cwd=root,
        capture_output=True, text=True, timeout=20,
    )
    assert result.returncode == 1, (result.returncode, result.stdout, result.stderr)
    assert "Bedrock terrain import failed" in result.stderr
    assert not imported.exists()
    assert not (root / "pumpkin.toml").exists()
    assert snapshot() == before, "source world was modified"

    result = subprocess.run(
        [str(server)], cwd=root, env=dict(os.environ, RUST_LOG="error"),
        capture_output=True, text=True, timeout=20,
    )
    output = result.stdout + result.stderr
    assert result.returncode == 1, (result.returncode, output)
    assert "This is a Bedrock LevelDB world" in output, output
    assert snapshot() == before, "source world was modified"
print("PASS: direct Bedrock open and rejected terrain import preserve source files")
