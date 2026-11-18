"""Exercise a Bedrock geometry edit with both edition offers already sent."""

import json
import subprocess
import time
import zipfile

from shared_resources import Java

GEOMETRY = "models/entity/sculith_statue.geo.json"
JAVA_MODEL = "assets/sculith/models/sculith/demo_statue.json"


def check(java_port, bedrock_port, root, probe, original_sha1):
    before, after = root / "resources-before-edit", root / "resources-after-edit"
    before.mkdir()
    after.mkdir()
    bedrock = str(probe), f"http://127.0.0.1:{bedrock_port}", "--pack-output"
    bedrock_before = before / "pack.mcpack"
    ready = root / "resume-pack-download"
    pending_java = None
    with (before / "bedrock.log").open("w") as log:
        pending = subprocess.Popen([*bedrock, str(bedrock_before), str(ready)], stdout=log, stderr=log)
        try:
            deadline = time.monotonic() + 8
            while "PACK OFFER " not in (before / "bedrock.log").read_text():
                assert pending.poll() is None and time.monotonic() < deadline, (before / "bedrock.log").read_text()
                time.sleep(.05)

            def edit_on_offer():
                editor = Java(java_port, "PackEditor")
                try:
                    new = editor.join_with_resources(java_port, after, False)
                    assert new["sha1"] != original_sha1, new
                finally:
                    editor.socket.close()
                ready.touch()

            pending_java = Java(java_port, "PackPending")
            old = pending_java.join_with_resources(java_port, before, False, before_download=edit_on_offer)
            assert old["sha1"] == original_sha1, old
            assert pending.wait(timeout=20) == 0, (before / "bedrock.log").read_text()
        finally:
            if pending_java is not None:
                pending_java.socket.close()
            if pending.poll() is None:
                pending.kill()
                pending.wait(timeout=5)

    bedrock_after = after / "pack.mcpack"
    result = subprocess.run([*bedrock, str(bedrock_after)], capture_output=True, text=True, timeout=25)
    (after / "bedrock.log").write_text(result.stdout + result.stderr)
    assert result.returncode == 0, result.stdout + result.stderr
    assert bedrock_before.read_bytes() == (root / "bedrock.mcpack").read_bytes()
    assert (before / "java.zip").read_bytes() == (root / "java.zip").read_bytes()
    with zipfile.ZipFile(bedrock_before) as old, zipfile.ZipFile(bedrock_after) as new:
        assert new.read("manifest.json") != old.read("manifest.json")
        assert "rejected.txt" not in new.namelist()
        geometry = json.loads(new.read(GEOMETRY))
        assert geometry["minecraft:geometry"][0]["bones"][1]["cubes"][0]["origin"] == [-8, 0, -2]
    with zipfile.ZipFile(after / "java.zip") as archive:
        assert json.loads(archive.read(JAVA_MODEL))["elements"][0]["from"] == [0, 0, 6]
    assert (root / "source/manifest.json").read_text() == "not a valid replacement pack"
    geometry = json.loads((root / "source" / GEOMETRY).read_text())
    assert geometry["minecraft:geometry"][0]["bones"][1]["cubes"][0]["origin"] == [-8, 0, -4]
    assert "PROBE_PACK_EDIT old revision survived validation; geometry updated" in (root / "server.log").read_text()
    return "old offers completed on both editions; new joins got translated geometry; invalid edit left the pack intact"
