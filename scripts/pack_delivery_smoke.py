#!/usr/bin/env python3
"""Independent archive/hash/HTTP and two-edition transfer checks, without game rendering."""
import argparse
import hashlib
import io
import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import tempfile
import time
from urllib.error import HTTPError
from urllib.request import Request, urlopen
import uuid
import zipfile

from java_smoke import Client, receive, send, vi

ROOT = Path(__file__).resolve().parents[1]


def smoke(server, client, wait_for_keepalive=False):
    with tempfile.TemporaryDirectory(prefix="sculith-pack-transfer-") as temporary:
        root = Path(temporary)
        source = root / "original source"
        shutil.copytree(ROOT / "examples/pack/bedrock", source)
        # More than two network chunks, independent of compression assumptions.
        (source / "credits.bin").write_bytes(bytes(range(256)) * 300)
        original = {p.relative_to(source).as_posix(): p.read_bytes() for p in source.rglob("*") if p.is_file()}
        mapping = ROOT / "examples/pack/textures.json"
        expected = root / "expected"
        subprocess.run([str(server), "pack", "bundle", "--input", str(source), "--mapping", str(mapping), "--output", str(expected)], check=True, capture_output=True)
        metadata = json.loads((expected / "bundle.json").read_text())
        native_bytes = (expected / "bedrock.mcpack").read_bytes()
        java_bytes = (expected / "java.zip").read_bytes()
        assert len(native_bytes) > 2 * 32768
        for data, hash_ in [(native_bytes, metadata["bedrock_sha256"]), (java_bytes, metadata["java_sha256"])]:
            assert hashlib.sha256(data).hexdigest() == hash_
            with zipfile.ZipFile(io.BytesIO(data)) as z:
                assert z.testzip() is None
                assert z.namelist() == sorted(z.namelist())
                assert all(e.compress_type == zipfile.ZIP_STORED and e.date_time == (1980, 1, 1, 0, 0, 0) for e in z.infolist())
        with zipfile.ZipFile(io.BytesIO(native_bytes)) as z:
            native_manifest = json.loads(z.read("manifest.json"))
            assert native_manifest["header"]["uuid"] == metadata["bedrock_id"]
            assert native_manifest["header"]["uuid"] != json.loads(original["manifest.json"])["header"]["uuid"]
            assert z.read("credits.bin") == original["credits.bin"]
        with zipfile.ZipFile(io.BytesIO(java_bytes)) as z:
            assert z.read("assets/minecraft/textures/block/stone.png") == original["textures/blocks/sculith_demo.png"]
            assert json.loads(z.read("pack.mcmeta"))["pack"]["min_format"] == [97, 1]
        assert hashlib.sha1(java_bytes).hexdigest() == metadata["java_sha1"]
        settings = root / "server.toml"
        settings.write_text('[java]\nsessions = true\nrequire_authentication = false\nlogin_timeout_secs = 60\n'
                            '[bedrock]\nsessions = true\nrequire_authentication = false\nlogin_timeout_secs = 30\n'
                            '[network.java]\nbind = "127.0.0.1:0"\n[network.bedrock]\nbind = "127.0.0.1:0"\n'
                            '[content]\nsource = ' + json.dumps(str(source)) + '\nmapping = ' + json.dumps(str(mapping)) + '\n', encoding="utf-8")
        log = root / "server.log"
        with log.open("w", encoding="utf-8") as out:
            process = subprocess.Popen([str(server), "run", "--config", str(settings)], cwd=root,
                                       stdout=out, stderr=out, env=dict(os.environ, NO_COLOR="1", RUST_LOG="info"),
                                       creationflags=subprocess.CREATE_NEW_PROCESS_GROUP if os.name == "nt" else 0)
            try:
                deadline = time.monotonic() + 8
                while time.monotonic() < deadline:
                    text = log.read_text(encoding="utf-8")
                    http = re.search(r'url=(http://127\.0\.0\.1:\d+/packs/[0-9a-f]+\.zip)', text)
                    java = re.search(r'Java TCP discovery listening.*?127\.0\.0\.1:(\d+)', text)
                    native = re.search(r'NetherNet HTTP signaling listening.*?127\.0\.0\.1:(\d+)', text)
                    if http and java and native: break
                    assert process.poll() is None, text
                    time.sleep(.05)
                else: raise TimeoutError(text)
                url = http[1]
                with urlopen(Request(url, method="HEAD"), timeout=3) as response:
                    assert response.status == 200 and int(response.headers["Content-Length"]) == len(java_bytes)
                    assert response.read() == b""
                for suffix in ("?download=1", "/../world/identity.pem"):
                    try: urlopen(url + suffix, timeout=3)
                    except HTTPError as error: assert error.code == 404
                    else: raise AssertionError("unexpected pack HTTP route")
                received = []
                def accept(reader, sock):
                    id_ = reader.take(16)
                    assert str(uuid.UUID(bytes=id_)) == metadata["java_id"]
                    assert reader.string() == url and reader.string() == metadata["java_sha1"]
                    assert reader.take(2) == b"\x01\x00"
                    deferred_keepalive = None
                    if wait_for_keepalive:
                        sock.settimeout(12)
                        # Keep the consent response pending beyond the official
                        # game's 30-second inactivity timeout, with no fake load.
                        for index in range(4):
                            packet_id, keepalive = receive(sock)
                            assert packet_id == 4
                            payload = keepalive.take(8)
                            keepalive.end()
                            if index == 3:
                                deferred_keepalive = payload
                            else:
                                send(sock, 4, payload)
                    send(sock, 6, id_ + vi(3))
                    with urlopen(url, timeout=3) as response:
                        data = response.read()
                    assert data == java_bytes
                    received.append(data)
                    send(sock, 6, id_ + vi(4))
                    send(sock, 6, id_ + vi(0))
                    if deferred_keepalive is not None:
                        # Loading may finish while a liveness reply is in flight.
                        send(sock, 4, deferred_keepalive)
                java_client = Client(("127.0.0.1", int(java[1])), pack_handler=accept)
                java_client.socket.close()
                assert len(received) == 1
                class ExpectedRejection(Exception):
                    pass
                for invalid_case in ("decline", "wrong_id", "load_before_accept", "reload_failure"):
                    def reject(reader, sock):
                        pack_id = reader.take(16)
                        reader.string(); reader.string(); reader.take(2)
                        try:
                            if invalid_case == "wrong_id":
                                send(sock, 6, bytes(16) + vi(3))
                            elif invalid_case == "reload_failure":
                                send(sock, 6, pack_id + vi(3))
                                send(sock, 6, pack_id + vi(6))
                            else:
                                send(sock, 6, pack_id + vi(1 if invalid_case == "decline" else 0))
                            packet_id, _ = receive(sock)
                            assert packet_id == 2, (invalid_case, packet_id)
                            raise ExpectedRejection()
                        finally:
                            sock.close()
                    try:
                        Client(("127.0.0.1", int(java[1])), pack_handler=reject)
                    except ExpectedRejection:
                        pass
                    else:
                        raise AssertionError("required pack failure was admitted: " + invalid_case)
                for index in range(2):
                    downloaded = root / f"native-{index}.mcpack"
                    result = subprocess.run([str(client), "http://127.0.0.1:" + native[1], "--pack-output", str(downloaded)],
                                            capture_output=True, text=True, encoding="utf-8", timeout=40)
                    assert result.returncode == 0, result.stdout + result.stderr
                    assert "PASS: received Bedrock resource pack" in result.stdout
                    assert downloaded.read_bytes() == native_bytes
                assert "cached=true" in log.read_text(encoding="utf-8"), "cached pack path was not used"
                assert {p.relative_to(source).as_posix(): p.read_bytes() for p in source.rglob("*") if p.is_file()} == original
                process.send_signal(signal.CTRL_BREAK_EVENT if os.name == "nt" else signal.SIGINT)
                assert process.wait(timeout=5) == 0
            except Exception:
                print(log.read_text(encoding="utf-8"))
                raise
            finally:
                if process.poll() is None:
                    process.kill()
                    process.wait(timeout=5)
    print("PASS: immutable native/Java archives, independent ZIP/CRC/hash checks, bounded HTTP routes, Java required-pack acknowledgement, Bedrock multi-chunk transfer and cached reconnect")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    suffix = ".exe" if os.name == "nt" else ""
    parser.add_argument("--server", type=Path, default=ROOT / ("target/debug/sculith" + suffix))
    parser.add_argument("--client", type=Path, required=True)
    parser.add_argument("--wait-for-keepalive", action="store_true")
    args = parser.parse_args()
    smoke(args.server.resolve(strict=True), args.client.resolve(strict=True), args.wait_for_keepalive)
