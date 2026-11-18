"""Ordinary native stack requests against a console-seeded copied world."""

import subprocess
import struct
import time


def native(server, port, root, probe, option, name, seed=None, *, setup=()):
    path = root / f"{name}.log"
    server_log = root / "server.log"
    with path.open("w") as log:
        process = subprocess.Popen([str(probe), f"http://127.0.0.1:{port}", option],
                                   stdout=log, stderr=log)
        try:
            deadline = time.monotonic() + 20
            # The client spawn acknowledgement can arrive before the server's
            # join callbacks and initial inventory synchronization finish.
            while ("SPAWN " not in path.read_text()
                   or f"{name} joined the game" not in server_log.read_text()):
                assert process.poll() is None and time.monotonic() < deadline, path.read_text()
                time.sleep(.05)
            stacks = seed if seed is not None else [("diamond_block", 128), ("grass_block", 64)]
            server.stdin.writelines(setup)
            for item, count in stacks:
                server.stdin.write(f"give {name} minecraft:{item} {count}\n")
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
    server.stdin.write("defaultgamemode survival\n")
    server.stdin.flush()
    time.sleep(.2)
    try:
        for option, name in [("--inventory-events", "InvDenyNative"),
                             ("--inventory-mutate", "InvMutNative"),
                             ("--inventory-drop-event", "DropDenyNative")]:
            native(server, port, root, probe, option, name)
        java_swap_veto(server, java_port, root, "InvDenyJava")
    finally:
        server.stdin.write("defaultgamemode creative\n")
        server.stdin.flush()
    rows = (root / "server.log").read_text().splitlines()
    for name in ["InvDenyNative", "InvMutNative", "DropDenyNative", "InvDenyJava"]:
        calls = [line for line in rows if "PROBE_INVENTORY_INTERACT" in line and f"player={name} " in line]
        assert len(calls) == 1 and "before=64 screen_before=64" in calls[0], (name, calls)
    assert any("PROBE_INVENTORY_DROP before=64 screen_before=64 count=64 cancelled=true" in line for line in rows)
    return "both editions veto inventory interaction; native callback mutation and drop veto preserve inventory without held screen locks"


def legacy(server, port, root, probe, inventory, callbacks):
    server.stdin.write("defaultgamemode survival\n")
    server.stdin.flush()
    time.sleep(.2)
    try:
        if inventory:
            native(server, port, root, probe, "--legacy-inventory", "LegacyTxnNative")
        if callbacks:
            native(server, port, root, probe, "--legacy-drop-veto", "LegacyDropDeny")
            native(server, port, root, probe, "--legacy-held-veto", "LegacyHeldNative")
            rows = (root / "server.log").read_text().splitlines()
            assert any("PROBE_INVENTORY_DROP" in row and "count=1 cancelled=true player=LegacyDropDeny" in row for row in rows)
            assert any("PROBE_PICK_HELD player=LegacyHeldNative previous=0 next=1 current=0" in row and "cancelled=true" in row for row in rows)
        if inventory:
            assert "PROBE_INVENTORY_CLICK player=LegacyTxnNative " not in (root / "server.log").read_text()
    finally:
        server.stdin.write("defaultgamemode creative\n")
        server.stdin.flush()
    return "legacy slot-query/journal/held-item validation and verified drop; optional Rust drop/held-slot vetoes"


def java_swap_veto(server, port, root, name):
    from creative_events import barrier
    from pick_events import PickJava
    from shared_resources import PACKETS, varint

    client = PickJava(port, name)
    try:
        client.join_with_resources(port, root, False)
        server.stdin.write(f"give {name} minecraft:diamond_block 128\n")
        server.stdin.write(f"give {name} minecraft:grass_block 64\n")
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


def clicks(server, java_port, port, root, probe):
    server.stdin.write("defaultgamemode survival\n")
    server.stdin.flush()
    time.sleep(.2)
    try:
        native(server, port, root, probe, "--click-events", "ClickDenyNative")
        native(server, port, root, probe, "--click-mutation", "ClickMutNative")
        native(server, port, root, probe, "--click-cursor", "ClickCurNative")
        java_swap_veto(server, java_port, root, "ClickDenyJava")
    finally:
        server.stdin.write("defaultgamemode creative\n")
        server.stdin.flush()
    rows = (root / "server.log").read_text().splitlines()
    outside = [row for row in rows if "PROBE_INVENTORY_CLICK player=ClickCurNative " in row]
    assert len(outside) == 3, outside
    assert "slot=36 raw=36" in outside[0] and "clicked=64 cursor=0 before=64" in outside[0], outside
    assert "slot=-999 raw=-999 type=Unknown button=-1 window=None clicked=0 cursor=1 before=63" in outside[1] and "cancelled=true" in outside[1], outside
    assert "slot=36 raw=36" in outside[2] and "clicked=63 cursor=1 before=63" in outside[2], outside
    for name in ["ClickDenyNative", "ClickMutNative", "ClickDenyJava"]:
        events = [row for row in rows if "PROBE_INVENTORY_CLICK" in row and f"player={name} " in row]
        assert len(events) == 1 and "clicked=64 cursor=0 before=64 screen_before=64" in events[0], (name, events)
        gesture = "type=NumberKey(8) button=8" if name.endswith("Java") else "type=Unknown button=-1"
        assert f"slot=36 raw=36 {gesture} window=None" in events[0], events
        later = [row for row in rows if "PROBE_INVENTORY_INTERACT" in row and f"player={name} " in row]
        if "Deny" in name:
            assert "cancelled=true" in events[0] and not later, (events, later)
        else:
            assert "mutated=true" in events[0] and len(later) == 1 and "before=7 screen_before=7" in later[0], (events, later)
    return "both editions receive click snapshots and veto; native gesture Unknown; callback mutations retained and click precedes interaction"
