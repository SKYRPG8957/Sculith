#!/usr/bin/env python3
"""Independent native requests and Java creative/click ownership, counts and hooks."""
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
from peer_smoke import until

ROOT = Path(__file__).resolve().parents[2]


def synchronize(client):
    slots = {}
    for _ in range(200):
        id_, reader = wire.receive_play(client)
        if id_ == 110:
            slot = reader.varint()
            assert 0 <= slot <= 40 and slot not in slots
            slots[slot] = wire.item_stack(reader)
            reader.end()
        elif id_ == 107:
            assert reader.varint() == 0
            reader.end()
            assert len(slots) == 41
            return slots
        else:
            raise AssertionError(("unexpected inventory frame", id_))
    raise AssertionError("inventory synchronization incomplete")


def click(client, menu_slot, button, mode, predictions=False, state=None):
    body = wire.vi(0) + wire.vi(client.inventory_state if state is None else state)
    body += struct.pack(">hB", menu_slot, button) + wire.vi(mode)
    if predictions:
        # Deliberately forged Stone x64 predictions cannot replace owner Grass.
        hashed = b"\x01" + wire.vi(1) + wire.vi(64) + bytes(2)
        body += wire.vi(1) + struct.pack(">h", menu_slot) + hashed + hashed
    else:
        body += bytes(2)  # No changed predictions, empty predicted cursor.
    wire.send(client.socket, 18, body)


def creative(client, menu_slot, count, item):
    # Stock CreativeInventoryScreen carries locally; success must acknowledge
    # just the edited slot, leaving its unsent carried stack alone.
    wire.send(client.socket, 57, struct.pack(">h", menu_slot) + wire.vi(count)
        + (wire.vi(item) + bytes(2) if count else b""))
    id_, reader = wire.receive(client.socket)
    while id_ != 110:
        assert id_ not in (18, 98), "creative edit reset the local carried stack"
        assert wire.consume_peer(client, id_, reader), ("unexpected creative frame", id_)
        id_, reader = wire.receive(client.socket)
    assert reader.varint() == (menu_slot - 36 if menu_slot >= 36 else menu_slot)
    assert wire.item_stack(reader) == (count, item if count else 0)
    reader.end()


