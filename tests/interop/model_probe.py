"""Shared model wire checks, using a stock Java protocol and an independent native decoder."""

import json
import os
from pathlib import Path
import selectors
import struct
import subprocess
import time

from shared_resources import Fields, Java, PACKETS, ROOT, read_nbt, string

LAYOUT = json.loads((ROOT / "assets/tracked_data.json").read_text())["item_display"]
TYPES = json.loads((ROOT / "assets/meta_data_type.json").read_text())
COMPONENTS = json.loads((ROOT / "assets/data_component.json").read_text())
MODEL_TYPE = json.loads((ROOT / "assets/entities.json").read_text())["item_display"]["id"]
PAPER = json.loads((ROOT / "assets/items.json").read_text())["paper"]["id"]
CB = PACKETS["clientbound"]["play"]
SB = PACKETS["serverbound"]["play"]


def signed_vi(fields):
    value = 0
    for index in range(5):
        byte = fields.take(1)[0]
        value |= (byte & 127) << (7 * index)
        if byte < 128:
            assert value <= 0xffffffff
            return value if value < 0x80000000 else value - (1 << 32)
    raise AssertionError("invalid signed VarInt")


def item(fields):
    assert fields.vi() == 1 and fields.vi() == PAPER
    added, removed = fields.vi(), fields.vi()
    assert added == 2 and removed == 0
    values = {}
    for _ in range(added):
        type_ = fields.vi()
        if type_ == COMPONENTS["minecraft:item_model"]["id"]:
            values["model"] = fields.text()
        elif type_ == COMPONENTS["minecraft:custom_data"]["id"]:
            values["data"] = read_nbt(fields)
        else:
            raise AssertionError(("unexpected carrier component", type_))
    assert values == {"model": "sculith:demo_statue", "data": {"sculith": {"model": "sculith:demo_statue"}}}
    return values


def metadata(fields, entity_id):
    values = {}
    while True:
        index = fields.take(1)[0]
        if index == 255:
            assert not fields.rest()
            return entity_id, values
        type_ = fields.vi()
        if type_ in [TYPES["byte"], TYPES["boolean"]]:
            value = fields.take(1)[0]
        elif type_ == TYPES["int"]:
            value = signed_vi(fields)
        elif type_ == TYPES["float"]:
            value, = struct.unpack(">f", fields.take(4))
        elif type_ == TYPES["vector3"]:
            value = struct.unpack(">fff", fields.take(12))
        elif type_ == TYPES["quaternion"]:
            value = struct.unpack(">ffff", fields.take(16))
        elif type_ == TYPES["item_stack"]:
            value = item(fields)
        else:
            raise AssertionError(("unexpected display metadata type", index, type_))
        values[index] = value


class ModelJava(Java):
    def __init__(self, port, name):
        super().__init__(port, name)
        self.model_id = None
        self.values = {}

    def enter(self, port, output):
        self.join_with_resources(port, output, False)
        self.send(SB["player_loaded"])

    def event(self):
        id_, fields = self.receive()
        if id_ == CB["keep_alive"]:
            self.send(SB["keep_alive"], fields.take(8))
        elif id_ == CB["chunk_batch_finished"]:
            self.send(SB["chunk_batch_received"], struct.pack(">f", 8.0))
        elif id_ == CB["disconnect"]:
            raise AssertionError(("model client disconnected", fields.rest()))
        return id_, fields

    def spawn(self, position, scale):
        self.model_id, self.values = None, {}
        for _ in range(8192):
            id_, fields = self.event()
            if id_ == CB["add_entity"]:
                entity_id, identity, type_ = fields.vi(), fields.take(16), fields.vi()
                pos = struct.unpack(">ddd", fields.take(24))
                if type_ == MODEL_TYPE:
                    assert self.model_id is None, "duplicate shared model"
                    assert pos == position
                    self.model_id, self.identity = entity_id, identity.hex()
            elif id_ == CB["set_entity_data"]:
                entity_id = fields.vi()
                if entity_id == self.model_id:
                    entity_id, values = metadata(fields, entity_id)
                    self.values.update(values)
            if self.model_id and LAYOUT["DATA_ITEM_STACK_ID"]["id"] in self.values:
                self.check_scale(scale)
                assert self.values[LAYOUT["DATA_RIGHT_ROTATION_ID"]["id"]] == (0.0, 1.0, 0.0, 0.0)
                return self.identity
        raise AssertionError("missing shared model spawn or item metadata")

    def check_scale(self, scale):
        assert self.values[LAYOUT["DATA_SCALE_ID"]["id"]] == (scale, scale, scale)
        assert self.values[LAYOUT["DATA_TRANSLATION_ID"]["id"]] == (0.0, scale * .5, 0.0)

    def command(self, command):
        self.send(SB["custom_click_action"], string("sculith:probe/model/" + command) + b"\1\0")

    def scaled(self):
        for _ in range(8192):
            id_, fields = self.event()
            if id_ == CB["set_entity_data"]:
                entity_id = fields.vi()
                if entity_id == self.model_id:
                    entity_id, values = metadata(fields, entity_id)
                    self.values.update(values)
                    if self.values.get(LAYOUT["DATA_SCALE_ID"]["id"]) == (2.0, 2.0, 2.0):
                        self.check_scale(2.0)
                        return
        raise AssertionError("missing Java model scale update")

    def moved(self):
        for _ in range(8192):
            id_, fields = self.event()
            if id_ == CB["entity_position_sync"] and fields.vi() == self.model_id:
                assert fields.vi() == 0, "expected a Java 26.3 linear position path"
                position = struct.unpack(">ddd", fields.take(24))
                if position == (1.0, -60.0, 4.0):
                    continue
                assert position == (3.0, -60.0, 4.0), position
                return
        raise AssertionError("missing Java model teleport")

    def removed(self):
        for _ in range(8192):
            id_, fields = self.event()
            if id_ == CB["remove_entities"]:
                ids = [fields.vi() for _ in range(fields.vi())]
                if self.model_id in ids:
                    return
        raise AssertionError("missing Java model removal")


