#!/usr/bin/env python3
"""Exercise built server + Rust plugin over real TCP/UDP; no third-party packages."""

import argparse
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


ROOT = Path(__file__).resolve().parents[1]
MAGIC = bytes.fromhex("00ffff00fefefefefdfdfdfd12345678")
PLUGIN_MOTD = "Sculith Rust plugin / discovery only"


def start_server(server, settings, log, env):
    return subprocess.Popen(
        [str(server), "run", "--config", str(settings)],
        cwd=ROOT, stdout=log, stderr=log, env=env,
        creationflags=subprocess.CREATE_NEW_PROCESS_GROUP if os.name == "nt" else 0,
    )


def stop_server(process):
    process.send_signal(signal.CTRL_BREAK_EVENT if os.name == "nt" else signal.SIGINT)


def varint(number):
    number &= 0xFFFFFFFF
    result = bytearray()
    while True:
        byte = number & 0x7F
        number >>= 7
        result.append(byte | (0x80 if number else 0))
        if not number:
            return bytes(result)


def read_exact(stream, size):
    data = bytearray()
    while len(data) < size:
        chunk = stream.recv(size - len(data))
        if not chunk:
            raise AssertionError("unexpected socket EOF")
        data.extend(chunk)
    return bytes(data)


def read_varint(stream):
    number = 0
    for index in range(5):
        byte = read_exact(stream, 1)[0]
        number |= (byte & 0x7F) << (7 * index)
        if not byte & 0x80:
            return number
    raise AssertionError("oversized response VarInt")


def frame(payload):
    return varint(len(payload)) + payload


def read_frame(stream):
    size = read_varint(stream)
    assert 0 < size < 65_536, f"invalid response size: {size}"
    return read_exact(stream, size)


def json_packet(payload):
    assert payload[0] == 0
    from io import BytesIO

    class Reader:
        def __init__(self, data):
            self.data = BytesIO(data)

        def recv(self, size):
            return self.data.read(size)

    stream = Reader(payload[1:])
    size = read_varint(stream)
    result = json.loads(read_exact(stream, size))
    assert not stream.recv(1), "trailing JSON response bytes"
    return result


def handshake(port, state):
    host = b"localhost"
    return frame(b"\x00" + varint(777) + varint(len(host)) + host + struct.pack(">H", port) + varint(state))


def check_java(port, stage="discovery-only"):
    with socket.create_connection(("127.0.0.1", port), timeout=2) as client:
        client.sendall(handshake(port, 1) + frame(b"\x00"))
        status = json_packet(read_frame(client))
        assert PLUGIN_MOTD in status["description"]["text"]
        assert status["players"] == {"max": 32, "online": 0, "sample": []}
        assert status["version"]["protocol"] == -1
        assert status["sculith"]["stage"] == stage
        ping = b"\x01" + struct.pack(">q", -123456789)
        client.sendall(frame(ping))
        assert read_frame(client) == ping, "Java ping payload changed"

    with socket.create_connection(("127.0.0.1", port), timeout=2) as client:
        username = b"SculithTest"
        login_start = b"\x00" + varint(len(username)) + username + bytes(16)
        client.sendall(handshake(port, 2) + frame(login_start))
        disconnect = json_packet(read_frame(client))
        assert "world login is not implemented" in disconnect["text"]

    # A malformed connection must not kill the shared listener.
    with socket.create_connection(("127.0.0.1", port), timeout=2) as client:
        client.sendall(varint(-1))
        assert not client.recv(1)
    with socket.create_connection(("127.0.0.1", port), timeout=2):
        pass


