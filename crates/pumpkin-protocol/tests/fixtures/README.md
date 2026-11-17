# Bedrock 2225 sound payloads

The sound fixtures were encoded independently with gophertunnel
`e2251a60f71d` (the version pinned in `tests/interop/bedrock/go.mod`). They contain
payload bytes without a packet ID.

- `play_sound_2225.bin`: `sculith.demo_tone`, position `(-1.25,-60.5,3.875)`,
  volume `2.5`, pitch `0.75`, loop count `-1`, range bypass false, handle `42`,
  playback offset `1.25` seconds.
- `stop_sound_2225.bin`: `sculith.demo_tone`, stop-all false, legacy-music false.

The position is scaled by eight and each component uses signed VarInt encoding.
The two optional fields include their presence bytes. Testing against a separate
encoder catches symmetric mistakes in a Rust-only round trip.
