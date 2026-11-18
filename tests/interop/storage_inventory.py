"""Ordinary native storage requests against a real chest in the copied world."""

import time

from inventory_requests import native


def check(server, port, root, probe, callbacks):
    server.stdin.write("defaultgamemode survival\n")
    server.stdin.flush()
    time.sleep(.2)
    try:
        native(server, port, root, probe, "--storage-inventory", "StoreNative", setup=(
            "setblock 3 -60 0 minecraft:chest\n",
            "item replace block 3 -60 0 container.0 with minecraft:diamond_block 64\n",
            "item replace block 3 -60 0 container.1 with minecraft:grass_block 64\n",
        ))
        if callbacks:
            # The next viewer must load the saved chunk, not reuse the live chest.
            deadline = time.monotonic() + 12
            while "Test passed" not in (root / "server.log").read_text():
                assert server.poll() is None and time.monotonic() < deadline, "storage chunk did not unload"
                server.stdin.write("execute unless loaded 3 -60 0\n")
                server.stdin.flush()
                time.sleep(.2)
            native(server, port, root, probe, "--storage-click-deny", "StoreClickDeny")
            native(server, port, root, probe, "--storage-drop-deny", "StoreDropDeny")
            native(server, port, root, probe, "--storage-mutate", "StoreMutNative")
    finally:
        server.stdin.write("defaultgamemode creative\n")
        server.stdin.flush()
    if callbacks:
        rows = (root / "server.log").read_text().splitlines()
        for name, veto, mutate in [("StoreClickDeny", True, False), ("StoreDropDeny", False, False),
                                   ("StoreMutNative", False, True)]:
            clicks = [row for row in rows if f"PROBE_INVENTORY_CLICK player={name} " in row]
            assert len(clicks) == 1, clicks
            assert "slot=0 raw=0 type=Unknown button=-1 window=Some(Generic9x3) clicked=64 cursor=0 before=64 screen_before=64" in clicks[0], clicks
            assert f"cancelled={str(veto).lower()} mutated={str(mutate).lower()}" in clicks[0], clicks
            interacts = [row for row in rows if f"PROBE_INVENTORY_INTERACT player={name} " in row]
            assert len(interacts) == (0 if veto else 1), interacts
        drops = [row for row in rows if "PROBE_INVENTORY_DROP" in row and "player=StoreDropDeny" in row]
        assert len(drops) == 1 and "count=1 cancelled=true" in drops[0], drops
    return "native chest transfers and invalid request readback; with callbacks, saved chunk reload, vetoes and mutation"