def check_bedrock(port, expected_reference, sessions=False):
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as client:
        client.settimeout(2)
        ping = b"\x01" + struct.pack(">Q", 987654321) + MAGIC + struct.pack(">Q", 7)
        client.sendto(ping, ("127.0.0.1", port))
        pong, remote = client.recvfrom(2048)
        assert remote[1] == port
        assert pong[0] == 0x1C
        assert pong[1:9] == ping[1:9]
        assert pong[17:33] == MAGIC
        assert len(pong[35:]) == struct.unpack(">H", pong[33:35])[0]
        fields = pong[35:].decode().split(";")
        assert fields[0] == "MCPE"
        assert fields[1] == PLUGIN_MOTD, "Bedrock did not receive the Rust plugin's common MOTD"
        assert int(fields[2]) == expected_reference["protocol"]
        assert fields[3] == expected_reference["version"]
        assert fields[4:6] == ["0", "32"]
        assert int(fields[6]) == struct.unpack(">Q", pong[9:17])[0]
        assert fields[7] == ("Sculith / Bedrock spawn preview" if sessions else "Sculith / discovery only")
        assert fields[8] == ("Creative" if sessions else "Survival")
        assert fields[10:12] == [str(port), "0"]

        invalid = bytearray(ping)
        invalid[9] ^= 255
        client.settimeout(0.15)
        client.sendto(invalid, ("127.0.0.1", port))
        try:
            client.recvfrom(2048)
        except socket.timeout:
            pass
        else:
            raise AssertionError("Bedrock replied to invalid RakNet magic")

        if sessions:
            # V2225 uses RakNet 11. This proves the built executable's driver
            # handles connection requests beyond discovery; world spawn is
            # exercised by the Rust headless protocol-client tests.
            client.settimeout(2)
            request = b"\x05" + MAGIC + b"\x0b" + bytes(548)
            client.sendto(request, ("127.0.0.1", port))
            reply, _ = client.recvfrom(2048)
            assert reply[0] == 0x06 and reply[1:17] == MAGIC


def run_cli(server, *args, ok=True):
    result = subprocess.run([str(server), *map(str, args)], cwd=ROOT, capture_output=True, text=True, timeout=10)
    assert (result.returncode == 0) == ok, (args, result.stdout, result.stderr)
    return result


