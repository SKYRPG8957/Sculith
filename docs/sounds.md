# Shared custom sounds

The current source converts explicitly mapped single-clip Bedrock sound events
and exposes private playback through Rust plugin API v4. The two editions receive
the same authored OGG bytes. This is a bounded sound-effect subset, without
weighted variants, looping, streaming, global broadcast or arbitrary positions.

```sh
cargo build --workspace --locked
cargo run --locked -- run --config examples/sounds-preview.toml
```

Public [Windows Preview 14](https://github.com/SKYRPG8957/Sculith/releases/tag/v0.1.0-preview.14)
bundles the sound executable, gameplay/forms examples and original pack. New
settings enable all three; existing settings are preserved.

Use Java 26.3 at `127.0.0.1:25565`, or the native Bedrock preview at
`127.0.0.1:19132`. The sound example plays a short original tone after player
initialization. Accept/load the required pack first. The sample configuration
uses local offline identities. Append `.exe` to the plugin path on Windows.

## Author once

`sounds/sound_definitions.json` in the authored Bedrock resource pack:

```json
{
  "format_version": "1.14.0",
  "sound_definitions": {
    "sculith.demo_tone": {
      "category": "player",
      "sounds": ["sounds/sculith_demo"]
    }
  }
}
```

Put the clip at `sounds/sculith_demo.ogg`. Extend the existing mapping JSON:

```json
{
  "textures": [],
  "sounds": [{
    "bedrock": "sculith.demo_tone",
    "java": "sculith:demo_tone",
    "file": "sounds/sculith_demo.ogg"
  }]
}
```

The canonical plugin name is the `java` resource identifier. The native adapter
resolves it through the frozen mapping; plugins never supply native packet names.
The converter writes a Java `sounds.json` event and its exact OGG clip under that
namespace. Native files retain their original definition. Changing assets changes
the generated pack/cache identities. The authored manifest stays untouched.

Only a definition exactly matching the shown category/single-clip structure is
accepted for a selected event. Extra weighting, pitch, volume, range and streaming
properties cause conversion failure, avoiding a silent change in sound semantics.
Unmapped definitions are not converted and are not registered for plugin playback.
Omitting `sounds` preserves existing texture-only mapping input compatibility.

## Rust API

```rust
Action::PlaySound {
    player_id: player.id,
    sound: "sculith:demo_tone".into(),
    volume: 0.7,
    pitch: 1.0,
}
```

The world owner checks the live initialized session, registered sound, finite
volume `0..1` and pitch `0.5..2`. Each player has a four-token burst with four
tokens replenished per second. The owner chooses the current eye position;
neither a client nor a plugin supplies coordinates. Both protocols quantize that
position to eighth-block coordinates, use the player category, and request one
playback without looping. Delivery targets only the selected session. Presentation
does not change the region revision or re-enter plugin game hooks.

The current SDK negotiates versions 1 through 5; sound playback requires v4. The host rejects a sound action on an
older negotiated API. Trusted executable plugins retain the server user's OS
permissions; this API does not provide a sandbox.

## Bounds and verification

At most 64 uniquely mapped sounds are accepted. Canonical names have at most 128
ASCII bytes and reject empty/dot/traversal path segments. Clips must contain one
Ogg Vorbis stream, one/two channels, `8000..48000` Hz and at most 30 seconds of
source samples. Existing per-file/source/archive/conversion byte bounds apply.
The converter checks Ogg framing, serial/sequence/continuation, page CRCs, the
Vorbis identification header and terminal granule count. It does not decode all
Vorbis setup/audio packets; the actual game decode is verified separately.

```sh
python3 scripts/sound_smoke.py --client target/gophertunnel-probe
```

Independent Python and Go/Pion clients check both downloaded packs, exact clip
bytes, event names, fixed-point positions, parameters, optional-field layout,
non-looping playback and recipient isolation. The Rust example also completes its
shutdown hook. These wire tests do not establish official Bedrock-game playback.

The example clip is an original 880 Hz, 0.75-second mono tone generated with
FFmpeg's sine source and Vorbis encoder, without proprietary game audio. Its
SHA-256 is `8e15745a94e04437cb78f5ae39fb2a6dda7390baae40079f91c2015effd8001f`.
FFmpeg is a development asset-generation tool, not a runtime dependency.

Actual stock Java PCM-output evidence and exact artifacts are recorded in
[the verification record](verification/2026-11-16-shared-sound.md). Full models,
animations and arbitrary Bedrock JSON UI remain pending.
