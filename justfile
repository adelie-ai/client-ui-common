default:
    @just --list

# --- Local verification ("local CI") -----------------------------------------
# These repos run their gate locally rather than in GitHub Actions. `check` is
# the whole of it: formatting, lints, tests, and the reducer's wasm build.

# Full local gate: formatting, lints, tests, wasm-clean reducer
check: fmt-check lint test wasm

# Verify formatting without modifying files
fmt-check:
    cargo fmt --check

# Apply formatting
fmt:
    cargo fmt

# `--workspace` is load-bearing: this workspace has a ROOT package as well as
# members, and a cargo command with no package selection covers the root package
# only — which would leave the C ABI (`ffi`) and the sanitizer (`markdown`)
# entirely ungated.

# Clippy across every package; warnings are errors
lint:
    cargo clippy --workspace --all-targets -- -D warnings

# Test every package
test:
    cargo test --workspace

# The target is pinned in rust-toolchain.toml so a fresh clone has it.

# Build the reducer for wasm32 — it must stay transport-free and tokio-free
wasm:
    cargo build -p client-ui-common --target wasm32-unknown-unknown

# Dependency advisories. Run whenever Cargo.lock changed.
audit:
    cargo audit
