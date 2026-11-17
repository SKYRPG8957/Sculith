"""Ordinary native stack requests against a console-seeded copied world."""

import subprocess
import struct
import time


def native(server, port, root, probe, option, name):
    path = root / f"{name}.log"
    with path.open("w") as log:
        process = subprocess.Popen([str(probe), f"http://127.0.0.1:{port}", option],
                                   stdout=log, stderr=log)
        try:
            deadline = time.monotonic() + 20
            while "SPAWN " not in path.read_text():
                assert process.poll() is None and time.monotonic() < deadline, path.read_text()
                time.sleep(.05)
            server.stdin.write(f"give {name} minecraft:diamond_block 128\n")
            server.stdin.write(f"give {name} minecraft:grass_block 64\n")
            server.stdin.flush()
            assert process.wait(timeout=30) == 0, path.read_text()
            server.stdin.write("kill @e[type=minecraft:item]\n")
            server.stdin.flush()
        finally:
            if process.poll() is None:
                process.kill()
                process.wait(timeout=5)


def check(server, port, root, probe):
    try:
        for mode, option, name in [("survival", "--inventory-requests", "InvSurvival"),
                                   ("creative", "--inventory-creative", "InvCreative"),
                                   ("spectator", "--inventory-spectator", "InvSpectator")]:
            server.stdin.write(f"defaultgamemode {mode}\n")
            server.stdin.flush()
            time.sleep(.2)
            native(server, port, root, probe, option, name)
    finally:
        server.stdin.write("defaultgamemode creative\n")
        server.stdin.flush()
    return "ordinary inventory bounds, atomic rejection, cursor, drops and spectator permission"


def events(server, java_port, port, root, probe):
    from creative_events import barrier
    from pick_events import PickJava
    from shared_resources import PACKETS, varint

    server.stdin.write("defaultgamemode survival\n")
    server.stdin.flush()
    time.sleep(.2)
    try:
        for option, name in [("--inventory-events", "InvDenyNative"),
                             ("--inventory-mutate", "InvMutNative"),
                             ("--inventory-drop-event", "DropDenyNative")]:
            native(server, port, root, probe, option, name)
        client = PickJava(java_port, "InvDenyJava")
        try:
            client.join_with_resources(java_port, root, False)
            server.stdin.write("give InvDenyJava minecraft:diamond_block 128\n")
            server.stdin.write("give InvDenyJava minecraft:grass_block 64\n")
            server.stdin.flush()
            expected = {36: ("diamond_block", 64), 37: ("diamond_block", 64), 38: ("grass_block", 64)}
            for _ in range(8192):
                client.event()
                if {i: value for i, value in client.contents.items() if value} == expected:
                    break
            assert {i: value for i, value in client.contents.items() if value} == expected, client.contents
            barrier(client, 801)
            client.send(PACKETS["serverbound"]["play"]["container_click"],
                        varint(0) + varint(0) + struct.pack(">hb", 36, 8) + varint(2) + varint(0) + b"\0")
            barrier(client, 802, correction=True)
            assert {i: value for i, value in client.contents.items() if value} == expected, client.contents
        finally:
            client.socket.close()
    finally:
        server.stdin.write("defaultgamemode creative\n")
        server.stdin.flush()
    rows = (root / "server.log").read_text().splitlines()
    for name in ["InvDenyNative", "InvMutNative", "DropDenyNative", "InvDenyJava"]:
        calls = [line for line in rows if "PROBE_INVENTORY_INTERACT" in line and f"player={name} " in line]
        assert len(calls) == 1 and "before=64 screen_before=64" in calls[0], (name, calls)
    assert any("PROBE_INVENTORY_DROP before=64 screen_before=64 count=64 cancelled=true" in line for line in rows)
    return "both editions veto inventory interaction; native callback mutation and drop veto preserve inventory without held screen locks"
