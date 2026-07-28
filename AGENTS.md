# Agent Instructions - client-ui-common

Shared standards live in [AGENTS.base.md](AGENTS.base.md), which is generated. This file holds the rules specific to this repo.

`client-ui-common` is the shared client core for Adele's UIs, a Cargo workspace of three
packages that must not blur together:

- The root package (`client-ui-common`) is a **pure, transport-free reducer** that stays
  **wasm-clean**: it consumes only wasm-compatible types (desktop-assistant's `api-model`,
  voice's `adele-voice-client-common` with the native chunker off) and expresses RPCs as
  `Effect`s the host runs, so it builds with `--target wasm32`. Never pull a transport crate
  or tokio into this package.
- The `ffi` member (`client-ui-ffi`) is the **native-only C-ABI** wrapper (cbindgen-generated
  header) that adds the transport plus tokio, and is what native clients such as `adele-kde`
  and `adele-mac` link against.
- The `markdown` member (`adele-markdown`) is the **security boundary for untrusted assistant
  text**: markdown to sanitized HTML, the CSP-pinned page templates the webview clients render
  replies inside, and the JavaScript-literal encoding for anything a host evaluates as script.
  It is a separate member because it pulls an HTML parser (`ammonia` / `html5ever`) that has no
  place in the wasm-clean reducer; consumers depend on it directly rather than through the root
  package.

Dependency sourcing is via path deps for now and is still being settled (so `api-model` is not
duplicated in a client's build graph). Where that sourcing blocks work here, track it rather
than working around it inside this crate. Warnings are denied mechanically via
the workspace `[lints]` (`rust.warnings` / `clippy.all` = "deny").

## Rust Conventions

Apply these consistently. The repo gate in **Overrides and additions to the shared base** is the floor.

### Coding
- `?` for error propagation. Reserve `unwrap` / `expect` for tests and proven invariants. When `expect`ing in production, the message must explain the invariant — not just describe what would be unwrapped.
- Prefer `&str` / `&[T]` in argument position; take ownership only when storing.
- Newtype wrappers for invariant-bearing values (validated ids, paths constrained to a directory, etc.).
- `From` / `Into` for type conversions; don't write `to_*` methods when traits suffice.
- Combinators (`map`, `and_then`, `unwrap_or_else`, `?`) over `match` for short `Option` / `Result` chains. Use `match` when there's branching control flow with side effects.
- Avoid `.clone()` on hot paths. `Arc<T>` for shared immutable, `Arc<Mutex<T>>` / `Arc<RwLock<T>>` for shared mutable.

### `unsafe`
- Don't use `unsafe` unless it's necessary AND you've reasoned about soundness. The bar is high.
- Required cases: `std::env::set_var` / `remove_var` (Rust 2024 edition makes these `unsafe` because libc env-mutation is not threadsafe). Anything else needs a strong justification.
- Every `unsafe` block must have a `// SAFETY:` comment naming the invariant the caller is relying on. No "obvious" unsafe — write the soundness argument down. Example:

  ```rust
  // SAFETY: single-threaded test; unique env-var name; no other code touches it.
  unsafe { std::env::remove_var(&unused); }
  ```

### Testing
- Unit tests colocated as `#[cfg(test)] mod tests {}` in lib files.
- Integration tests in `tests/` next to `Cargo.toml`.
- `#[tokio::test]` for async; `#[tokio::test(flavor = "multi_thread")]` only when explicitly testing concurrent behavior.
- Mock at trait boundaries. For HTTP: `httpmock`. For time: an injected `Clock` trait.
- Determinism: sort outputs before assertion; never depend on hash iteration order.
- `expect("descriptive reason")` over `unwrap()` in tests so failure messages are self-explanatory.
- Test public behavior, not private implementation. If a private fn needs testing, surface as `pub(crate)` with a documented contract.
- Don't hold `std::sync::MutexGuard` across `.await`. Drop the guard explicitly before awaiting — `clippy::await_holding_lock` flags this.

### Generics
- `impl Trait` in argument position for single-bound, single-use parameters.
- Named generics with `where` clauses for multiple bounds, recursion, or readability.
- Avoid generic explosion: 3+ generic parameters usually indicates a missing struct or associated type.
- Prefer `Arc<dyn Trait>` over hand-rolled enum-dispatch when there are many implementors and no perf-critical specialization.
- Trait bounds: keep `Send + Sync + 'static` co-located on the trait def when the trait is only useful in async contexts.

### Error handling
- Library crates: `thiserror` with structured variants.
- Binary crates: `anyhow` with `Context::context()` for narrative.
- **Never pattern-match on error message strings.** Pattern-match on variants. If you find yourself doing `error.to_string().contains("429")`, the upstream type is throwing away structured info that should be preserved.
- Surface enough context in `Display` for debugging without leaking secrets.

### Async
- Don't hold non-async locks (`std::sync::Mutex`, `parking_lot::Mutex`) across `.await`. Drop the guard explicitly, or use `tokio::sync::Mutex` if the lock genuinely needs to span the await.
- `tokio::join!` for independent parallel work; `tokio::try_join!` when both must succeed and the first error should cancel the rest.
- Long-running spawned tasks need cancellation — channel-based or `CancellationToken`. Don't leak.
- Cross-cutting context: `tokio::task_local!`.

### Documentation
- Doc comments (`///`) on every public item.
- Include rationale (`Why:` lines) for non-obvious choices, not just descriptions of behavior.
- Don't narrate PR / issue history in code comments. Reference issues only when the comment captures a non-obvious WHY tied to that issue.

## Overrides and additions to the shared base

Everything in [AGENTS.base.md](AGENTS.base.md) applies to this repo. This section
records only the points where this repo deliberately differs from the base, or adds a
rule the base does not have.

### 3.1 The gate for this repo (addition)

The `adelie-ai` repos have no CI. The gate is local and the author runs it. `just check` runs
all four of these in order. Run it, or run them by hand:

1. `cargo fmt --check`
2. `cargo clippy --workspace --all-targets -- -D warnings`
3. `cargo test --workspace`
4. The reducer still builds wasm-clean:
   `cargo build -p client-ui-common --target wasm32-unknown-unknown`.

`--workspace` on steps 2 and 3 is load-bearing, not decoration. This workspace has a **root
package** as well as members, and a cargo command with no package selection defaults to the
root package alone - so without it, clippy and the test run cover the reducer and skip both
`ffi` and `markdown`, which is to say they skip the C ABI and the sanitizer. Add `--workspace`
to any other cargo verb you reach for here, for the same reason.

Run `just audit` as well whenever `Cargo.lock` changed. Run `just install-hooks` once per clone
to put the gate on pre-push. The workspace `[lints]` table denies warnings mechanically, so a
plain `cargo build` or `cargo test` also hard-fails on one - there is no soft period.

### 4.3 Branch and pull request - merge when green (override, weaker than the base)

The base opens a pull request and waits for the user. In these repos the merge is delegated:
merge your own pull request as soon as it is green and independently shippable. Green here
means more than a clean build. The gate above passed, the tests cover the new behavior and
not only the absence of a panic, the security pass is done, and the change stands on its own.
Assign `dspadea` with `gh pr edit --add-assignee` and verify it; a review request from the
same account no-ops without an error, so never report a pull request as review-requested.
When in doubt, hold.

### 4.4 Worktrees - the group convention (addition)

Put the worktree at `.worktrees/<repo>/issue-N-slug/` under the group directory, on a branch
that mirrors the slug. Before you run tasks in parallel worktrees, look for shared files,
shared `Cargo.toml` dependency edits, and shared migration ordinals. Serialize the work where
they overlap, and tell each parallel agent the scope it owns.

### 6.1 Dependencies - the group's scan workflow (addition)

Base rule 6.1 sets the policy, including that a high or critical advisory blocks the change.
This group runs it with its own tooling:

1. Add the dependency (`cargo add <crate>`). This writes the lockfile but does not build.
2. Scan the updated lockfile with the `cve-mcp` server's `scan_packages` tool, or with
   `cargo audit`. Pass every (name, version, ecosystem) tuple.
3. Build only after the scan is clean, or after you have accepted the findings in writing.

### 9.1 Tracker for this project

GitHub Issues on `github.com/adelie-ai/client-ui-common`, together with the shared `adelie-ai` project
board. Manage entries with the `gh` CLI (`gh issue create`, `gh issue list`, `gh issue edit`,
`gh pr create`). The board states in use are In Progress, In Review, and Done.

### Platform, not a single product (addition)

Adele is a platform, not one product. Solve for the general case at every seam that is
plural by domain: storage backends, LLM providers, transports, clients, MCP servers, speech
engines. When a requirement names two of something, ask whether the real requirement is N
of them, and build that one instead.

Put the abstraction at the port. Keep the conditional compilation and the selection in one
factory, so a new implementation costs a crate, a feature, and one arm - not an edit to
every implementation that already exists. A hand-rolled `AnyX` enum with a variant per
implementation is the shape that fails this test: it re-dispatches every trait method by
hand and grows with the set.

Base rule 7.3 still holds inside a component. Do not invent indirection that a single call
site does not need. It does not licence the narrow build at a platform seam, because there
the plurality is the product, and the seam is already past the three-call-site test.

Fail loudly and by name when a configured selection is not compiled in, or is unavailable.
Name what was asked for and what is actually present. Silent degradation to a lesser
backend hides the problem from the one person who could fix it.
