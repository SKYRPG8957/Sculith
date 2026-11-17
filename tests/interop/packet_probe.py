"""Exercise current-version outgoing packet edits through a stable Rust Wasm plugin."""

import json
import re
import socket
import struct
import subprocess

from shared_resources import Java, PACKETS, string, varint


def check(java_port, native_port, root, probe):
    client = Java(java_port, "PacketJava")
    try:
        client.join_with_resources(java_port, root, False)
        expected = ["sculith:rewritten", "sculith:payload", "sculith:sentinel"]
        for _ in range(4096):
            id_, fields = client.receive()
            assert id_ != PACKETS["clientbound"]["play"]["sound"], "original packet escaped the plugin"
            if id_ == PACKETS["clientbound"]["play"]["stop_sound"]:
                assert fields.take(1) == b"\2"
                assert fields.text() == expected.pop(0)
                assert fields.rest() == b""
                if not expected:
                    break
        assert not expected, ("missing packet edits", expected)
    finally:
        client.socket.close()
    result = subprocess.run([str(probe), f"http://127.0.0.1:{native_port}", "--packet-edits"], capture_output=True, text=True, timeout=30)
    (root / "packet-edits-native.log").write_text(result.stdout + result.stderr)
    assert result.returncode == 0, result.stdout + result.stderr
    return "both editions; ID and payload changes; cancellation; unchanged sentinel; Rust Wasm callback"


def connections(java_port, native_port, root, probe):
    client = object.__new__(Java)
    client.socket = socket.create_connection(("127.0.0.1", java_port), timeout=10)
    client.compression = None
    try:
        client.send(0, varint(777) + string("127.0.0.1") + struct.pack(">H", java_port) + varint(1))
        client.send(0)
        id_, fields = client.receive()
        assert id_ == PACKETS["clientbound"]["status"]["status_response"]
        assert json.loads(fields.text())["version"]["protocol"] == 777
        client.send(1, b"EVENT123")
        id_, fields = client.receive()
        assert id_ == PACKETS["clientbound"]["status"]["pong_response"] and fields.rest() == b"EVENT123"
    finally:
        client.socket.close()
    result = subprocess.run([str(probe), f"http://127.0.0.1:{native_port}", "--connection-probe"], capture_output=True, text=True, timeout=30)
    (root / "connection-probe-native.log").write_text(result.stdout + result.stderr)
    assert result.returncode == 0, result.stdout + result.stderr
    rows = (root / "server.log").read_text().splitlines()
    for state in ["HandShake", "Status", "Login", "Config"]:
        assert any("PROBE_JAVA_CONNECTION_RX" in row and f"state={state}" in row for row in rows), state
    for state in ["Status", "Login", "Config"]:
        assert any("PROBE_JAVA_CONNECTION_TX" in row and f"state={state}" in row for row in rows), state
    assert any("PROBE_JAVA_CONNECTION_TX" in row and "state=Status" in row and "replaced=true" in row for row in rows)
    rewritten = {}
    for row in rows:
        if "PROBE_BEDROCK_CONNECTION_RX" in row and "rewritten=true" in row:
            conn, id_ = re.search(r"connection=(\d+).* id=(\d+)", row).groups()
            rewritten.setdefault(conn, set()).add(int(id_))
    assert any(ids == {193, 1} for ids in rewritten.values()), rewritten
    assert any("PROBE_BEDROCK_CONNECTION_TX" in row and "protocol=2225 id=143" in row for row in rows)
    return "Java handshake/status/login/config; status replacement via cancellation and extras; native network-settings/login observers and proposal rewrite before admission"
