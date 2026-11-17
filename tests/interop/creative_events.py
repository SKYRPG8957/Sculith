"""Creative inventory cancellation and callback mutation on both editions."""

import json
import struct
import subprocess

from pick_events import PickJava
from shared_resources import PACKETS, ROOT, varint

CB, SB = PACKETS["clientbound"]["play"], PACKETS["serverbound"]["play"]
DIAMOND = json.loads((ROOT / "assets/items.json").read_text())["diamond_block"]["id"]


def barrier(client, nonce, correction=False):
    client.send(SB["ping_request"], struct.pack(">q", nonce))
    corrected = False
    for _ in range(8192):
        id_, fields = client.event()
        if id_ in [CB["container_set_content"], CB["container_set_slot"], CB["set_player_inventory"]]:
            corrected = True
        if id_ == CB["pong_response"] and fields.take(8) == struct.pack(">q", nonce):
            assert not correction or corrected, "cancelled Java creative prediction was not corrected"
            return
    raise AssertionError("missing creative Java ordering barrier")


def check(java_port, native_port, root, probe):
    for mutation, name in [(False, "CreateDenyJava"), (True, "CreateMutJava")]:
        # Check native cancellation first: the old native path never fires this event.
        option = "--creative-mutate" if mutation else "--creative-deny"
        result = subprocess.run([str(probe), f"http://127.0.0.1:{native_port}", option],
                                capture_output=True, text=True, timeout=35)
        (root / f"{name}-native.log").write_text(result.stdout + result.stderr)
        assert result.returncode == 0, result.stdout + result.stderr
        client = PickJava(java_port, name)
        try:
            client.join_with_resources(java_port, root, False)
            client.send(SB["player_loaded"])
            if not mutation:
                client.pick_block((2, -60, 2))
                client.wait_state(0, 1)
            barrier(client, 701)
            item = varint(64) + varint(DIAMOND) + b"\0\0"
            client.send(SB["set_creative_mode_slot"], struct.pack(">h", 36) + item)
            barrier(client, 702, correction=True)
            expected = {36: ("grass_block", 7)} if mutation else {36: ("diamond_block", 1)}
            assert {slot: value for slot, value in client.contents.items() if value} == expected, client.contents
        finally:
            client.socket.close()
    rows = (root / "server.log").read_text().splitlines()
    for name in ["CreateDenyNative", "CreateMutNative", "CreateDenyJava", "CreateMutJava"]:
        events = [row for row in rows if "PROBE_CREATIVE" in row and f"player={name} " in row]
        assert events, name
        assert all("count=64" in row or "count=1" in row for row in events), events
        if "Mut" in name:
            assert len(events) == 1 and "before=0 screen_before=0" in events[0] and "mutated=true" in events[0], events
        else:
            assert all("before=1 screen_before=1" in row for row in events), events
            assert any("cancelled=true" in row for row in events), events
    return "both editions; creative veto/correction; native prefix swap, cursor and drop rejected; callback mutation retained"
