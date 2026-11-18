"""Item-use vetoes precede equipment changes and timed consumption."""

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
    names = []
    try:
        for option, name in [("--use-deny", "UseDenyNative"),
                             ("--use-input-deny", "UseDenyInput"),
                             ("--use-mutate", "UseMutNative"),
                             ("--use-allow", "UseAllowNative"),
                             ("--eat-deny", "EatDenyNative"),
                             ("--eat-allow", "EatAllowNative")]:
            item = "golden_apple" if name.startswith("Eat") else "leather_helmet"
            native(server, native_port, root, probe, option, name, seed=[(item, 1)])
            names.append(name)
        for prefix, mode in [("Use", "Deny"), ("Use", "Mut"), ("Use", "Allow"),
                             ("Eat", "Deny"), ("Eat", "Allow")]:
            name = f"{prefix}{mode}Java"
            item = "golden_apple" if prefix == "Eat" else "leather_helmet"
            client = PickJava(java_port, name)
            try:
                client.join_with_resources(java_port, root, False)
                client.send(PACKETS["serverbound"]["play"]["player_loaded"])
                server.stdin.write(f"give {name} minecraft:{item} 1\n")
                server.stdin.flush()
                original = {36: (item, 1)}
                for _ in range(8192):
                    client.event()
                    if {i: value for i, value in client.contents.items() if value} == original:
                        break
                assert {i: value for i, value in client.contents.items() if value} == original, client.contents
                barrier(client, 721)
                client.send(PACKETS["serverbound"]["play"]["use_item"],
                            varint(0) + varint(1) + struct.pack(">ff", 0, -90))
                if prefix == "Eat":
                    time.sleep(2.5)
                barrier(client, 722, correction=mode != "Allow")
                expected = {}
                if mode == "Deny":
                    expected = original
                elif mode == "Mut":
                    expected = {36: ("grass_block", 7)}
                elif prefix == "Use":
                    expected = {5: (item, 1)}
                assert {i: value for i, value in client.contents.items() if value} == expected, (name, client.contents)
                names.append(name)
            finally:
                client.socket.close()
    finally:
        server.stdin.write("defaultgamemode creative\n")
        server.stdin.flush()
    rows = (root / "server.log").read_text().splitlines()
    for name in names:
        events = [row for row in rows if "PROBE_ITEM_USE" in row and f"player={name} " in row]
        assert len(events) == 1 and "held=1 screen=1 head=0 active=0 air=true" in events[0], (name, events)
        assert f"cancelled={'true' if 'Deny' in name else 'false'}" in events[0], events
        assert f"mutated={'true' if 'Mut' in name else 'false'}" in events[0], events
    return "both editions; interaction veto before equipment and timed consumption; callback stack changes retained; allowed controls"