def smoke(server, native, plugin):
    with tempfile.TemporaryDirectory(prefix="sculith inventory ") as temporary:
        root = Path(temporary)
        settings, log = root / "server.toml", root / "server.log"
        settings.write_text(
            '[java]\nsessions=true\nrequire_authentication=false\n'
            '[bedrock]\nsessions=true\nrequire_authentication=false\n'
            'nethernet_identity_path=' + json.dumps(str(root / "identity.pem")) + '\n'
            '[network.java]\nbind="127.0.0.1:0"\n'
            '[network.bedrock]\nbind="127.0.0.1:0"\n'
            '[[plugins]]\npath=' + json.dumps(str(plugin)) + '\nargs=["--trace"]\n',
            encoding="utf-8")
        with log.open("w", encoding="utf-8") as output:
            process = subprocess.Popen([str(server), "run", "--config", str(settings)], cwd=ROOT,
                stdout=output, stderr=output, env=dict(os.environ, NO_COLOR="1", RUST_LOG="info,sculith::protocol::bedrock=debug"),
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
                java = wire.Client(("127.0.0.1", int(tcp[1])), "InvJavaProbe")
                assert java.entity_id == 1
                probe = subprocess.Popen([str(native), "http://127.0.0.1:" + udp[1], "--inventory"],
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
                    deadline = time.monotonic() + 45
                    while time.monotonic() < deadline:
                        line = updates.get(timeout=max(.01, deadline-time.monotonic()))
                        assert line is not None, "".join(lines)
                        if line == marker:
                            return
                    raise TimeoutError("".join(lines))

                barrier("INVENTORY_NATIVE_READY")
                until(java, lambda: getattr(java, "peer_entities", {}).get(2, {}).get("held") == (5, 55))
                creative(java, 36, 2, 54)
                barrier("INVENTORY_JAVA_HELD_READY")
                click(java, 36, 1, 0, predictions=True)
                slots = synchronize(java)
                assert slots[0] == (1, 54) and java.cursor == (1, 54)
                # A stale client state still computes a legal owner swap, then fully synchronizes.
                click(java, 37, 0, 2, state=0)
                slots = synchronize(java)
                assert slots[0] == (64, 55) and slots[1] == (1, 54) and java.cursor == (1, 54)
                # Three creative UI replacements must not emit a cursor reset.
                creative(java, 9, 17, 1)
                creative(java, 10, 17, 55)
                creative(java, 9, 17, 55)
                click(java, 36, 0, 5)
                slots = synchronize(java)
                assert slots[9] == slots[10] == (17, 55) and java.cursor == (1, 54)
                # Shift clicks compute merges from owner counts, even with stale
                # state and forged predictions. A nonempty cursor is untouched.
                click(java, 36, 0, 1, predictions=True, state=0)
                slots = synchronize(java)
                assert slots[0] == (0, 0) and slots[9] == (64, 55) and slots[10] == (34, 55)
                assert java.cursor == (1, 54)
                click(java, 10, 1, 1)
                slots = synchronize(java)
                assert slots[0] == (34, 55) and slots[10] == (0, 0)
                click(java, 9, 0, 1)
                slots = synchronize(java)
                assert slots[0] == (64, 55) and slots[3] == (34, 55) and slots[9] == (0, 0)
                assert java.cursor == (1, 54)
                # Place the Grass cursor away, then split Dirt from an unselected
                # hotbar stack. Collection must take that partial stack before
                # reducing the full selected stack, with forged/stale predictions.
                click(java, 10, 0, 0)
                synchronize(java)
                assert java.cursor == (0, 0)
                click(java, 39, 1, 0)
                slots = synchronize(java)
                assert java.cursor == (17, 55) and slots[3] == (17, 55)
                click(java, 9, 0, 6, predictions=True, state=0)
                slots = synchronize(java)
                assert java.cursor == (64, 55) and slots[0] == (34, 55) and slots[3] == (0, 0)
                full = slots.copy()
                click(java, 9, 0, 6)
                assert synchronize(java) == full and java.cursor == (64, 55)
                click(java, 9, 0, 0)
                slots = synchronize(java)
                assert slots[9] == (64, 55) and java.cursor == (0, 0)
                click(java, 10, 0, 0)
                synchronize(java)
                assert java.cursor == (1, 54)
                click(java, 10, 1, 6)
                slots = synchronize(java)
                assert java.cursor == (64, 54) and slots[1] == (0, 0) and slots[2] == (2, 54)
                assert probe.wait(timeout=10) == 0, "".join(lines)
                reader.join(timeout=2)
                before = slots.copy()
                click(java, 36, 0, 5)  # Unsupported drag painting stays connected and unchanged.
                assert synchronize(java) == before and java.cursor == (64, 54)
                wire.send(java.socket, 57, struct.pack(">h", 36) + wire.vi(64) + wire.vi(2) + bytes(2))
                assert synchronize(java) == before
                java.socket.close()
                java = None
                process.send_signal(signal.CTRL_BREAK_EVENT if os.name == "nt" else signal.SIGINT)
                assert process.wait(timeout=5) == 0
                text = log.read_text(encoding="utf-8")
                assert 'events inventory player=1' in text and 'events inventory player=2' in text
                for packet_id in (33, 47):
                    assert re.search(r"events packet player=2 edition=Bedrock .*id=" + str(packet_id) + r" .*dispatch=Control", text)
                assert re.search(r"events equipment player=1 .*count=2", text)
                assert re.search(r"events equipment player=2 .*count=5", text)
                assert 'plugin disabled' not in text and 'events plugin stopped' in text
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
    print("PASS: independent native transactions, Java creative/pickup/swap/quick-move/collect, forged-prediction rejection, counted peer hands, owner plugin events and no disconnect on unsupported inventory actions")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    suffix = ".exe" if os.name == "nt" else ""
    parser.add_argument("--server", type=Path, default=ROOT / ("target/debug/sculith" + suffix))
    parser.add_argument("--plugin", type=Path, default=ROOT / ("target/debug/sculith-events-plugin" + suffix))
    parser.add_argument("--client", type=Path, required=True)
    args = parser.parse_args()
    smoke(args.server.resolve(strict=True), args.client.resolve(strict=True), args.plugin.resolve(strict=True))