class Native:
    def __init__(self, probe, port, root):
        self.log = (root / "models-native.log").open("wb")
        self.proc = subprocess.Popen([str(probe), f"http://127.0.0.1:{port}", "--models-check"], stdout=subprocess.PIPE, stderr=self.log, bufsize=0)
        self.selector = selectors.DefaultSelector()
        self.selector.register(self.proc.stdout, selectors.EVENT_READ)
        self.pending = bytearray()

    def expect(self, marker):
        deadline = time.monotonic() + 20
        while True:
            if b"\n" in self.pending:
                line, _, remaining = self.pending.partition(b"\n")
                self.pending = bytearray(remaining)
                if marker in line.decode():
                    return
                continue
            assert time.monotonic() < deadline, ("native model phase timed out", marker)
            for key, _ in self.selector.select(.5):
                data = os.read(key.fd, 4096)
                assert data, ("native model probe closed", marker)
                self.log.write(data)
                self.log.flush()
                self.pending.extend(data)

    def close(self):
        self.selector.close()
        if self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait(timeout=5)
        self.log.close()


def lifecycle(java_port, native_port, root, probe):
    owner = ModelJava(java_port, "ModelOwner")
    native = None
    viewer = None
    try:
        owner.enter(java_port, root)
        owner.spawn((1.0, -60.0, 4.0), 1.25)
        native = Native(probe, native_port, root)
        native.expect("MODEL_READY")
        owner.command("scale")
        owner.scaled()
        native.expect("MODEL_SCALED")
        owner.command("move")
        owner.moved()
        native.expect("MODEL_MOVED")
        viewer = ModelJava(java_port, "ModelViewer")
        viewer.enter(java_port, root)
        assert viewer.spawn((3.0, -60.0, 4.0), 2.0) == owner.identity
        assert viewer.model_id == owner.model_id
        result = subprocess.run([str(probe), f"http://127.0.0.1:{native_port}", "--model-snapshot"], capture_output=True, timeout=35, text=True)
        (root / "models-late-native.log").write_text(result.stdout + result.stderr)
        assert result.returncode == 0, result.stdout + result.stderr
        owner.command("remove")
        owner.removed()
        viewer.removed()
        native.expect("PASS:")
        assert native.proc.wait(timeout=5) == 0
        owner.command("spawn")
        identity = owner.spawn((1.0, -60.0, 4.0), 1.25)
        owner.command("scale")
        owner.scaled()
        owner.command("move")
        owner.moved()
        return {"uuid": identity, "paths": "spawn/scale/move/remove; both editions; late watchers; cancelled spawn; rejected invalid input"}
    finally:
        if native:
            native.close()
        owner.socket.close()
        if viewer:
            viewer.socket.close()


def restored(java_port, native_port, root, probe, identity):
    client = ModelJava(java_port, "ModelRestored")
    try:
        client.enter(java_port, root)
        assert client.spawn((3.0, -60.0, 4.0), 2.0) == identity
        result = subprocess.run([str(probe), f"http://127.0.0.1:{native_port}", "--model-snapshot"], capture_output=True, timeout=35, text=True)
        (root / "models-restarted-native.log").write_text(result.stdout + result.stderr)
        assert result.returncode == 0, result.stdout + result.stderr
    finally:
        client.socket.close()
