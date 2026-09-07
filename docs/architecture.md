# Workspace architecture

The repository contains five library packages and three executable/adapter
packages. The root Cargo manifest owns shared dependencies, lint policy, and
build profiles. Source belongs to its package; no old source entry points are
retained at the repository root.

| Package | Responsibility | Production workspace dependencies |
| --- | --- | --- |
| `crates/core` | Strings, atoms, source positions, numeric primitives, bytecode, function/module drafts, and representation-only validation | None |
| `crates/compiler` | Lexing, parsing, scope analysis, name resolution, and lowering into unlinked drafts | core |
| `crates/engine` | Heap/GC, VM, rooted values, realm state, publication, objects, builtins, module execution, and jobs | core, compiler |
| `crates/host` | Native clock, timezone, random seed, and qjs output implementation | core |
| `crates/quickjs-oxide` | The Rust embedding entry point and default runtime construction | engine, host |
| `apps/cli` | qjs arguments, file loading, diagnostics, and process policy | quickjs-oxide |
| `apps/web` | WASM exports, browser host services, and result conversion | quickjs-oxide |
| `tools/test262` | Test262 admission, scheduling, execution, and reporting | quickjs-oxide with test262-host |

## Shared models and compilation

Core owns `JsString`, atoms, instructions, function descriptors, and unlinked
function/module products. `PrimitiveValue` contains only runtime-independent
constants. Object and symbol roots remain in the engine's `Value`. Publication
converts between these representations explicitly; no runtime handles enter
compiler-owned constant pools.

The pure regular-expression compiler/matcher lives in core because both literal
compilation and runtime RegExp construction consume it. The ECMAScript RegExp
object shell remains an engine builtin. This migration retains the existing
matcher rather than introducing a separate package or replacement engine.

Compiler owns its lexer and parser modules and the existing resolution and
lowering passes. There is no new AST pipeline. Bytecode layout checks which are
independent from allocation live in core; private-binding authentication and
heap identity checks remain with engine storage/publication.

Engine depends on compiler for `eval`, dynamic functions, and module compilation.
Compiler has no production dependency on engine. A module attribute callback
can stop parsing with `ModuleCompileFailure::Host`; the caller retains the exact
thrown engine value. The parser neither stringifies the exception nor owns it.

## Engine internals

- `heap/` owns storage, allocation, reference counting, edge traversal and GC.
- `vm.rs` owns execution frames, instructions, unwinding and suspension.
- `object.rs`, `property.rs`, `shape.rs`, and runtime property/internal-method
  modules own object representation and observable property semantics.
- `runtime/intrinsics/` owns ECMAScript builtin behavior.
- `runtime/bootstrap/` and `context/` own realm construction and operations.
- `runtime/bytecode_publish.rs` owns draft validation and publication.
- `runtime/module/` and `jobs.rs` own module execution and pending jobs.
- Engine `function.rs` and `object.rs` own rooted published handles.
- `runtime/host.rs` exposes the embedding service contract. The data-only
  contract is defined in core so providers need no dependency on engine state.

These are internal module boundaries, not additional Cargo packages. They may
share engine-private state without exporting heap mutation to an outer crate.

## Host and Rust embedding

`quickjs-oxide::Runtime::new()` selects the native host provider. Explicit host
services remain supported. The facade runtime owns an engine runtime and
forwards access through `Deref`; Context, Value, and rooted handles retain a
single engine-owned representation. `into_engine()` transfers that ownership
when composing lower-level APIs.

Engine-only callers construct a runtime with `Runtime::new_with_host_services`.
The engine does not select an operating-system provider. qjs helper installation
and value formatting remain with internal native dispatch; the host service
receives formatted output bytes and the flush policy. Custom providers may
capture output by implementing `HostServices::write_output`; its default is a
sink. The native provider preserves stdout writes and ignored I/O errors.

Test262 is opt-in. Agent sessions accept a runtime factory; each worker creates
its runtime on its own thread. The runner supplies native services. No runtime
root crosses a thread boundary.

## Tests and tooling

Unit tests live with their owner. Compiler execution tests use an engine dev
dependency and narrowly gated `test-support` APIs, without introducing a
production dependency cycle. Detached bytecode tests can use primitive values
in core/compiler and rooted values in engine.

Rust embedding integration tests live in `crates/quickjs-oxide/tests`. CLI and
oracle integration tests live in `apps/cli/tests`, where Cargo provides the qjs
binary. Oracle fixtures and expected outputs live with those tests. A shared
syntax-error assertion remains in the embedding test helpers and is imported
explicitly by the CLI test helpers.

Scripts retain their checks/test262/quickjs/unicode/web responsibilities.
Current Test262 fingerprints cover the new package trees, while historical
receipt input lists continue to authenticate their original commits.

For quick iteration, check the affected packages with `cargo check -p PACKAGE`.
At a completed migration boundary, use `cargo check --workspace --all-targets`.
Run focused tests for changed semantics; reserve the full oracle and mutation
suites for a deliberate broader validation pass.
