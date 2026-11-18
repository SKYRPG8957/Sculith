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


def destruction(server, java_port, native_port, root, probe):
    from inventory_requests import native

    for option, name in [("--destroy-deny", "EraseDenyNative"),
                         ("--destroy-mutate", "EraseMutNative"),
                         ("--destroy-allow", "EraseAllowNative")]:
        native(server, native_port, root, probe, option, name)
    for mode in ["Deny", "Mut", "Allow"]:
        name = f"Erase{mode}Java"
        client = PickJava(java_port, name)
        try:
            client.join_with_resources(java_port, root, False)
            client.send(SB["player_loaded"])
            server.stdin.write(f"give {name} minecraft:diamond_block 64\n")
            server.stdin.flush()
            original = {36: ("diamond_block", 64)}
            for _ in range(8192):
                client.event()
                if {i: value for i, value in client.contents.items() if value} == original:
                    break
            assert {i: value for i, value in client.contents.items() if value} == original, client.contents
            barrier(client, 711)
            client.send(SB["set_creative_mode_slot"], struct.pack(">h", 36) + b"\0")
            barrier(client, 712, correction=True)
            expected = original if mode == "Deny" else {36: ("grass_block", 7)} if mode == "Mut" else {}
            assert {i: value for i, value in client.contents.items() if value} == expected, client.contents
        finally:
            client.socket.close()
    rows = (root / "server.log").read_text().splitlines()
    for name, payloads in {
        "EraseDenyNative": [(44, "diamond_block", 63), (36, "air", 0), (-1, "air", 0)],
        "EraseAllowNative": [(36, "diamond_block", 61), (36, "air", 0), (-1, "air", 0)],
        "EraseMutNative": [(36, "air", 0)],
        "EraseDenyJava": [(36, "air", 0)],
        "EraseMutJava": [(36, "air", 0)],
        "EraseAllowJava": [(36, "air", 0)],
    }.items():
        events = [row for row in rows if "PROBE_CREATIVE" in row and f"player={name} " in row]
        assert len(events) == len(payloads), (name, events)
        for event, (slot, item, count) in zip(events, payloads):
            assert f"slot={slot} item={item} count={count} " in event, event
            assert f"cancelled={'true' if 'Deny' in name else 'false'}" in event, event
        if "Mut" in name:
            assert "before=64 screen_before=64" in events[0] and "mutated=true" in events[0], events
        trace = [row for row in rows if f"player={name} " in row and any(
            marker in row for marker in ["PROBE_INVENTORY_CLICK", "PROBE_INVENTORY_INTERACT", "PROBE_CREATIVE"])]
        if name.endswith("Native"):
            for i, row in enumerate(trace):
                if "PROBE_CREATIVE" in row:
                    assert i >= 2 and "PROBE_INVENTORY_CLICK" in trace[i - 2] and "PROBE_INVENTORY_INTERACT" in trace[i - 1], trace
        else:
            assert trace == events, trace


def check(server, java_port, native_port, root, probe):
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
    destruction(server, java_port, native_port, root, probe)
    return "both editions; creative creation and destruction veto/correction; native prefix swap, cursor and drop rejected; callback mutation retained"
