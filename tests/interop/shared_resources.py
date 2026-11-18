"""Real Java TCP and Bedrock NetherNet resource transfer, without game rendering."""
import argparse
import hashlib
import io
import json
import os
from pathlib import Path
import shutil
import socket
import struct
import subprocess
import tempfile
import time
from urllib.error import HTTPError
from urllib.request import Request, urlopen
import uuid
import zipfile
import zlib

ROOT = Path(__file__).resolve().parents[2]
LIMIT = 8 * 1024 * 1024
PACKETS = json.loads((ROOT / "assets/packets.json").read_text())


def nbt_string(value):
    # Java's modified UTF-8 represents supplementary characters as surrogate pairs.
    utf16 = value.encode("utf-16-be")
    units = "".join(chr(int.from_bytes(utf16[i:i + 2], "big")) for i in range(0, len(utf16), 2))
    data = units.encode("utf-8", "surrogatepass").replace(b"\0", b"\xc0\x80")
    return struct.pack(">H", len(data)) + data


def read_nbt(fields, tag=None):
    tag = fields.take(1)[0] if tag is None else tag
    if tag == 0:
        return None
    if tag in (1, 2, 3, 4, 5, 6):
        format_ = {1: ">b", 2: ">h", 3: ">i", 4: ">q", 5: ">f", 6: ">d"}[tag]
        return struct.unpack(format_, fields.take(struct.calcsize(format_)))[0]
    if tag == 8:
        length, = struct.unpack(">H", fields.take(2))
        text = fields.take(length).replace(b"\xc0\x80", b"\0").decode("utf-8", "surrogatepass")
        return text.encode("utf-16-le", "surrogatepass").decode("utf-16-le")
    if tag == 9:
        child = fields.take(1)[0]
        count, = struct.unpack(">i", fields.take(4))
        assert 0 <= count <= LIMIT
        return [read_nbt(fields, child) for _ in range(count)]
    if tag == 10:
        out = {}
        while True:
            child = fields.take(1)[0]
            if child == 0:
                return out
            name = read_nbt(fields, 8)
            out[name] = read_nbt(fields, child)
    raise AssertionError(("unexpected fixture tag", tag))


def varint(value):
    out = bytearray()
    while value >= 128:
        out.append((value & 127) | 128)
        value >>= 7
    return bytes(out) + bytes([value])


def string(value):
    data = value.encode()
    return varint(len(data)) + data


class Fields:
    def __init__(self, data):
        self.data = memoryview(data)
        self.offset = 0

    def take(self, length):
        assert 0 <= length <= len(self.data) - self.offset
        out = self.data[self.offset:self.offset + length].tobytes()
        self.offset += length
        return out

    def vi(self):
        value = 0
        for index in range(5):
            byte = self.take(1)[0]
            value |= (byte & 127) << (index * 7)
            if byte < 128:
                assert value <= LIMIT
                return value
        raise AssertionError("oversized VarInt")

    def text(self):
        return self.take(self.vi()).decode()

    def rest(self):
        return self.take(len(self.data) - self.offset)


