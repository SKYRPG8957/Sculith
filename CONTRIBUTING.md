# Contributing to Sculith

Sculith is in early development. Working, tested increments are welcome; the [current status](docs/development/status.md) distinguishes implemented features from planned work.

## Before changing code

- Read [AGENTS.md](AGENTS.md), the [architecture](docs/internals/architecture.md), and the [roadmap](docs/development/roadmap.md).
- Keep one authoritative Bedrock-native world. Protocol adapters translate validated inputs and committed outputs; they do not run separate simulations.
- Preserve region ownership, bounded queues, byte budgets, input validation and authentication.
- Keep version-specific packet and registry facts separate from internal world IDs.
- Check the license before using external code. Do not copy an incompatible implementation or redistribute Minecraft assets.
- Discuss large architectural changes in an issue before investing in them.

## Local validation

Use the pinned Rust toolchain and checked-in lockfile:

```sh
python3 scripts/check_architecture.py
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
cargo test --workspace --doc --locked
cargo build --workspace --locked
python3 tests/interop/smoke.py
python3 tests/interop/smoke.py --sessions
```

The cloud workspace may need `. ./scripts/dev-env.sh` to activate its existing Rust installation. A normal Rust installation does not need the cloud-specific path.

## Pull requests

Describe the concrete behavior changed, the relevant edition/version, the tests executed, and any remaining limitations. Keep patches focused and preserve existing user/world data.

For protocol work, cite versioned sources and add codec and state-transition coverage. For world changes, verify ownership, revisions, persistence and negative coordinates. For performance work, provide a reproducible benchmark and equivalent comparison conditions.

Distinguish these forms of evidence:

1. Unit and codec tests.
2. Headless protocol clients and real socket tests.
3. Official Minecraft clients, including rendering and gameplay.

Do not claim support for an edition, version or content feature from packet IDs or a passing status ping alone. Record skipped, failing and unrun checks honestly.

## Bug reports

Include the Sculith commit, OS/toolchain, Minecraft edition/version, minimal reproduction and relevant sanitized logs. Remove authentication tokens, signed login payloads, credentials and private addresses before posting logs. Reports can be filed in [GitHub Issues](https://github.com/SKYRPG8957/Sculith/issues).

Do not post a working exploit or sensitive credentials in a public issue. If a private reporting channel is unavailable, open a minimal request to arrange one without disclosing the vulnerability details.
