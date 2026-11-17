#!/usr/bin/env python3
"""Bidirectional metadata, lifecycle, and retained v8 connection subscribers."""
import argparse
from collections import defaultdict
import json
import os
from pathlib import Path
import re
import signal
import socket
import struct
import subprocess
import tempfile
import time

import java_smoke as wire

ROOT = Path(__file__).resolve().parents[2]
SECRET = "private-packet-body-should-never-appear"


def wait_for(log, process, condition, seconds=10):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        text = log.read_text(encoding="utf-8")
        result = condition(text)
        if result:
            return result
        assert process.poll() is None, text
        time.sleep(.02)
    raise TimeoutError(log.read_text(encoding="utf-8"))


def start(server, plugin, root, sessions=True, slow=False, legacy=False):
    config, log = root / "server.toml", root / "server.log"
    config.write_text(
        '[java]\nsessions=' + str(sessions).lower() + '\nrequire_authentication=false\nlogin_timeout_secs=3\n'
        '[bedrock]\nsessions=' + str(sessions).lower() + '\nrequire_authentication=false\n'
        'nethernet_identity_path=' + json.dumps(str(root / "identity.pem")) + '\n'
        '[network.java]\nbind="127.0.0.1:0"\n[network.bedrock]\nbind="127.0.0.1:0"\n'
        '[[plugins]]\npath=' + json.dumps(str(plugin)) + '\n' + ('args=["--slow"]\n' if slow else 'args=["--api8"]\n' if legacy else ''),
        encoding="utf-8")
    if not legacy and not slow:
        with config.open("a", encoding="utf-8") as output:
            output.write('\n[[plugins]]\npath=' + json.dumps(str(plugin)) + '\nargs=["--api8"]\n')
    with log.open("w", encoding="utf-8") as output:
        process = subprocess.Popen([str(server), "run", "--config", str(config)], cwd=ROOT,
            stdout=output, stderr=output, env=dict(os.environ, NO_COLOR="1", RUST_LOG="info"),
            creationflags=subprocess.CREATE_NEW_PROCESS_GROUP if os.name == "nt" else 0)
    match = wait_for(log, process, lambda text: re.search(r"Java listening.*?127\.0\.0\.1:(\d+)", text))
    return process, log, ("127.0.0.1", int(match[1]))


def stop(process):
    process.send_signal(signal.CTRL_BREAK_EVENT if os.name == "nt" else signal.SIGINT)
    assert process.wait(timeout=10) == 0


def handshake(connection, address, state, protocol=777):
    wire.send(connection, 0, wire.vi(protocol) + wire.string("localhost") + struct.pack(">H", address[1]) + wire.vi(state))


def status(address):
    with socket.create_connection(address, timeout=5) as connection:
        handshake(connection, address, 1)
        wire.send(connection, 0)
        id_, reader = wire.receive(connection)
        assert id_ == 0 and "players" in json.loads(reader.string())
        wire.send(connection, 1, struct.pack(">q", 42))
        id_, reader = wire.receive(connection)
        assert id_ == 1 and reader.number(">q") == 42


def observations(text, legacy=False):
    marker = "connection-v8-fixture {" if legacy else "connection-fixture {"
    return [json.loads("{" + line.split(marker, 1)[1])
            for line in text.splitlines() if marker in line]


def verify(text, sessions, legacy=False):
    assert f"api_version={8 if legacy else 12}" in text
    assert "plugin disabled" not in text, text
    assert ("connection v8 fixture stopped" if legacy else "connection fixture stopped") in text
    events = observations(text, legacy)
    assert events, text
    grouped = defaultdict(list)
    for event in events:
        assert event["connection_id"] > 0
        grouped[event["connection_id"]].append(event)
    assert SECRET not in json.dumps(events) and "SensitiveProfile" not in json.dumps(events)
    sequences = []
    for events in grouped.values():
        assert events[0]["type"] == "opened" and events[-1]["type"] == "closed", events
        assert sum(e["type"] == "opened" for e in events) == 1
        assert sum(e["type"] == "closed" for e in events) == 1
        packets = [e["packet"] for e in events if e["type"] == "packet_received"]
        assert [p["sequence"] for p in packets] == list(range(1, len(packets) + 1))
        assert all(set(p) == {"protocol", "phase", "id", "name", "dispatch", "sequence"} for p in packets)
        sent = [e["packet"] for e in events if e["type"] == "packet_sent"]
        assert [p["sequence"] for p in sent] == list(range(1, len(sent) + 1))
        assert all(set(p) == {"protocol", "phase", "id", "name", "dispatch", "sequence"} for p in sent)
        if legacy:
            assert not sent, events
        sequences.append((events[0]["transport"], packets, events))
    assert any([p["phase"] for p in packets] == ["handshake", "status", "status"]
        and [p["name"] for p in packets] == ["minecraft:intention", "minecraft:status_request", "minecraft:ping_request"]
        for _, packets, _ in sequences)
    if not legacy:
        assert any([e["packet"]["name"] for e in events if e["type"] == "packet_sent"]
            == ["minecraft:status_response", "minecraft:pong_response"] for _, _, events in sequences)
        if sessions:
            java_sent = next([e["packet"] for e in events if e["type"] == "packet_sent"]
                for transport, _, events in sequences if transport == "java_tcp" and any(e["type"] == "player_attached" for e in events))
            assert {"login", "configuration", "spawn", "play"} <= {p["phase"] for p in java_sent}
            assert any(p["name"] == "minecraft:level_chunk_with_light" for p in java_sent)
            native_sent = next([e["packet"] for e in events if e["type"] == "packet_sent"]
                for transport, _, events in sequences if transport == "bedrock_nethernet" and any(e["type"] == "player_attached" for e in events))
            assert {"network_settings", "login", "resource_packs", "spawn", "play"} <= {p["phase"] for p in native_sent}
            assert any(p["name"] == "InventoryContentPacket" for p in native_sent)
            assert any(p["name"] == "StartGamePacket" for p in native_sent)
    if sessions:
        java = next(packets for transport, packets, events in sequences
            if transport == "java_tcp" and any(e["type"] == "player_attached" for e in events))
        assert {"handshake", "login", "configuration", "spawn", "play"} <= {p["phase"] for p in java}
        assert any(p["id"] == 0x16 and p["name"] == "minecraft:custom_payload" for p in java)
        native = next(packets for transport, packets, events in sequences
            if transport == "bedrock_nethernet" and any(e["type"] == "player_attached" for e in events))
        assert {"network_settings", "login", "resource_packs", "spawn"} <= {p["phase"] for p in native}
        assert all(p["protocol"] == 2225 for p in native)
        assert any(len(packets) == 1 and packets[0]["phase"] == "handshake" for _, packets, _ in sequences), "login timeout missing"
    else:
        assert all(e["type"] != "player_attached" for group in grouped.values() for e in group)
    assert any(p["protocol"] == 776 and p["name"] == "unknown"
        for _, packets, _ in sequences for p in packets), "wrong-version login mislabeled"