def smoke(server, plugin, sessions=False):
    versions = json.loads(run_cli(server, "versions").stdout)
    assert versions["java"]["gameplay_supported_versions"] == []
    assert versions["bedrock"]["gameplay_supported_versions"] == []
    run_cli(server, "check", "--config", ROOT / "sculith.example.toml")

    with tempfile.TemporaryDirectory(prefix="sculith-smoke-") as temporary:
        base = Path(temporary)
        snapshot = base / "world" / "region.sculith"
        settings = base / "server.toml"
        preview = '[bedrock]\nsessions = true\nrequire_authentication = false\n' if sessions else ''
        settings.write_text(
            '[world]\nsnapshot_path = ' + json.dumps(str(snapshot)) + '\n' + preview +
            '[server]\nmotd = "before plugin"\nmax_players = 32\n'
            '[network.java]\nbind = "127.0.0.1:0"\n'
            '[network.bedrock]\nbind = "127.0.0.1:0"\n'
            '[[plugins]]\npath = ' + json.dumps(str(plugin)) + '\ntimeout_ms = 1000\n',
            encoding="utf-8",
        )
        run_cli(server, "check", "--config", settings)
        invalid = base / "invalid.toml"
        invalid.write_text("[server]\nmax_player = 20\n", encoding="utf-8")
        run_cli(server, "check", "--config", invalid, ok=False)

        log_path = base / "server.log"
        env = dict(os.environ, NO_COLOR="1", RUST_LOG="info")
        with log_path.open("w") as log:
            process = start_server(server, settings, log, env)
            try:
                deadline = time.monotonic() + 10
                while time.monotonic() < deadline:
                    text = re.sub(r"\x1b\[[0-9;]*m", "", log_path.read_text(encoding="utf-8"))
                    java = re.search(r"Java TCP discovery listening.*?127\.0\.0\.1:(\d+)", text)
                    bedrock = re.search(r"Bedrock UDP discovery listening.*?127\.0\.0\.1:(\d+)", text)
                    if java and bedrock:
                        break
                    assert process.poll() is None, text
                    time.sleep(0.05)
                else:
                    raise AssertionError("server startup timed out: " + text)
                check_java(int(java[1]), "bedrock-spawn-preview" if sessions else "discovery-only")
                check_bedrock(int(bedrock[1]), versions["bedrock"], sessions)
                stop_server(process)
                assert process.wait(timeout=5) == 0, log_path.read_text(encoding="utf-8")
                text = log_path.read_text(encoding="utf-8")
                assert "welcome plugin stopped" in text, text
                assert "Sculith stopped" in text, text
            finally:
                if process.poll() is None:
                    process.kill()
                    process.wait(timeout=5)

        assert snapshot.read_bytes().startswith(b"SCULITHR"), "orderly shutdown did not save the world"
        saved_snapshot = snapshot.read_bytes()
        restart_log = base / "restart.log"
        with restart_log.open("w") as log:
            restarted = start_server(server, settings, log, env)
            try:
                deadline = time.monotonic() + 10
                while time.monotonic() < deadline:
                    text = re.sub(r"\x1b\[[0-9;]*m", "", restart_log.read_text(encoding="utf-8"))
                    java = re.search(r"Java TCP discovery listening.*?127\.0\.0\.1:(\d+)", text)
                    if java:
                        break
                    assert restarted.poll() is None, text
                    time.sleep(0.05)
                else:
                    raise AssertionError("snapshot restart did not become ready: " + text)
                check_java(int(java[1]), "bedrock-spawn-preview" if sessions else "discovery-only")
                stop_server(restarted)
                assert restarted.wait(timeout=5) == 0, restart_log.read_text(encoding="utf-8")
            finally:
                if restarted.poll() is None:
                    restarted.kill(); restarted.wait(timeout=5)
        assert snapshot.read_bytes() == saved_snapshot, "unchanged world must roundtrip deterministically"
        foreign = base / "foreign-world"
        foreign.write_bytes(b"existing non-Sculith world data")
        settings.write_text('[world]\nsnapshot_path = ' + json.dumps(str(foreign)) + '\n', encoding="utf-8")
        run_cli(server, "run", "--config", settings, ok=False)
        assert foreign.read_bytes() == b"existing non-Sculith world data"

        destination = base / "java-pack"
        major, minor = versions["java"]["resource_pack_format"]
        command = ["pack", "convert", "--input", ROOT / "examples/pack/bedrock", "--mapping", ROOT / "examples/pack/textures.json", "--output", destination, "--pack-format", major, "--pack-format-minor", minor]
        report = json.loads(run_cli(server, *command).stdout)
        assert report["converted_textures"] == 1
        assert report["java_pack_format_minor"] == minor
        original = (ROOT / "examples/pack/bedrock/textures/blocks/sculith_demo.png").read_bytes()
        assert (destination / "assets/minecraft/textures/block/stone.png").read_bytes() == original
        metadata = json.loads((destination / "pack.mcmeta").read_text(encoding="utf-8"))
        assert metadata["pack"]["min_format"] == [major, minor]
        assert metadata["pack"]["max_format"] == [major, minor]
        run_cli(server, *command, ok=False)
        assert (destination / "assets/minecraft/textures/block/stone.png").read_bytes() == original

    print("PASS: shared Java TCP + Bedrock UDP discovery, Rust plugin, ping, login rejection, malformed traffic, shutdown, world save/restart, foreign-world preservation, config and PNG pack conversion" + (", built-executable RakNet connection request" if sessions else ""))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    suffix = ".exe" if os.name == "nt" else ""
    parser.add_argument("--server", type=Path, default=ROOT / ("target/debug/sculith" + suffix))
    parser.add_argument("--plugin", type=Path, default=ROOT / ("target/debug/sculith-welcome" + suffix))
    parser.add_argument("--sessions", action="store_true", help="also enable the loopback-only Bedrock spawn preview and probe its RakNet driver")
    args = parser.parse_args()
    smoke(args.server.resolve(strict=True), args.plugin.resolve(strict=True), args.sessions)