class Java:
    def __init__(self, port, name="ResourceJava"):
        self.socket = socket.create_connection(("127.0.0.1", port), timeout=10)
        self.socket.settimeout(40)
        self.compression = None
        self.send(0, varint(777) + string("127.0.0.1") + struct.pack(">H", port) + varint(2))
        self.send(0, string(name) + uuid.UUID(int=0).bytes)
        for _ in range(10):
            id_, data = self.receive()
            if id_ == 3:
                self.compression = data.vi()
            elif id_ == 2:
                self.send(3)
                self.send(0, string("en_us") + bytes([2, 0, 1, 127, 1, 0, 1, 0]))
                return
            else:
                raise AssertionError(("login", id_, data.rest()))
        raise AssertionError("login did not complete")

    def exact(self, length):
        out = bytearray()
        while len(out) < length:
            chunk = self.socket.recv(length - len(out))
            if not chunk:
                raise EOFError("Java connection closed")
            out.extend(chunk)
        return bytes(out)

    def receive(self):
        prefix = bytearray()
        for _ in range(5):
            prefix.extend(self.exact(1))
            if prefix[-1] < 128:
                break
        size = Fields(prefix).vi()
        assert 0 < size <= LIMIT
        data = self.exact(size)
        if self.compression is not None:
            fields = Fields(data)
            size = fields.vi()
            compressed = fields.rest()
            if size:
                decoder = zlib.decompressobj()
                data = decoder.decompress(compressed, size + 1)
                assert len(data) == size and decoder.eof
            else:
                data = compressed
        fields = Fields(data)
        return fields.vi(), fields

    def frame(self, id_, payload=b""):
        data = varint(id_) + payload
        if self.compression is not None:
            data = (varint(len(data)) + zlib.compress(data)) if len(data) >= self.compression else bytes([0]) + data
        return varint(len(data)) + data

    def send(self, id_, payload=b""):
        self.socket.sendall(self.frame(id_, payload))

    def join_with_resources(self, port, output, delayed):
        archive = None
        pack_id = None
        heartbeat_count = 0
        known_count = 0
        for _ in range(256):
            id_, fields = self.receive()
            if id_ == 9:
                assert pack_id is None, "duplicate resource offer"
                pack_id = fields.take(16)
                url, sha1 = fields.text(), fields.text()
                assert fields.take(1) == bytes([1]), "pack must be required"
                assert fields.take(1) == bytes([0]), "unexpected prompt component"
                assert url.startswith(f"http://127.0.0.1:{port}/sculith/packs/")
                with urlopen(url, timeout=10) as response:
                    archive = response.read(LIMIT)
                assert hashlib.sha1(archive).hexdigest() == sha1
                (output / "java.zip").write_bytes(archive)
                with urlopen(Request(url, method="HEAD"), timeout=10) as response:
                    assert int(response.headers["Content-Length"]) == len(archive)
                    assert response.read() == b""
                try:
                    urlopen(url + "/../level.dat", timeout=10)
                except HTTPError as error:
                    assert error.code == 404
                else:
                    raise AssertionError("unexpected resource path accepted")
                # Neither unknown IDs nor a download-only ACK may finish configuration.
                self.send(6, uuid.uuid4().bytes + varint(0))
                self.send(6, pack_id + varint(3))
                self.send(6, pack_id + varint(4))
                if not delayed:
                    self.send(6, pack_id + varint(0))
            elif id_ == 4:
                heartbeat_count += 1
                pong = fields.take(8)
                if delayed and heartbeat_count == 1:
                    self.send(4, pong)
                    loaded = self.frame(6, pack_id + varint(0))
                    # Hold an incomplete frame across the next server heartbeat.
                    self.socket.sendall(loaded[:1])
                elif delayed and heartbeat_count == 2:
                    self.socket.sendall(loaded[1:])
                    self.send(4, pong)
                    self.send(6, pack_id + varint(0))
                else:
                    self.send(4, pong)
            elif id_ == 15:
                assert archive is not None
                assert not delayed or heartbeat_count >= 2, "advanced before pack application"
                known_count += 1
                assert known_count == 1, "duplicate known-pack exchange"
                self.send(7, fields.rest())
            elif id_ == 3:
                assert known_count == 1
                self.send(3)
                break
            elif id_ == 2:
                raise AssertionError(("configuration disconnect", fields.rest()))
        else:
            raise AssertionError("configuration did not finish")
        for _ in range(256):
            id_, fields = self.receive()
            if id_ == 0x32:
                return {"url": url, "sha1": sha1, "keepalives": heartbeat_count,
                        "known_pack_exchanges": known_count, "play_login": True}
            if id_ == 0x20:
                raise AssertionError(("play disconnect", fields.rest()))
        raise AssertionError("no play login")

    def check_plugin_sounds(self):
        played, stopped = False, False
        for _ in range(512):
            id_, fields = self.receive()
            if id_ == 119:
                assert fields.vi() == 0
                assert fields.text() == "sculith:demo_tone"
                assert fields.take(1) == bytes([0])
                assert fields.vi() == 0
                assert struct.unpack(">iii", fields.take(12)) == (-10, -484, 31)
                assert struct.unpack(">ff", fields.take(8)) == (2.5, .75)
                played = True
            elif id_ == 121:
                assert fields.take(1) == bytes([2])
                assert fields.text() == "sculith:demo_tone"
                stopped = True
            if played and stopped:
                self.send(44)  # Java 26.3 player_loaded, observed by the Rust fixture.
                return
        raise AssertionError("missing plugin sound or named stop")

    def check_plugin_world_sound(self):
        for _ in range(512):
            id_, fields = self.receive()
            if id_ != 119 or fields.vi() != 0:
                continue
            assert fields.text() == "sculith:demo_tone"
            assert fields.take(1) == bytes([0]) and fields.vi() == 0
            assert struct.unpack(">iii", fields.take(12)) == (9, -482, 36)
            assert struct.unpack(">ff", fields.take(8)) == (.625, 1.25)
            return
        raise AssertionError("missing shared world sound broadcast")

    def check_common_forms(self, output):
        previous = None
        for stage in range(4):
            for _ in range(2048):
                id_, fields = self.receive()
                if id_ == PACKETS["clientbound"]["play"]["keep_alive"]:
                    self.send(PACKETS["serverbound"]["play"]["keep_alive"], fields.take(8))
                elif id_ == PACKETS["clientbound"]["play"]["show_dialog"]:
                    assert fields.vi() == 0
                    nbt = fields.rest()
                    (output / f"java-form-{stage}.nbt").write_bytes(nbt)
                    dialog_fields = Fields(nbt)
                    dialog = read_nbt(dialog_fields)
                    assert not dialog_fields.rest()
                    break
                elif id_ == PACKETS["clientbound"]["play"]["disconnect"]:
                    raise AssertionError(("form disconnect", fields.rest()))
            else:
                raise AssertionError(("missing shared form", stage))
            assert dialog["type"] == "minecraft:multi_action" and dialog["pause"] == 0
            assert dialog["can_close_with_escape"] == 1 and dialog["after_action"] == "close"
            if previous:
                self.send(PACKETS["serverbound"]["play"]["custom_click_action"], previous)
            if stage < 2:
                action = dialog["actions"][stage]["action"]["id"]
                payload = b"\0"
                bad = action.rsplit("/", 1)[0] + "/99"
                self.send(68, string(bad) + b"\1\0")
                self.send(68, string("sculith:ui/forged/0") + b"\1\0")
            elif stage == 2:
                inputs = dialog["inputs"]
                assert [value["key"] for value in inputs] == ["f1", "f2", "f3", "f4", "f5"]
                assert [value["type"] for value in inputs] == ["minecraft:boolean", "minecraft:number_range", "minecraft:single_option", "minecraft:single_option", "minecraft:text"]
                action = dialog["actions"][0]["action"]["id"]
                assert dialog["actions"][0]["action"]["type"] == "minecraft:dynamic/custom"
                payload = (b"\x0a\x01" + nbt_string("f1") + b"\x01\x05" + nbt_string("f2") + struct.pack(">f", 2.5)
                    + b"".join(b"\x08" + nbt_string(key) + nbt_string(value)
                    for key, value in [("f3", "1"), ("f4", "0"), ("f5", "가나다😀")]) + b"\0")
                self.send(68, string(action) + b"\1\0")  # Missing inputs must leave the request pending.
            else:
                action = dialog["exit_action"]["action"]["id"]
                payload = b"\0"
            previous = string(action) + varint(len(payload)) + payload
            self.send(PACKETS["serverbound"]["play"]["custom_click_action"], previous)
        self.send(PACKETS["serverbound"]["play"]["custom_click_action"], previous)


