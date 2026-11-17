# Protocol fixtures

## Bedrock 2225 control packets

`bedrock_controls_2225.json` contains complete packet headers and payloads encoded
by gophertunnel `e2251a60f71d`, using its packet structs and writer. All subclient
bits are zero. It covers both stonecutter directions, matchmaking state and
cancellation, signed audio registration and playback, and block passengers.
The fixtures include nested optional fields, two audio registrations and all
trailing sound fields. Values are small test strings and the numeric values in
`tests/bedrock_controls.rs`; no service credentials or signed content is included.

These frames catch stale IDs as well as missing fields. The stonecutter request
regression fails with the old ID 332 against the independent header ID 354.

## Bedrock 2225 sound payloads

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

## Java custom-action replies

`java_custom_click_*_777.bin` contains packet payloads encoded by the official
Java 26.3 `ServerboundCustomClickActionPacket.STREAM_CODEC`:

| Fixture | Action | Encoded optional NBT |
| --- | --- | --- |
| `none` | `sculith:form/7/cancel` | TAG_End, length 1 |
| `small` | `sculith:input/7/submit` | Compound with `toggle_0 = true`, length 14 |
| `large` | `sculith:input/7/submit` | Compound with `text_0 = "A"` repeated 160 times, length 173 |

The large fixture requires a two-byte length prefix. The data retains the NBT
root tag. A boolean presence flag is not part of this packet. Bytecode inspection
confirmed the same length-prefixed optional-tag codec in Java 1.21.6, 1.21.11,
26.1, 26.2 and 26.3. The official server bundle SHA1 values were:

```
1.21.6   6e64dcabba3c01a7271b4fa6bd898483b794c59b
1.21.11  64bb6d763bed0a9f1d632ec347938594144943ed
26.1     3872a7f07a1a595e651aef8b058dfc2bb3772f46
26.2     823e2250d24b3ddac457a60c92a6a941943fcd6a
26.3     33680f5f2ac32864d6d7cf5e56a705fdb3e05f4c
```

The jars and generated Java class remain outside the repository. The wire
fixtures contain only the test action identifiers and test values above.
