# Test structure and conventions

Tests follow Cargo's normal discovery rules. Each top-level `tests/*.rs` file
is an independent integration target; `tests/oracle/main.rs` is the entry point
for the larger oracle target. `cargo test --test cli` and `--test oracle` keep
their existing names. There is no manual target inventory in `Cargo.toml`.

## Unit tests

Unit tests live beside the implementation under `src/` and may access private
interfaces. Small suites can stay in an inline `#[cfg(test)] mod tests`. Larger
suites use `tests.rs` and topic files under `tests/`, behind the same test gate.
Do not expose production internals just to move a test.

- Heap tests cover storage, payload validation, buffers, collections, and GC.
- Compiler tests cover scope and binding rules, lowering, syntax, and debug data.
- Runtime tests cover publication, realms, execution, ownership, and builtin
  interactions. Module-loader tests belong under `src/runtime/module/tests/`.
- String intrinsic tests group Unicode, search, conversion, and recursion
  behavior. Bytecode-image tests group atoms, modules, graph topology, budgets,
  shared buffers, and writing.
- Tests that coordinate several internal components remain at their common
  owning module. Merely using a public method does not require moving a test.

The parent test modules retain imports and helpers used by multiple topics.
Helpers used by one topic belong in that topic. Test function names are retained
when splitting files, but full module-qualified filters gain the topic segment;
filter by the function name unless an exact path is needed.

## Integration tests

Integration tests use public library APIs or invoke a built executable. CLI and
oracle tests are integration tests, not additional Rust test categories.

```text
tests/
  checked_string_construction.rs  public string-construction contract
  unsupported_diagnostics.rs      public diagnostic contract
  rust_only.rs                    product/dependency boundary regressions
  cli.rs                         CLI target entry and shared fixture setup
  cli/                           arguments, printing, files, options, evaluation
  common/                        helpers shared across integration targets
  oracle/main.rs                 one oracle target with feature-gated host modules
  oracle/<topic>/                multi-file topic suites
  oracle/<topic>.rs              single-file topic suites
  oracle/support/                oracle-only helpers
  fixtures/inputs/               authored JS/module/C probe inputs
  fixtures/expected/             frozen reference-engine observations
```

Start a small integration suite as `tests/<topic>.rs`. Split its source into a
subdirectory when the suite has multiple substantial themes; that does not need
another executable. Oracle retains one executable to share its helpers and
configuration. Topic directory `mod.rs` files replace its former forwarding
wrappers, and normal `mod` declarations register their children.

`common/mod.rs` provides helpers shared by separate test targets. Oracle-specific
observation and comparison helpers live in `oracle/support/`. Keep per-topic
helpers local; avoid a universal runner with flags for unrelated behaviors.

## Writing tests

- Name the behavior and relevant condition in `snake_case`; place regressions
  with their behavior and mention an issue in a comment when it supplies context.
- Keep preparation, execution, and assertions easy to follow. Short tests do not
  need mechanical section comments.
- Assert the relevant result, error variant, and observable state. Check exact
  message text when the diagnostic itself is part of the compatibility contract.
- Use table-driven cases for the same operation over different inputs. Give cases
  descriptions so failures identify their input; keep different protocols separate.
- Use temporary directories for mutable files and clean them up on drop. Do not
  write into tracked fixtures or depend on the working directory.
- Keep reference-engine dependencies explicit. Many oracle tests return early
  with `SKIP` diagnostics when `QJS_ORACLE` is absent; an ordinary green run does
  **not** establish differential parity. CI's differential job supplies the
  pinned engine. Rust-only and frozen-observation assertions still run locally.
- Use doctests beside public APIs for executable usage examples.

## Data and conformance

See [fixture provenance](fixtures/README.md) for the input/output split.
Generated Test262 manifests and ledgers live under
`dev-support/test262/generated/`; the external corpus stays in the existing
pinned cache. The `run-test262` binary is gated by `test262-host`. See
[the conformance guide](../dev-support/test262/README.md) for complete runs and
receipt handling. Structural refactors do not refresh historical result receipts.

## Running checks

```sh
# Unit tests, ordinary integration tests, oracle Rust/frozen checks, examples.
cargo test --locked --workspace --all-targets
cargo test --locked --workspace --doc

# Host behavior and runner unit tests.
cargo test --locked -p quickjs-oxide --features test262-host \
  --lib --bins --test oracle --test unsupported_diagnostics

# One integration target or one behavior within it.
cargo test --locked --test cli
cargo test --locked --test oracle module_global_shadow

# Oracle tree registration and compiled default/host inventory.
./scripts/check-oracle-registry.sh --compiled
python3 scripts/test-oracle-registry.py

# Pinned differential suite (prepares its reference engine).
./scripts/test-parity-slice.sh

# Authenticate conformance inputs and existing receipts without a full run.
./scripts/test-test262.sh --spec dev-support/test262/current.conf --check
```

Formatting and Clippy use the CI toolchain recorded in `.github/workflows/ci.yml`.
When moving tests, compare both feature configurations' inventories, run the
affected targets, and update exact filters and fixture-path consumers together.
