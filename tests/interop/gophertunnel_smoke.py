#!/usr/bin/env python3
"""Exercise the built Rust server with an independent pinned Go/Pion Bedrock client."""
import argparse
import os
from pathlib import Path
import re
import signal
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[2]


def smoke(server, client):
    with tempfile.TemporaryDirectory(prefix="sculith-gophertunnel-") as temporary:
        root = Path(temporary)
        config = root / "server.toml"
        config.write_text(
            '[bedrock]\nsessions = true\nrequire_authentication = false\nlogin_timeout_secs = 30\n'
            '[network.java]\nenabled = false\nbind = "127.0.0.1:0"\n'
            '[network.bedrock]\nbind = "127.0.0.1:0"\n', encoding="utf-8")
        log_path = root / "server.log"
        with log_path.open("w", encoding="utf-8") as output:
            process = subprocess.Popen(
                [str(server), "run", "--config", str(config)],
                stdout=output, stderr=output, cwd=ROOT,
                env=dict(os.environ, NO_COLOR="1", RUST_LOG="info"),
                creationflags=subprocess.CREATE_NEW_PROCESS_GROUP if os.name == "nt" else 0,
            )
            try:
                deadline = time.monotonic() + 5
                while time.monotonic() < deadline:
                    text = re.sub(r"\x1b\[[0-9;]*m", "", log_path.read_text(encoding="utf-8"))
                    match = re.search(r"NetherNet listening.*?127\.0\.0\.1:(\d+)", text)
                    if match:
                        break
                    assert process.poll() is None, text
                    time.sleep(.05)
                else:
                    raise TimeoutError(text)
                result = subprocess.run(
                    [str(client), "http://127.0.0.1:" + match[1]],
                    capture_output=True, text=True, encoding="utf-8", timeout=40,
                )
                assert result.returncode == 0, result.stdout + result.stderr
                assert "PASS: independent gophertunnel NetherNet world spawn" in result.stdout
                assert "PASS: independent Dragonfly decoding of 25 chunks, all 2457600 blocks" in result.stdout
                deadline = time.monotonic() + 2
                while time.monotonic() < deadline:
                    if "Bedrock client acknowledged the flat-world spawn" in log_path.read_text(encoding="utf-8"):
                        break
                    time.sleep(.01)
                else:
                    raise AssertionError("server did not activate the client's player lease")
                process.send_signal(signal.CTRL_BREAK_EVENT if os.name == "nt" else signal.SIGINT)
                assert process.wait(timeout=5) == 0
            except Exception:
                print(log_path.read_text(encoding="utf-8"))
                raise
            finally:
                if process.poll() is None:
                    process.kill()
                    process.wait(timeout=5)
    print("PASS: independent gophertunnel/Go/Pion signed NetherNet login, pack negotiation, world spawn and server-side activation; Dragonfly decodes all 2457600 blocks, biomes and startup definitions")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    suffix = ".exe" if os.name == "nt" else ""
    parser.add_argument("--server", type=Path, default=ROOT / ("target/debug/sculith" + suffix))
    parser.add_argument("--client", type=Path, required=True)
    args = parser.parse_args()
    smoke(args.server.resolve(strict=True), args.client.resolve(strict=True))
