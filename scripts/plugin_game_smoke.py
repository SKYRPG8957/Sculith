#!/usr/bin/env python3
"""Real-process Rust plugin game hooks, independent Java and Bedrock wire clients.

This is not an official Minecraft graphical-client test.
"""
import argparse
import json
import os
from pathlib import Path
import re
import signal
import shutil
import subprocess
import tempfile
import time

import java_smoke as wire

ROOT = Path(__file__).resolve().parents[1]


def nbt_text(reader):
    assert reader.number(">B") == 8, "expected a literal NBT text component"
    raw = reader.take(reader.number(">H"))
    # Java NBT uses modified UTF-8, including UTF-16 surrogate pairs for emoji.
    assert not any(byte >= 0xf0 for byte in raw), "Java NBT cannot contain four-byte UTF-8 sequences"
    return raw.replace(b"\xc0\x80", b"\0").decode("utf-8", "surrogatepass").encode(
        "utf-16", "surrogatepass").decode("utf-16")


def smoke(server, plugin, client, api_version=5):
    with tempfile.TemporaryDirectory(prefix="sculith game plugin ") as temporary:
        root = Path(temporary)
        staged_plugin = root / "plugins with spaces" / plugin.name
        staged_plugin.parent.mkdir()
        shutil.copy2(plugin, staged_plugin)
        settings = root / "server.toml"
        settings.write_text(
            '[server]\nmotd = "Sculith / plugin game smoke"\n'
            '[java]\nsessions = true\nrequire_authentication = false\n'
            '[bedrock]\nsessions = true\nrequire_authentication = false\nnethernet = true\n'
            'nethernet_identity_path = ' + json.dumps(str(root / "identity.pem")) + '\n'
            '[network.java]\nbind = "127.0.0.1:0"\n'
            '[network.bedrock]\nbind = "127.0.0.1:0"\n'
            '[[plugins]]\npath = ' + json.dumps(str(staged_plugin)) + '\ntimeout_ms = 1000\n',
            encoding="utf-8")
        log_path = root / "server.log"
        with log_path.open("w", encoding="utf-8") as output:
            process = subprocess.Popen([str(server), "run", "--config", str(settings)],
                cwd=ROOT, stdout=output, stderr=output,
                env=dict(os.environ, NO_COLOR="1", RUST_LOG="info"),
                creationflags=subprocess.CREATE_NEW_PROCESS_GROUP if os.name == "nt" else 0)
            java = None
            try:
                deadline = time.monotonic() + 5
                while time.monotonic() < deadline:
                    text = re.sub(r"\x1b\[[0-9;]*m", "", log_path.read_text(encoding="utf-8"))
                    tcp = re.search(r"Java TCP discovery listening.*?127\.0\.0\.1:(\d+)", text)
                    nether = re.search(r"NetherNet HTTP signaling listening.*?127\.0\.0\.1:(\d+)", text)
                    if tcp and nether:
                        break
                    assert process.poll() is None, text
                    time.sleep(.05)
                else:
                    raise TimeoutError(text)
                assert f"api_version={api_version}" in text and "gameplay-demo" in text
                java = wire.Client(("127.0.0.1", int(tcp[1])), "PluginJavaProbe")
                id_, reader = wire.receive_play(java)
                assert id_ == 0x7c
                assert nbt_text(reader) == "Sculith: PluginJavaProbe님, 환영합니다 👋"
                assert reader.number(">B") == 0
                reader.end()
                native = subprocess.run([str(client), "http://127.0.0.1:" + nether[1], "--plugin-message"],
                    capture_output=True, text=True, encoding="utf-8", timeout=40)
                assert native.returncode == 0, native.stdout + native.stderr
                assert "PASS: independent Bedrock decoder received Rust plugin's private Unicode system message" in native.stdout
                # Java must not receive the other player's private welcome. An
                # extra chat packet makes this independent block reader fail.
                wire.send(java.socket, 0x29, wire.vi(0) + wire.pos(1, 3, 1) + b"\x01" + wire.vi(1))
                java.block((1, 3, 1), 0, "air")
                deadline = time.monotonic() + 2
                while time.monotonic() < deadline:
                    if "gameplay block commit revision=1" in log_path.read_text(encoding="utf-8"):
                        break
                    time.sleep(.01)
                else:
                    raise AssertionError("Rust plugin did not observe the committed block event")
                java.socket.close()
                java = None
                process.send_signal(signal.CTRL_BREAK_EVENT if os.name == "nt" else signal.SIGINT)
                assert process.wait(timeout=5) == 0
                text = log_path.read_text(encoding="utf-8")
                assert "gameplay plugin stopped" in text
                assert "plugin disabled" not in text and "plugin message rejected" not in text
            except Exception:
                print(log_path.read_text(encoding="utf-8"))
                raise
            finally:
                if java is not None:
                    java.socket.close()
                if process.poll() is None:
                    process.kill()
                    process.wait(timeout=5)
    print(f"PASS: real Rust API-v{api_version} plugin, private Unicode welcome in both editions, no cross-player leak, committed block hook and graceful stopping")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    suffix = ".exe" if os.name == "nt" else ""
    parser.add_argument("--server", type=Path, default=ROOT / ("target/debug/sculith" + suffix))
    parser.add_argument("--plugin", type=Path, default=ROOT / ("target/debug/sculith-gameplay-plugin" + suffix))
    parser.add_argument("--client", type=Path, required=True)
    parser.add_argument("--api-version", type=int, choices=(2,3,4,5), default=5)
    args = parser.parse_args()
    smoke(args.server.resolve(strict=True), args.plugin.resolve(strict=True), args.client.resolve(strict=True), args.api_version)
