"""Cursor distribution and cancellation through the native Rust event."""

import time

from inventory_requests import native


def check(server, port, root, probe):
    cases = [
        ("--drag-allow", "DragNativeAllow", "cancelled=false edited=false"),
        ("--drag-veto", "DragNativeVeto", "cancelled=true edited=false"),
        ("--drag-edit", "DragNativeEdit", "cancelled=false edited=true"),
    ]
    server.stdin.write("defaultgamemode survival\n")
    server.stdin.flush()
    time.sleep(.2)
    try:
        for option, name, _ in cases:
            native(server, port, root, probe, option, name)
    finally:
        server.stdin.write("defaultgamemode creative\n")
        server.stdin.flush()

    rows = (root / "server.log").read_text().splitlines()
    for _, name, decision in cases:
        events = [row for row in rows if f"PROBE_CURSOR_DRAG player={name} " in row]
        assert len(events) == 1 and f"cursor=6 target=0 {decision}" in events[0], (name, events)
    return "native cursor distribution, veto, stale-plan rejection after plugin edits, stop and close reset"
