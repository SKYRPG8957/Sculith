# Shared static model verification — 2026-11-16 UTC

Scope: local offline Java 26.3/protocol 777 and independent native protocol 2225,
one region owner. The user is away; no official Bedrock retest is assumed.

Initial actual-game testing rendered the original three-cuboid statue using a
stock Item Display Entity. The first texture path was under `entity/`; Java 26.3
logged a missing sprite and displayed the missing-texture checkerboard. Inspection
of the checksum-pinned official client's `items.json` atlas confirms the `item/`
directory source. Moving the generated model texture there fixes the actual game:
the statue renders with the original green checkerboard and no missing-sprite log.
The game JAR SHA-1 remains `e877b6a07acd633fb3bb475002175cec036e7b87`.

The wire facts are checked against the pinned official report/client:
AddEntity 1; RemoveEntities 78; SetEntityData 101; item_display type 73; item_model
component 10; base metadata count eight; Display count fifteen; item metadata 23
with Optional ItemStack serializer 7; context byte 24. The new LP Vec3 velocity's
zero is a single byte, before rotation. Only facts and an independent Rust encoder
are shipped, without copied client implementation or Minecraft JAR/assets.

Independent Python/Go tests pass the required packs, exact original PNG/geometry,
native actor ID registration, show/replacement/hide and two-session privacy. The
native reader independently decodes current AddActor optional fields. Rust owner
tests preserve world revision and game hooks while rejecting invalid lease,
catalog, clock/rate and bounds requests. Conversion rejects unsupported rotation,
bone cycles, out-of-bounds UV and generated-file collisions.

Linux workspace tests, Clippy and ownership doctest pass. Retained actual v1/v2/v3/v4
Rust executables pass the new host, including old forms/sounds and pack/cache/consent
regressions. Final exact-source actual-game captures and native Windows release
verification are pending. Official Windows Bedrock-game entry/rendering remains
unverified; independent native tests are not labelled official-game verification.
