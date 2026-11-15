# Source this file from a shell: . ./scripts/dev-env.sh
# Normal Rust installations do not need these cloud-specific paths.
if ! command -v cargo >/dev/null 2>&1 && [ -x /workspace/.tools/cargo/bin/cargo ]; then
    export CARGO_HOME=/workspace/.tools/cargo
    export RUSTUP_HOME=/workspace/.tools/rustup
    export PATH="$CARGO_HOME/bin:$PATH"
fi
