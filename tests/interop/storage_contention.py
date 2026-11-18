"""One chest item, two clients and one server tick."""

import re
import struct
import subprocess
import time

from creative_events import barrier
from pick_events import PickJava, stack
from shared_resources import PACKETS, varint

CB, SB = PACKETS["clientbound"]["play"], PACKETS["serverbound"]["play"]


class ChestJava(PickJava):
    def __init__(self, port):
        super().__init__(port, "RaceChestJava")
        self.window = None
        self.revision = 0
        self.chest = {}

    def event(self):
        id_, fields = super().event()
        fields.offset = 0
        assert fields.vi() == id_
        if id_ == CB["open_screen"]:
            self.window = fields.vi()
        elif id_ in [CB["container_set_content"], CB["container_set_slot"]]:
            window, revision = fields.vi(), fields.vi()
            if window == self.window:
                self.revision = revision
                if id_ == CB["container_set_content"]:
                    count = fields.vi()
                    assert count == 63, ("expected a single chest and player slots", count)
                    self.chest = {slot: stack(fields) for slot in range(count)}
                else:
                    slot, = struct.unpack(">h", fields.take(2))
                    self.chest[slot] = stack(fields)
        return id_, fields

    def withdraw(self):
        position = (3 << 38) | (-60 & 0xfff)
        self.send(SB["use_item_on"], b"\0" + struct.pack(">Q", position) + b"\1"
                  + struct.pack(">fff", .5, 1., .5) + b"\0\0\1")
        deadline = time.monotonic() + 6
        while time.monotonic() < deadline:
            self.event()
            if self.chest.get(0) == ("diamond_block", 1):
                break
        else:
            raise AssertionError(("Java did not see the seeded chest", self.window, self.chest))
        self.send(SB["container_click"], varint(self.window) + varint(self.revision)
                  + struct.pack(">hb", 0, 0) + b"\1\0\0")


def check(server, java_port, bedrock_port, root, probe, frozen):
    server.stdin.write("defaultgamemode survival\n")
    server.stdin.flush()
    time.sleep(.2)

    def join_java():
        client = ChestJava(java_port)
        client.join_with_resources(java_port, root, False)
        client.send(SB["player_loaded"])
        return client

    # Exercise both join orders across the running and frozen scenarios.
    java = None if frozen else join_java()
    try:
        log_path = root / "storage-contention-bedrock.log"
        with log_path.open("w") as log:
            native = subprocess.Popen([str(probe), f"http://127.0.0.1:{bedrock_port}", "--storage-race"],
                                      stdout=log, stderr=log)
            try:
                deadline = time.monotonic() + 20
                while "SPAWN " not in log_path.read_text():
                    assert native.poll() is None and time.monotonic() < deadline, log_path.read_text()
                    time.sleep(.05)
                if java is None:
                    java = join_java()
                server.stdin.writelines((
                    "setblock 3 -60 0 minecraft:chest\n",
                    "item replace block 3 -60 0 container.0 with minecraft:diamond_block 1\n",
                    "item replace block 3 -60 0 container.1 with minecraft:grass_block 64\n",
                    "give RaceChestBed minecraft:diamond_block 128\n",
                    "give RaceChestBed minecraft:grass_block 64\n",
                ))
                if frozen:
                    server.stdin.write("tick freeze\n")
                server.stdin.flush()
                seeded = "The game is frozen" if frozen else "Gave 64 [Grass Block] to RaceChestBed"
                deadline = time.monotonic() + 5
                while seeded not in (root / "server.log").read_text():
                    assert server.poll() is None and time.monotonic() < deadline, "chest seed did not finish"
                    time.sleep(.02)
                java.withdraw()
                assert native.wait(timeout=18) == 0, log_path.read_text()
                barrier(java, 815)
                assert java.chest.get(0) is None, java.chest
                java_items = [item for slot, item in java.chest.items() if slot >= 27 and item]
                assert all(item == ("diamond_block", 1) for item in java_items), java_items
                found = re.search(r"STORAGE_NATIVE acquired=([01])", log_path.read_text())
                assert found, log_path.read_text()
                bedrock_count = int(found[1])
                assert (root / "server.log").read_text().count("PROBE_STORAGE_CONTENTION") == 1
                assert len(java_items) + bedrock_count == 1, ("chest item count changed", len(java_items), bedrock_count)
            finally:
                if native.poll() is None:
                    native.kill()
                    native.wait(timeout=5)
    finally:
        if java is not None:
            java.socket.close()
        if frozen:
            server.stdin.write("tick unfreeze\n")
        server.stdin.write("defaultgamemode creative\n")
        server.stdin.flush()
    return f"Java shift-click and Bedrock withdrawal preserve one chest item with {'frozen' if frozen else 'normal'} world ticks"
