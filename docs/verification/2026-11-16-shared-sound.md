# Shared sound verification — 2026-11-16 UTC

Scope: the current local offline preview, Java 26.3/protocol 777, native protocol
2225, one region owner. The user is away; no manual user retest is assumed.

The unmodified official Java game loads the generated required pack, enters and
renders the world, and plays the original custom tone requested by the actual
Rust sound example. OpenAL Soft's built-in Wave File Writer provides real client
PCM output without game mods or an attached sound device. The client JAR remains
SHA-1 `e877b6a07acd633fb3bb475002175cec036e7b87`.

The first capture is stereo PCM16 at 48 kHz. The strongest one-second window has
an FFT peak at **880 Hz**, with **99.8059%** of windowed spectral energy in
875–885 Hz. This demonstrates actual client pack/audio decoding and mixing; it
does not mean a human heard sound through speakers. A final exact-source capture
and its hashes will be recorded before publication.

`scripts/sound_smoke.py` passes against the built Linux executable and real Rust
plugin. Python independently checks the generated Java archive and wire packet;
Go/Pion receives the original native OGG and definition and decodes the latest
PlaySound layout. A Java mutation after native join verifies that the native
player's sound was not broadcast to Java. The test requires exact callback counts
and graceful plugin stopping. Unit tests reject unregistered sounds, malformed
identifiers, nonfinite/range-invalid parameters, pending/dead recipients, and rate
overflow, while preserving region revision and the game-hook stream.

Previously compiled v2 gameplay and v3 forms executables pass the new host; all
versions remain negotiated independently. Both texture delivery/cache and delayed
consent regressions pass. Native Windows execution/package checks are pending
for this candidate. Official Windows Bedrock GDK world entry and sound rendering
remain unverified: the available native test is an independent client, not the
official game.
