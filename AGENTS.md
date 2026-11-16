# Sculith development

Read `docs/development/engineering-directive.md` for the user's Master Engineering Directive v1.0.
It is the project direction; implementation and verification evidence determine actual support.

- Preserve existing work. Inspect branch, worktree and changes before editing; one owner per file during parallel work.
- Keep the existing MIT LICENSE. Do not copy incompatible copyleft implementation code or change licenses without authorization.
- The canonical repository is `SKYRPG8957/Sculith`. Do not silently rewrite remotes, push, publish or create releases.
- User instruction (2026-11-16): do not publish preview packages, create preview release tags, or upload build artifacts. Removed previews must stay removed. Keep CI focused on building and testing; do not restore publication workflows or the preview installer.
- One Bedrock-native authoritative world, exposed through edition-specific adapters. Never create separate Java and Bedrock simulations.
- Sculith targets a broadly usable public production server, with 에스테리아 일대기 as its primary real-use server. Keep that server's RPG rules/content in Rust plugins and packs; do not specialize the engine around its game design.
- Keep gameplay commands, kits and reward rules out of the engine. Integration-only plugins belong in `tests/fixtures`; normal builds and default configurations must not include or register test/gameplay commands.
- Keep operator-facing text brief and practical. Put setup and API guides under `docs/guides`, internals under `docs/internals`, and development/verification history under `docs/development`.
- Keep dependency direction explicit: core/world → game → runtime → plugin-host/server. Protocol adapters stay outside game decisions, and example plugins depend on the SDK. Run `python3 scripts/check_architecture.py` after changing crate boundaries.
- Consider region ownership, bounded queues, byte budgets and committed event boundaries from the start. A single region is the initial correctness target.
- Keep latest stable Bedrock distinct from previews. Java starts with one near-latest stable version. Protocol reference numbers are not proof of gameplay support.
- Validate RakNet and NetherNet independently. Do not label RakNet as NetherNet compatibility.
- Java Vanilla mode uses stock client capabilities and reports unsupported assets; optional Fabric Companion mode targets higher fidelity. Do not claim arbitrary Bedrock JSON UI or Script API works in vanilla Java.
- Official Rust plugin architecture should evaluate WASI Component Model with measured execution limits. The current trusted executable JSON-lines plugin is an initial prototype, not a sandbox.
- Prioritize actual Bedrock login/spawn, Java same-world crossplay, game/plugin behavior, content fidelity, then parallel simulation/optimization. Add crates only for real implementations.
- Use stable pinned Rust, preserve input/auth/permission checks, bound network input, never delete real worlds.
- Check REA tool availability for relevant research; unavailable REA does not block independent work. Record versioned public sources instead of inventing commands.
- The user's selected REA is `https://github.com/morluto/rea`. This cloud setup has a working `/workspace/.tools/rea` CLI when that path is present; use it for artifact analysis even if the active chat has no REA tool namespace. Keep tooling and analysis caches outside the product repository.
- Follow Inspect → Design → Implement → Test → Review → Benchmark → Document → Next Task. Keep `docs/development/status.md` factual: implemented, verified, failed, performance, limits and next action.
- Distinguish synthetic protocol tests, real socket tests and real Minecraft client verification. Do not call the goal complete while required game/content features remain missing.

Development commands (from the existing checkout; each cloud task is isolated, no extra worktree needed):

```sh
. ./scripts/dev-env.sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
cargo build --workspace --locked
python3 tests/interop/smoke.py
```

Do not publish the environment; the user has explicitly reserved publication.
