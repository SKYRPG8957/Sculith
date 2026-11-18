"""Inventory close restitution and authoritative cursor correction."""

import struct
import time

from creative_events import barrier
from inventory_requests import native
from pick_events import PickJava
from shared_resources import PACKETS, varint


def check(server, java_port, native_port, root, probe):
    server.stdin.write("defaultgamemode survival\n")
    server.stdin.flush()
    time.sleep(.2)
    try:
        native(server, native_port, root, probe, "--close-inventory", "CloseNative")
        native(server, native_port, root, probe, "--close-mutate", "CloseMutNative")
        client = PickJava(java_port, "CloseJava")
        try:
            client.join_with_resources(java_port, root, False)
            client.send(PACKETS["serverbound"]["play"]["player_loaded"])
            server.stdin.write("give CloseJava minecraft:diamond_block 64\n")
            server.stdin.flush()
            expected = {36: ("diamond_block", 64)}
            for _ in range(8192):
                client.event()
                if {i: value for i, value in client.contents.items() if value} == expected:
                    break
            assert {i: value for i, value in client.contents.items() if value} == expected, client.contents
            barrier(client, 831)
            client.send(PACKETS["serverbound"]["play"]["container_click"],
                        varint(0) + varint(0) + struct.pack(">hb", 36, 0) + varint(0) + varint(0) + b"\0")
            barrier(client, 832, correction=True)
            assert not {i: value for i, value in client.contents.items() if value}, client.contents
            client.send(PACKETS["serverbound"]["play"]["container_close"], varint(0))
            barrier(client, 833)
            for _ in range(8192):
                if {i: value for i, value in client.contents.items() if value} == expected:
                    break
                client.event()
            assert {i: value for i, value in client.contents.items() if value} == expected, client.contents
        finally:
            client.socket.close()
    finally:
        server.stdin.write("defaultgamemode creative\n")
        server.stdin.flush()
    rows = (root / "server.log").read_text().splitlines()
    for name in ["CloseNative", "CloseJava"]:
        events = [row for row in rows if "PROBE_INVENTORY_CLOSE" in row and f"player={name} " in row]
        assert events and "window=None before=64 screen=64 cursor=0 mutated=false" in events[0], (name, events)
    changed = [row for row in rows if "PROBE_INVENTORY_CLOSE player=CloseMutNative " in row and "mutated=true" in row]
    assert len(changed) == 1 and "window=None before=64 screen=64 cursor=0" in changed[0], changed
    return "both editions return cursor items on close; native UI-window correction retains callback inventory and cursor changes"
