# Region checkpoints

Configured region files checkpoint changed revisions every 60 seconds by default.
The interval accepts 1–86400 seconds; 0 retains final-only saving. No snapshot
path means no persistence worker I/O. A loaded revision is already considered
saved; merely loading flat chunks does not trigger a rewrite.

## Ordering and failure handling

One worker requests a bounded immutable capture through the existing region
mailbox. It encodes and atomically saves that capture on a blocking I/O thread,
then waits for the next timer. There is no write queue or concurrent replacement.
The owner can commit new edits while the captured state is being written.
Shutdown signals the worker, joins an active write, drains network sessions and
captures the final committed state. A periodic failure stops the server and
attempts a final save. Both error causes survive if the final attempt also fails.

The existing version/checksum, forward-revision and destination checks remain
unchanged. Corrupt, foreign, read-only and symlink destinations are not repaired
or overwritten automatically.

## Verification

Runtime source `5a24c89410ad2ca0dfdd8e3221f905f5d37e6723` completes
[Linux run 43949705160](https://github.com/SKYRPG8957/Sculith/actions/runs/43949705160).
The native [Windows run 43949705866](https://github.com/SKYRPG8957/Sculith/actions/runs/43949705866)
passes every configured build, runtime and interoperability step, including the
forced-termination/recovery probe against its static-CRT release executable.
Only cache/checkout cleanup remains at this observation. This is actual Windows
process/filesystem evidence with independent protocol clients, not official
Windows GDK rendering or power-loss testing.

Local checks pass **235 project tests**, with two release benchmarks ignored,
Clippy, ownership doctests and the eight-crate architecture check. Unit checks
exercise live checkpoints, skip unchanged revisions, preserve a foreign file on
failure and retain final saving when periodic writes are disabled.

`tests/interop/autosave_smoke.py` runs actual server and Rust fixture processes:

1. Save an initial live region, commit a two-chunk plugin edit and wait for the
   post-sync save notification. Kill the server without its shutdown hooks.
2. Restart from that file; an independent Java decoder verifies the changed
   chunks, heightmaps and skylight. The private world read returns revision 1 and
   the expected Stone/Dirt/Air state.
3. Disable periodic saving, remove the two blocks and verify the file stays
   unchanged while the server runs. Orderly shutdown persists revision 2; another
   restart recovers that revision and empty positions.
4. A loaded unchanged revision produces no save notification or file change.
5. Replace a temporary test world's destination with an unrelated file. The next
   changed checkpoint exits nonzero, preserves that file and reports both the
   autosave and final-save failures.

A separate local release build passes the same forced-termination probe using
the release server and Rust fixture. A Linux OS-lock experiment holds the storage
sidecar while the owner still accepts a world read and a newer edit. Shutdown
waits until that lock is released: checkpoint revision 1 completes before the
final revision 2, and another process restores revision 2. This lock experiment
is separate from the portable CI fixture and is not Windows evidence.

Existing world-API cross-edition checks, listener/plugin/persistence checks in
both transport modes and independent Java gameplay/reconnect checks also pass.
Independent Go/Pion and aiortc/OpenSSL probes retain initialized native entry,
transport-bound login and startup/chunk decoding.
All worlds are temporary; no gameplay command or test plugin enters production
defaults. Linux and Windows CI run the new forced-termination check; neither
workflow publishes packages or artifacts.

## Local performance sample and limits

The existing release `world_bench 1000` probe measures 1,000 encodes of a
1,193-byte modified region in **14.810168 ms**, 1,000 validation/decodes in
**18.372249 ms**, and 20 atomic save/reload operations in **17.357416 ms**. The disk
sample includes owner commits, capture, sync and reload on this cloud filesystem.
It is not a full-region, server-TPS or Windows disk benchmark, and is not an
optimization comparison.

A forced process restart recovers only a completed snapshot. Later edits and
player state may be lost. Slow storage or scheduling can extend the checkpoint
window. No write-ahead log, power-loss experiment, corruption repair, multi-server
world lease or coordinated multi-region checkpoint is implemented. Official-game
rendering and real-account authentication are separate evidence.
