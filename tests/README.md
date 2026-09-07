# Test layout

Unit tests belong beside the implementation under `src/`. Small suites can use
an inline `#[cfg(test)] mod tests`; larger suites use `tests.rs` and, when useful,
topic modules under `tests/`. For example, `src/runtime/module/tests.rs` owns the
module-runtime unit suite. These tests may access private implementation details;
moving them must not require widening the public API.

Integration tests live here and exercise public APIs or executable behavior.
Small suites remain single files, such as `checked_string_construction.rs` and
`cli.rs`. Larger suites use one entry point with topic modules:

- `oracle/main.rs` registers the oracle suite, including feature-gated host tests.
- `oracle/oracle_*.rs` group existing suites and their topic subdirectories.
- `support/` contains shared integration-test helpers.
- `fixtures/` contains test inputs rather than Rust test targets.

Cargo uses `autotests = false` and explicit `[[test]]` entries. Oracle remains one
test executable; adding a topic file does not create another Cargo target. Keep
module names stable when moving tests so existing test filters continue to work.
The registry check detects unregistered suites and feature-gate drift.

Use doctests beside public API documentation for executable usage examples.
Test262 conformance runs belong to the existing `run-test262` runner and its
corpus configuration, separate from ordinary unit and integration suites.

Common checks:

```sh
cargo test --locked --workspace --all-targets
cargo test --locked -p quickjs-oxide --test oracle
cargo test --locked -p quickjs-oxide --features test262-host --test oracle
./scripts/check-oracle-registry.sh --compiled
```
