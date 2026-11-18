"""Manual crafting against the advertised Bedrock recipes."""

import time

from inventory_requests import native


def check(server, port, root, probe):
    cases = [
        ("--craft-allow", "CraftReference", False, False),
        ("--craft-veto", "CraftVeto", True, False),
        ("--craft-prep-veto", "CraftPrepVeto", False, True),
        ("--craft-edit", "CraftEdit", False, False),
    ]
    server.stdin.write("defaultgamemode survival\n")
    server.stdin.flush()
    time.sleep(.2)
    try:
        for option, name, _, _ in cases:
            native(server, port, root, probe, option, name, [("oak_log", 2)])
        native(server, port, root, probe, "--craft-table", "CraftTable", [("oak_planks", 9)],
               setup=("setblock 3 -60 0 minecraft:crafting_table\n",))
    finally:
        server.stdin.write("defaultgamemode creative\n")
        server.stdin.flush()

    log = (root / "server.log").read_text()
    for _, name, veto, prepare_veto in cases:
        craft = f"PROBE_CRAFT player={name} id=oak_planks input=1 target=0 cancelled={str(veto).lower()}"
        prepare = f"PROBE_CRAFT_PREPARE player={name} id=oak_planks cancelled={str(prepare_veto).lower()}"
        assert log.count(craft) == 1 and log.count(prepare) == 1, name
        assert log.index(craft) < log.index(prepare), name
    return "manual 2x2 and table crafting, both Rust vetoes and retained callback edits"
