# Resource dispatch plugin fixture

This Rust Wasm plugin registers join and packet events. On join it plays the same
canonical shared sound and stops that sound by name. It also broadcasts a sound
through the world API; the harness checks that an existing Java peer hears the
Bedrock join's broadcast. It tests nested packet callbacks while a plugin event
is active. It is never installed by default.

```sh
rustup target add wasm32-wasip2
cargo build --locked --release --target wasm32-wasip2 \
  --manifest-path tests/interop/plugin/Cargo.toml
python3 tests/interop/shared_resources.py /path/to/sculith /path/to/bedrock-probe \
  --world /path/to/stopped-flat-world --delayed --building \
  --plugin tests/interop/plugin/target/wasm32-wasip2/release/sculith_wasm_audio_probe.wasm
```

The harness installs the fixture only into its scratch directory and explicitly
allows that unsigned plugin there. It checks real Java and NetherNet packet
transfers and preserves normal server plugin policies. Immediate play/stop is a
dispatch check, not an audible-playback test. Use an optimized Wasm build; this
fixture's debug build was rejected by the Wasmtime compiler and is not validated.
