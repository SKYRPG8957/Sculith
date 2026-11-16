# Bedrock 1.26.63.1 reference

The official download service selected the stable Linux dedicated server, not a
preview. The archive and executable remain outside the product repository.

- [Official archive](https://www.minecraft.net/bedrockdedicatedserver/bin-linux/bedrock-server-1.26.63.1.zip), 98,092,352 bytes; SHA-256 `b40b04c8f5a7c42a19ee685c22fc593e66116b9c22741b8c75489306b252ff62`.
- Executable: 265,503,224 bytes; SHA-256 `dc9b3c566a014cf53930eb1b54c6b599a99a81cc32c68aba06a5d73a0fd63179`.
- Startup reports build 53340802, branch `r/26_u6`, commit `2fcf3da2f65401b298466e1f5953182d548b2de7`.

## Observed execution

REA 6.9.0 `capture_process_scenario` launched that executable on loopback with
NetherNet, a separate reference world, Creative mode and local offline identities.
A pinned gophertunnel client (e2251a60) joined with protocol 2225. Its independent
packet callback decoded inventory traffic; those packet observations are separate
from REA's terminal/process capture.

After the player spawned, the capture submitted `give SculithRef stone 5`, then
`give SculithRef stone 3`, then stopped the reference server. REA Evidence
`ev_f8db21a632808629c20b8632c497afa399c0d4cae735b3449d2bb2fc48233d3b`
records both successful commands, exit 0, no output truncation and verified owned
process cleanup. A matching executable digest is a prelaunch file sample, not
loaded-image attestation or proof that a particular internal function ran.

| Static window | Slots | Fresh contents |
| --- | ---: | --- |
| Main inventory, 0 | 36 | Empty |
| Armour, 120 | 5 | Empty |
| UI, 124 | 57 | Empty |
| Offhand, 119 | 1 | Empty |

The content packets used zero-valued full-container metadata without a dynamic
ID. The first grant produced Stone 5 in slot 0 with stack ID 2; the second
produced Stone 8 in the same slot with stack ID 4. This establishes the observed
grant/merge path and changing positive stack IDs, not every transaction rule.

Earlier trials are retained: connecting before server readiness received an
identityless SDP answer that the client rejected; another grant ran before the
player spawned and matched no target. Those trials do not establish item delivery.
The final capture waits through readiness and player initialization before grants.

## Implementation and limits

Sculith now initializes the auxiliary static windows before loading chunks. UI
slots 0 and 50 project the owner's cursor and created output; unsupported cells
start empty. This does not add armour, offhand or general crafting actions.
The independent Go startup observer checks these three windows on every ordinary
probe; a Rust regression checks canonical cursor/output projection.

Local validation passed 201 workspace tests, the ownership compile-fail doctest,
Clippy and the architecture check. Independent probes passed native spawn,
inventory/grants, peers/equipment, plugin observations, forms, sounds, models,
pack transfer, Java gameplay and WebRTC. The session probe first caught stale
NetherNet version advertising; it passed after using the bundled patch target
for discovery while retaining protocol 2225.

The build and interoperability checks also passed for commit `a69a64e` on
[Linux](https://github.com/SKYRPG8957/Sculith/actions/runs/43899920944) and
[Windows](https://github.com/SKYRPG8957/Sculith/actions/runs/43899920837), including
the Windows release executable with a static CRT. These are independent protocol
clients, not verification with the official Windows game.

The prototype main-inventory starter stacks have since been removed. New
production sessions start empty. Integration clients explicitly create their
fixture items through creative inventory requests; no engine kit command was
added. See [current validation](../status.md).
No Minecraft executable, pack, registry asset or decompiled implementation is
included in Sculith.

REA's ELF export inspection finds zero exports in 389 dynamic entries. Full
Ghidra 12.2 startup/analysis exceeded both 15- and 30-minute provider budgets;
partial diagnostics were preserved and cleanup completed. No function-level
decompilation or string cross-reference result is claimed from those attempts.
The usable REA result here is the completed native process capture, combined
with separately identified independent wire observations. Official Windows GDK
rendering and online-account entry remain separate verification work.
