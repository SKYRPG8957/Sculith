"""Pick selection through a native Rust cancellation and inventory-mutation callback."""

import json
import struct
import subprocess

from shared_resources import Java, PACKETS, ROOT, varint

CB, SB = PACKETS["clientbound"]["play"], PACKETS["serverbound"]["play"]
ITEMS = {entry["id"]: name for name, entry in json.loads((ROOT / "assets/items.json").read_text()).items()}
PIG = json.loads((ROOT / "assets/entities.json").read_text())["pig"]["id"]


def stack(fields):
    count = fields.vi()
    if count == 0:
        return None
    item = fields.vi()
    assert fields.vi() == 0 and fields.vi() == 0, "unexpected item component patch"
    return ITEMS[item], count


class PickJava(Java):
    def __init__(self, port, name):
        super().__init__(port, name)
        self.contents = {}
        self.near = None
        self.initial_slot = False
        self.selected = None

    def event(self):
        id_, fields = self.receive()
        if id_ == CB["keep_alive"]:
            self.send(SB["keep_alive"], fields.take(8))
        elif id_ == CB["chunk_batch_finished"]:
            self.send(SB["chunk_batch_received"], struct.pack(">f", 8.0))
        elif id_ == CB["disconnect"]:
            raise AssertionError(("pick client disconnected", fields.rest()))
        elif id_ == CB["set_held_slot"]:
            self.selected = fields.vi()
            self.initial_slot = self.selected == 0 or self.initial_slot
        elif id_ == CB["add_entity"]:
            entity_id = fields.vi()
            fields.take(16)
            type_ = fields.vi()
            x, _, _ = struct.unpack(">ddd", fields.take(24))
            if type_ == PIG and x < 6:
                self.near = entity_id
        elif id_ == CB["container_set_content"]:
            window, _ = fields.vi(), fields.vi()
            if window == 0:
                count = fields.vi()
                assert count == 46, ("unexpected player inventory size", count)
                values = [stack(fields) for _ in range(count)]
                self.contents = {i: value for i, value in enumerate(values) if value}
        elif id_ == CB["container_set_slot"]:
            window, _ = fields.vi(), fields.vi()
            if window == 0:
                slot, = struct.unpack(">h", fields.take(2))
                self.contents[slot] = stack(fields)
        elif id_ == CB["set_player_inventory"]:
            slot = fields.vi()
            self.contents[slot + 36 if slot < 9 else slot] = stack(fields)
        return id_, fields

    def wait_state(self, selected, count):
        selection = False
        expected = {36: ("diamond_block", count)}
        for _ in range(8192):
            id_, fields = self.event()
            if id_ == CB["set_held_slot"]:
                actual = self.selected
                assert actual == selected, ("pick event changed held slot", actual, selected)
                selection = True
            contents = {slot: value for slot, value in self.contents.items() if value}
            assert all(value[0] == "diamond_block" for value in contents.values()), contents
            if selection and contents == expected:
                return
        raise AssertionError("missing Java pick correction")

    def pick_block(self, position):
        x, y, z = position
        packed = ((x & 0x3ffffff) << 38) | ((z & 0x3ffffff) << 12) | (y & 0xfff)
        self.send(SB["pick_item_from_block"], struct.pack(">Q", packed) + b"\0")


def check(java_port, native_port, root, probe):
    for mutation, name in [(False, "HoldJava"), (True, "MutateJava")]:
        client = PickJava(java_port, name)
        try:
            client.join_with_resources(java_port, root, False)
            client.send(SB["player_loaded"])
            for _ in range(8192):
                client.event()
                if client.near is not None and client.initial_slot:
                    break
            assert client.near is not None and client.initial_slot, "missing Java initial state"
            if not mutation:
                client.pick_block((2, -60, 2))
                client.wait_state(0, 1)
            client.send(SB["set_carried_item"], struct.pack(">h", 5))
            if mutation:
                client.pick_block((0, -61, 2))
                client.wait_state(5, 7)
            else:
                for position in [(2, -60, 2), (0, -61, 2)]:
                    client.pick_block(position)
                    client.wait_state(5, 1)
                client.send(SB["pick_item_from_entity"], varint(client.near) + b"\0")
                client.wait_state(5, 1)
        finally:
            client.socket.close()
        mode = "--pick-mutation" if mutation else "--pick-events"
        result = subprocess.run([str(probe), f"http://127.0.0.1:{native_port}", mode],
                                capture_output=True, text=True, timeout=35)
        (root / f"{name}-native.log").write_text(result.stdout + result.stderr)
        assert result.returncode == 0, result.stdout + result.stderr
    rows = (root / "server.log").read_text().splitlines()
    for name in ["HoldJava", "HoldNative", "MutateJava", "MutateNative"]:
        events = [row for row in rows if "PROBE_PICK_HELD" in row and f"player={name} " in row]
        assert any("previous=0 next=5 current=0" in row for row in events), (name, events)
        if name.startswith("Hold"):
            cancelled = [row for row in events if "cancelled=true" in row]
            assert len(cancelled) == 3, (name, events)
            assert all("previous=5" in row and "current=5" in row for row in cancelled)
        else:
            assert sum("previous=5 next=0 current=5 count0=0 cancelled=false mutated=true" in row
                       for row in events) == 1, (name, events)
    return "both editions; cancelled new/reused/entity picks; callback inventory mutation retained"