def smoke(server, plugin, native):
    for sessions, legacy in [(True, False), (False, False), (True, True)]:
        with tempfile.TemporaryDirectory(prefix="sculith connections ") as temporary:
            process, log, address = start(server, plugin, Path(temporary), sessions, legacy=legacy)
            idle = None
            try:
                status(address)
                with socket.create_connection(address, timeout=5) as connection:
                    handshake(connection, address, 2, 776)
                    wire.send(connection, 0, wire.string("SensitiveProfile") + bytes(16))
                    assert wire.receive(connection)[0] == 0
                if sessions:
                    with socket.create_connection(address, timeout=5) as connection:
                        handshake(connection, address, 2)
                        # Session timeout before Login Start; no player is reserved.
                        assert wire.receive(connection)[0] == 0
                    client = wire.Client(address, "SensitiveProfile")
                    wire.send(client.socket, 0x16, wire.string("sculith:test") + SECRET.encode())
                    time.sleep(.1)
                    client.socket.close()
                    match = wait_for(log, process, lambda text: re.search(r"NetherNet listening.*?127\.0\.0\.1:(\d+)", text))
                    result = subprocess.run([str(native), "http://127.0.0.1:" + match[1], "--connections"], capture_output=True,
                        text=True, encoding="utf-8", timeout=40)
                    assert result.returncode == 0, result.stdout + result.stderr
                # Listener shutdown aborts this task; its RAII trace must close.
                idle = socket.create_connection(address, timeout=5)
                time.sleep(.1)
                stop(process)
                verify(log.read_text(encoding="utf-8"), sessions, legacy)
                if not legacy:
                    verify(log.read_text(encoding="utf-8"), sessions, True)
            except Exception:
                print(log.read_text(encoding="utf-8"))
                raise
            finally:
                if idle:
                    idle.close()
                if process.poll() is None:
                    process.kill(); process.wait(timeout=5)
    # Explicit overflow disables the observer plugin without stopping discovery.
    with tempfile.TemporaryDirectory(prefix="sculith connection backlog ") as temporary:
        process, log, address = start(server, plugin, Path(temporary), False, True)
        connections = []
        try:
            for _ in range(48):
                connection = socket.create_connection(address, timeout=5)
                connections.append(connection)
                handshake(connection, address, 1)
                wire.send(connection, 0)
                wire.send(connection, 1, struct.pack(">q", 7))
            wait_for(log, process, lambda text: "plugin disabled after missing connection events" in text)
            for connection in connections:
                connection.close()
            connections.clear()
            status(address)
            stop(process)
        except Exception:
            print(log.read_text(encoding="utf-8"))
            raise
        finally:
            for connection in connections:
                connection.close()
            if process.poll() is None:
                process.kill(); process.wait(timeout=5)
    print("PASS: Rust API-v12 host, v9 bidirectional connection metadata, retained v8 subscriber, phases, status-only mode, login rejection/timeout, Java/native attachment, shutdown closes, redaction and explicit backlog isolation")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    suffix = ".exe" if os.name == "nt" else ""
    parser.add_argument("--server", type=Path, default=ROOT / ("target/debug/sculith" + suffix))
    parser.add_argument("--plugin", type=Path, default=ROOT / ("target/debug/sculith-connection-fixture" + suffix))
    parser.add_argument("--client", type=Path, required=True)
    args = parser.parse_args()
    smoke(args.server.resolve(strict=True), args.plugin.resolve(strict=True), args.client.resolve(strict=True))