def free_port():
    with socket.socket() as stream:
        stream.bind(("127.0.0.1", 0))
        return stream.getsockname()[1]


def configure(path, changes):
    section = ""
    out = []
    for line in path.read_text().splitlines():
        text = line.strip()
        if text.startswith("["):
            section = text[1:-1]
        key = text.split("=", 1)[0].strip()
        if (section, key) in changes:
            line = key + " = " + changes[(section, key)]
        out.append(line)
    path.write_text("\n".join(out) + "\n")


def stop_server(server):
    if server.poll() is None:
        try:
            server.stdin.write("stop\n")
            server.stdin.flush()
        except BrokenPipeError:
            pass
        try:
            server.wait(timeout=30)
        except subprocess.TimeoutExpired:
            server.terminate()
            try:
                server.wait(timeout=10)
            except subprocess.TimeoutExpired:
                server.kill()
                server.wait(timeout=10)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("server", type=Path)
    parser.add_argument("bedrock_probe", type=Path)
    parser.add_argument("--world", type=Path, required=True, help="stopped Java flat world copied for testing")
    parser.add_argument("--delayed", action="store_true", help="hold a partial packet across two keepalives")
    parser.add_argument("--building", action="store_true", help="also exercise native building in the copied flat world")
    parser.add_argument("--picking", action="store_true", help="check native pick-block hotbar selection and inventory preservation")
    parser.add_argument("--actor-picking", action="store_true", help="check native mob picking, reach and entity ID bounds")
    parser.add_argument("--pick-events", action="store_true", help="check native plugin cancellation and callback inventory changes on both editions")
    parser.add_argument("--creative-inventory", action="store_true", help="check native creative permissions, catalogue IDs and stack limits")
    parser.add_argument("--creative-events", action="store_true", help="check creative veto, correction and callback inventory changes on both editions")
    parser.add_argument("--inventory-requests", action="store_true", help="check ordinary native requests, bounds and atomic rejection")
    parser.add_argument("--inventory-events", action="store_true", help="check native inventory/drop callbacks and Java inventory cancellation")
    parser.add_argument("--legacy-inventory", action="store_true", help="check legacy slot queries, journals, held items and verified drops")
    parser.add_argument("--legacy-events", action="store_true", help="check legacy drop and held-slot cancellation through Rust callbacks")
    parser.add_argument("--click-events", action="store_true", help="check shared inventory click callbacks, vetoes and native snapshot payloads")
    parser.add_argument("--tracking-events", action="store_true", help="remove a tracked mob from its outgoing spawn callback")
    parser.add_argument("--plugin", type=Path, help="compiled tests/interop/plugin Wasm fixture")
    parser.add_argument("--dialog-replies", action="store_true", help="deliver official Java reply fixtures to the Rust plugin")
    parser.add_argument("--forms", action="store_true", help="exercise shared forms and Rust response events on both editions")
    parser.add_argument("--models", action="store_true", help="exercise shared model lifecycle, late joins and saved state")
    parser.add_argument("--packet-edits", action="store_true", help="exercise outgoing packet edits and cancellation through Rust callbacks")
    parser.add_argument("--connections-plugin", type=Path, help="compiled native tests/interop/connections fixture")
    args = parser.parse_args()
    if (args.pick_events or args.tracking_events or args.creative_events or args.inventory_events or args.legacy_events or args.click_events) and not args.connections_plugin:
        parser.error("event checks require --connections-plugin")
    if (args.dialog_replies or args.forms or args.models or args.packet_edits) and not args.plugin:
        parser.error("--dialog-replies, --forms, --models and --packet-edits require the Rust --plugin fixture")
    root = Path(tempfile.mkdtemp(prefix="sculith-shared-resources-"))
    print("EVIDENCE", root, flush=True)
    server_path = args.server.resolve()
    fixture = ROOT / "crates/sculith-packs/tests/fixtures"
    shutil.copytree(fixture / "pack", root / "source")
    shutil.copy2(fixture / "textures.json", root / "mapping.json")
    if args.models:
        mapping = json.loads((root / "mapping.json").read_text())
        mapping["models"][0]["bedrock"] = "sculith:native_statue"
        (root / "mapping.json").write_text(json.dumps(mapping))
        entity_file = root / "source" / mapping["models"][0]["entity"]
        entity = json.loads(entity_file.read_text())
        entity["minecraft:client_entity"]["description"]["identifier"] = "sculith:native_statue"
        entity_file.write_text(json.dumps(entity))
    (root / "source/credits.bin").write_bytes(bytes(range(256)) * 9000)
    shutil.copytree(args.world, root / "world")
    if args.plugin or args.connections_plugin:
        (root / "plugins").mkdir()
    if args.plugin:
        shutil.copy2(args.plugin, root / "plugins/resource-probe.wasm")
    if args.connections_plugin:
        shutil.copy2(args.connections_plugin, root / "plugins" / args.connections_plugin.name)
    env = dict(os.environ, RUST_LOG=os.environ.get("SCULITH_INTEROP_LOG", "info"), NO_COLOR="1")
    subprocess.run([str(server_path), "--init-settings"], cwd=root, env=env, check=True, capture_output=True)
    java, bedrock = free_port(), free_port()
    changes = {
        ("", "allow_nether"): "false", ("", "allow_end"): "false",
        ("", "default_gamemode"): '"Creative"', ("", "force_gamemode"): "true",
        ("networking.java", "address"): json.dumps(f"127.0.0.1:{java}"),
        ("networking.java", "online_mode"): "false", ("networking.java", "encryption"): "false",
        ("networking.java.authentication", "enabled"): "false",
        ("networking.bedrock", "online_mode"): "false",
        ("networking.bedrock.authentication", "enabled"): "false",
        ("networking.bedrock.nethernet", "address"): json.dumps(f"127.0.0.1:{bedrock}"),
        ("world.chunk", "type"): '"anvil"',
    }
    if args.building or args.legacy_inventory:
        changes[("", "spawn_protection")] = "0"
    for edition in ["java", "bedrock"]:
        for key in ["view_distance", "simulation_distance"]:
            changes[(f"networking.{edition}", key)] = "2"
    configure(root / "pumpkin.toml", changes)
    with (root / "pumpkin.toml").open("a") as config:
        config.write('[resource_pack.shared]\nsource="source"\nmapping="mapping.json"\nrequired=true\n')
        if args.plugin:
            config.write('[plugins.overrides.sculith-wasm-audio-probe]\nallow_unsigned=true\n')
        if args.connections_plugin:
            config.write('[plugins.overrides.sculith-interop-connections]\nallow_unsigned=true\n')
    log = (root / "server.log").open("w")
    server = subprocess.Popen([str(server_path)], cwd=root, env=env, stdin=subprocess.PIPE, stdout=log, stderr=log, text=True)
    client = None
    try:
        deadline = time.monotonic() + 30
        while "Server is now running" not in (root / "server.log").read_text():
            assert server.poll() is None, (root / "server.log").read_text()
            assert time.monotonic() < deadline, "server startup timed out"
            time.sleep(.1)
        for rule in ["spawn_mobs", "spawn_monsters", "mob_drops"]:
            server.stdin.write(f"gamerule {rule} false\n")
        server.stdin.flush()
        deadline = time.monotonic() + 5
        while not any("mob_drops" in row and "false" in row and "set" in row
                      for row in (root / "server.log").read_text().splitlines()):
            assert server.poll() is None and time.monotonic() < deadline, "scratch mob-drop rule was not applied"
            time.sleep(.05)
        original = {str(p.relative_to(root / "source")): p.read_bytes() for p in (root / "source").rglob("*") if p.is_file()}
        # Both edition archives must remain frozen if the source changes mid-session.
        (root / "source/manifest.json").write_text("not a valid replacement pack")
        client = Java(java)
        report = client.join_with_resources(java, root, args.delayed)
        if args.plugin:
            client.check_plugin_sounds()
            client.check_plugin_world_sound()
            if args.dialog_replies:
                for case in ["none", "small", "large"]:
                    data = (ROOT / f"crates/pumpkin-protocol/tests/fixtures/java_custom_click_{case}_777.bin").read_bytes()
                    client.send(68, data)  # Java 26.3 custom_click_action.
                deadline = time.monotonic() + 10
                while True:
                    replies = [line for line in (root / "server.log").read_text().splitlines()
                               if "PROBE_DIALOG_REPLY" in line and "player=ResourceJava" in line]
                    if all(any(f"case={case}" in line for line in replies) for case in ["none", "small", "large"]):
                        break
                    assert server.poll() is None and time.monotonic() < deadline, "missing Rust dialog reply event"
                    time.sleep(.1)
                report["java_dialog_replies"] = "official None/14/173-byte payloads delivered to Rust Wasm"
            result = subprocess.run([str(args.bedrock_probe.resolve()), f"http://127.0.0.1:{bedrock}", "--sound-check"],
                                    capture_output=True, text=True, timeout=35)
            (root / "sounds.log").write_text(result.stdout + result.stderr)
            assert result.returncode == 0, result.stdout + result.stderr
            client.check_plugin_world_sound()
            log_text = (root / "server.log").read_text()
            assert "PROBE_LOADED" in log_text
            for player in ["ResourceJava", "ForkProbe"]:
                for event in ["PROBE_JOIN", "PROBE_RECEIVED", "PROBE_SENT"]:
                    assert any(event in line and f"player={player}" in line
                               for line in log_text.splitlines()), ("missing plugin event", player, event)
            report["rust_wasm_sound_dispatch"] = "player and world; both editions; named stop; packet observer enabled"
            if args.forms:
                form_client = Java(java, "FormJava")
                try:
                    form_client.join_with_resources(java, root, False)
                    form_client.check_common_forms(root)
                    result = subprocess.run([str(args.bedrock_probe.resolve()), f"http://127.0.0.1:{bedrock}", "--forms-check"], capture_output=True, text=True, timeout=35)
                    (root / "forms.log").write_text(result.stdout + result.stderr)
                    assert result.returncode == 0, result.stdout + result.stderr
                    deadline = time.monotonic() + 10
                    while True:
                        lines = (root / "server.log").read_text().splitlines()
                        if all(any("PROBE_FORMS_COMPLETE" in line and f"player={player}" in line for line in lines) for player in ["FormJava", "FormBedrock"]):
                            break
                        assert server.poll() is None and time.monotonic() < deadline, "missing common form completion"
                        time.sleep(.1)
                    for player in ["FormJava", "FormBedrock"]:
                        replies = [line for line in lines if "PROBE_FORM_REPLY" in line and f"player={player}" in line]
                        assert len(replies) == 4, ("unexpected form callback count", player, replies)
                    report["shared_forms"] = "simple/modal/custom/cancel; Rust callbacks; busy and invalid/replayed replies rejected"
                finally:
                    form_client.socket.close()
        result = subprocess.run([str(args.bedrock_probe.resolve()), f"http://127.0.0.1:{bedrock}", "--pack-output", str(root / "bedrock.mcpack")], capture_output=True, text=True, timeout=35)
        (root / "bedrock.log").write_text(result.stdout + result.stderr)
        assert result.returncode == 0, result.stdout + result.stderr
        with zipfile.ZipFile(root / "bedrock.mcpack") as archive:
            for name, content in original.items():
                if name != "manifest.json":
                    assert archive.read(name) == content, name
        with zipfile.ZipFile(root / "java.zip") as archive:
            assert "pack.mcmeta" in archive.namelist()
            assert archive.read("assets/sculith/sounds/demo_tone.ogg") == original["sounds/sculith_demo.ogg"]
            assert "assets/sculith/models/sculith/demo_statue.json" in archive.namelist()
        if args.building or args.picking or args.actor_picking or args.pick_events or args.tracking_events or args.creative_events or args.inventory_requests or args.inventory_events or args.legacy_inventory or args.legacy_events or args.click_events:
            server.stdin.write("kill @e[type=!minecraft:player]\n")
            server.stdin.flush()
            time.sleep(.5)
        if args.building:
            result = subprocess.run([str(args.bedrock_probe.resolve()), f"http://127.0.0.1:{bedrock}"],
                                    capture_output=True, text=True, timeout=35)
            (root / "building.log").write_text(result.stdout + result.stderr)
            assert result.returncode == 0, result.stdout + result.stderr
            report["native_building_paths"] = 4
        if args.picking:
            result = subprocess.run([str(args.bedrock_probe.resolve()), f"http://127.0.0.1:{bedrock}", "--pick-check"],
                                    capture_output=True, text=True, timeout=35)
            (root / "picking.log").write_text(result.stdout + result.stderr)
            assert result.returncode == 0, result.stdout + result.stderr
            report["native_picking"] = "new and existing items; hotbar selection; inventory preserved"
        if args.actor_picking or args.pick_events or args.tracking_events:
            for x in [1, 12]:
                server.stdin.write(f'summon minecraft:pig {x} -60 2 {{NoAI:1b}}\n')
            server.stdin.flush()
            time.sleep(.5)
        if args.actor_picking:
            result = subprocess.run([str(args.bedrock_probe.resolve()), f"http://127.0.0.1:{bedrock}", "--actor-pick-check"],
                                    capture_output=True, text=True, timeout=35)
            (root / "actor-picking.log").write_text(result.stdout + result.stderr)
            assert result.returncode == 0, result.stdout + result.stderr
            report["native_actor_picking"] = "create/reuse correct egg; reach and 64-bit ID bounds"
        if args.pick_events:
            from pick_events import check
            report["pick_events"] = check(java, bedrock, root, args.bedrock_probe.resolve())
        if args.creative_events:
            from creative_events import check
            report["creative_events"] = check(server, java, bedrock, root, args.bedrock_probe.resolve())
        if args.inventory_requests:
            from inventory_requests import check
            report["inventory_requests"] = check(server, bedrock, root, args.bedrock_probe.resolve())
        if args.inventory_events:
            from inventory_requests import events
            report["inventory_events"] = events(server, java, bedrock, root, args.bedrock_probe.resolve())
        if args.click_events:
            from inventory_requests import clicks
            report["click_events"] = clicks(server, java, bedrock, root, args.bedrock_probe.resolve())
        if args.legacy_inventory or args.legacy_events:
            from inventory_requests import legacy
            report["legacy_inventory"] = legacy(server, bedrock, root, args.bedrock_probe.resolve(), args.legacy_inventory, args.legacy_events)
        if args.tracking_events:
            for option, name in [("--tracking-events", "TrackerNative"), ("--tracking-data-events", "TrackerDataNative")]:
                if name == "TrackerDataNative":
                    server.stdin.write('summon minecraft:pig 1 -60 2 {NoAI:1b}\n')
                    server.stdin.flush()
                    time.sleep(.5)
                result = subprocess.run([str(args.bedrock_probe.resolve()), f"http://127.0.0.1:{bedrock}", option],
                                        capture_output=True, text=True, timeout=35)
                (root / f"{name}.log").write_text(result.stdout + result.stderr)
                assert result.returncode == 0, result.stdout + result.stderr
                rows = [row for row in (root / "server.log").read_text().splitlines()
                        if "PROBE_TRACKING_REMOVE_" in row and f"player={name} " in row]
                assert sum("PROBE_TRACKING_REMOVE_BEGIN" in row for row in rows) == 1, rows
                assert sum("PROBE_TRACKING_REMOVE_END" in row and "removed=true" in row for row in rows) == 1, rows
            report["tracking_events"] = "Rust outgoing spawn and motion callbacks remove their mobs and cancel packets without stalling tracking"
        if args.creative_inventory:
            try:
                for mode, option in [("survival", "--survival-creative-check"), ("creative", "--creative-inventory-check")]:
                    server.stdin.write(f"defaultgamemode {mode}\n")
                    server.stdin.flush()
                    time.sleep(.3)
                    result = subprocess.run([str(args.bedrock_probe.resolve()), f"http://127.0.0.1:{bedrock}", option],
                                            capture_output=True, text=True, timeout=35)
                    (root / f"creative-{mode}.log").write_text(result.stdout + result.stderr)
                    assert result.returncode == 0, result.stdout + result.stderr
            finally:
                server.stdin.write("defaultgamemode creative\n")
                server.stdin.flush()
            time.sleep(.3)
            result = subprocess.run([str(args.bedrock_probe.resolve()), f"http://127.0.0.1:{bedrock}", "--creative-flow"],
                                    capture_output=True, text=True, timeout=35)
            (root / "creative-flow.log").write_text(result.stdout + result.stderr)
            assert result.returncode == 0, result.stdout + result.stderr
            report["native_creative_inventory"] = "Survival rejects creation; Creative creates one legal stack; invalid catalogue IDs reject the whole request"
        if args.packet_edits:
            from packet_probe import check
            report["packet_edits"] = check(java, bedrock, root, args.bedrock_probe.resolve())
        if args.connections_plugin:
            from packet_probe import connections
            report["connection_events"] = connections(java, bedrock, root, args.bedrock_probe.resolve())
        if args.models:
            from model_probe import lifecycle
            report["shared_models"] = lifecycle(java, bedrock, root, args.bedrock_probe.resolve())
        report.update({"bedrock_spawn": True, "frozen_source": True,
                       "scope": "independent protocol clients; no rendering claim"})
        (root / "result.json").write_text(json.dumps(report, indent=2) + "\n")
        print("PASS", json.dumps(report), flush=True)
    finally:
        if client is not None:
            client.socket.close()
        stop_server(server)
        log.close()
    assert server.returncode == 0, f"server exit {server.returncode}"
    if args.models:
        from model_probe import restored
        (root / "source/manifest.json").write_bytes(original["manifest.json"])
        restart_log = (root / "models-restart-server.log").open("w")
        restarted = subprocess.Popen([str(server_path)], cwd=root, env=env, stdin=subprocess.PIPE,
                                     stdout=restart_log, stderr=restart_log, text=True)
        try:
            deadline = time.monotonic() + 30
            while "Server is now running" not in (root / "models-restart-server.log").read_text():
                assert restarted.poll() is None and time.monotonic() < deadline, "model restart failed"
                time.sleep(.1)
            restored(java, bedrock, root, args.bedrock_probe.resolve(), report["shared_models"]["uuid"])
            report["shared_models"]["normal_restart"] = True
            (root / "result.json").write_text(json.dumps(report, indent=2) + "\n")
        finally:
            stop_server(restarted)
            restart_log.close()
        assert restarted.returncode == 0, f"restart exit {restarted.returncode}"
        print("PASS MODELS", json.dumps(report["shared_models"]), flush=True)


if __name__ == "__main__":
    main()
