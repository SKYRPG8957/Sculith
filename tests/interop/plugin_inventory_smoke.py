#!/usr/bin/env python3
"""Real Rust inventory requests, private callbacks and Java/native state delivery."""
import argparse
import json
import os
from pathlib import Path
import queue
import re
import signal
import struct
import subprocess
import tempfile
import threading
import time

import java_smoke as wire
from inventory_smoke import click, creative, synchronize
from peer_smoke import until
from plugin_game_smoke import nbt_text

ROOT = Path(__file__).resolve().parents[2]


def response(client, message, changes):
    seen = {}
    for _ in range(300):
        id_, reader = wire.receive(client.socket)
        assert id_ not in (18, 98), "plugin grant reset the creative carried stack"
        if id_ == 110:
            slot = reader.varint()
            assert slot in changes and slot not in seen, ("unexpected private delta", slot)
            seen[slot] = wire.item_stack(reader)
            reader.end()
        elif id_ == 124:
            text = nbt_text(reader)
            assert reader.number(">B") == 0
            reader.end()
            assert text == message, text
            assert seen == changes, (seen, changes)
            return
        else:
            assert wire.consume_peer(client, id_, reader), id_
    raise AssertionError("inventory callback deadline")


def smoke(server, plugin, native, api_version=11):
    with tempfile.TemporaryDirectory(prefix="sculith plugin inventory ") as temporary:
        root = Path(temporary)
        settings, log = root / "server.toml", root / "server.log"
        settings.write_text('[java]\nsessions=true\nrequire_authentication=false\n'
            '[bedrock]\nsessions=true\nrequire_authentication=false\n'
            'nethernet_identity_path=' + json.dumps(str(root / "identity.pem")) + '\n'
            '[network.java]\nbind="127.0.0.1:0"\n[network.bedrock]\nbind="127.0.0.1:0"\n'
            '[[plugins]]\npath=' + json.dumps(str(plugin)) + '\n'
            '[[plugins]]\npath=' + json.dumps(str(plugin)) + '\nargs=["--observe"]\n', encoding="utf-8")
        with log.open("w", encoding="utf-8") as output:
            process = subprocess.Popen([str(server), "run", "--config", str(settings)], cwd=ROOT,
                stdout=output, stderr=output, env=dict(os.environ, NO_COLOR="1", RUST_LOG="info"),
                creationflags=subprocess.CREATE_NEW_PROCESS_GROUP if os.name == "nt" else 0)
            java = probe = None
            lines = []
            try:
                deadline = time.monotonic() + 8
                while time.monotonic() < deadline:
                    text = log.read_text(encoding="utf-8")
                    tcp = re.search(r"Java listening.*?127\.0\.0\.1:(\d+)", text)
                    udp = re.search(r"NetherNet listening.*?127\.0\.0\.1:(\d+)", text)
                    if tcp and udp:
                        break
                    assert process.poll() is None, text
                    time.sleep(.05)
                else:
                    raise TimeoutError(text)
                java = wire.Client(("127.0.0.1", int(tcp[1])), "InventoryApiJava")
                assert java.entity_id == 1
                creative(java, 36, 0, 0)
                probe = subprocess.Popen([str(native), "http://127.0.0.1:" + udp[1], "--plugin-inventory"],
                    stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, encoding="utf-8")
                updates = queue.Queue()

                def read_output():
                    for line in probe.stdout:
                        lines.append(line)
                        updates.put(line.strip())
                    updates.put(None)

                reader = threading.Thread(target=read_output, daemon=True)
                reader.start()

                def barrier(marker):
                    while True:
                        line = updates.get(timeout=30)
                        assert line is not None, "".join(lines)
                        if line == marker:
                            return

                barrier("PLUGIN_INVENTORY_NATIVE_GRANTED")
                until(java, lambda: getattr(java, "peer_entities", {}).get(2, {}).get("held") == (5, 55))
                assert not getattr(java, "chat_messages", []), "native private reply leaked to Java"
                wire.send(java.socket, 7, wire.string("sculith_test_give"))
                response(java, "Added 5 Dirt.", {0: (5, 55)})
                barrier("PLUGIN_INVENTORY_JAVA_GRANTED")
                assert probe.wait(timeout=10) == 0
                reader.join(timeout=2)
                # An actual owner cursor must survive a concurrent server grant.
                click(java, 36, 1, 0)
                assert synchronize(java)[0] == (2, 55) and java.cursor == (3, 55)
                time.sleep(.55)
                wire.send(java.socket, 7, wire.string("sculith_test_give"))
                response(java, "Added 5 Dirt.", {0: (7, 55)})
                assert java.cursor == (3, 55)
                time.sleep(.55)
                wire.send(java.socket, 7, wire.string("sculith_test_inventory"))
                response(java, "Inventory: 3 stacks, 135 items.", {})
                click(java, 36, 0, 0)
                assert synchronize(java)[0] == (10, 55) and java.cursor == (0, 0)
                for slot in range(9, 45):
                    creative(java, slot, 64, 1)
                time.sleep(.55)
                wire.send(java.socket, 7, wire.string("sculith_test_give"))
                response(java, "Inventory request rejected: Full", {})
                time.sleep(.55)
                wire.send(java.socket, 7, wire.string("sculith_test_inventory"))
                response(java, "Inventory: 36 stacks, 2304 items.", {})
                java.socket.close()
                java = None
                process.send_signal(signal.CTRL_BREAK_EVENT if os.name == "nt" else signal.SIGINT)
                assert process.wait(timeout=5) == 0
                text = log.read_text(encoding="utf-8")
                assert f"api_version={api_version}" in text and "inventory plugin stopped" in text
                assert "unexpected foreign inventory response" not in text and "plugin disabled" not in text
                print("".join(lines), end="")
            except Exception:
                print(log.read_text(encoding="utf-8"))
                print("".join(lines))
                raise
            finally:
                if java:
                    java.socket.close()
                if probe and probe.poll() is None:
                    probe.kill()
                    probe.wait(timeout=5)
                if process.poll() is None:
                    process.kill()
                    process.wait(timeout=5)
    print("PASS: Rust API-v7 atomic grants/queries, private plugin results, live Java/native slots and hands, cursor preservation and full-inventory rejection")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    suffix = ".exe" if os.name == "nt" else ""
    parser.add_argument("--server", type=Path, default=ROOT / ("target/debug/sculith" + suffix))
    parser.add_argument("--plugin", type=Path, default=ROOT / ("target/debug/sculith-inventory-fixture" + suffix))
    parser.add_argument("--client", type=Path, required=True)
    parser.add_argument("--api-version", type=int, choices=(7,8,9,10,11), default=11)
    args = parser.parse_args()
    smoke(args.server.resolve(strict=True), args.plugin.resolve(strict=True), args.client.resolve(strict=True), args.api_version)
