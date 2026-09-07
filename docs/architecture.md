# Engine module responsibilities

The engine currently lives in one Rust crate. Internal modules separate
implementation responsibilities while the existing public paths remain stable.

## Heap

- `src/heap.rs` owns heap identities, stored payloads, allocation, and
  representation validation.
- `src/heap/buffers.rs` owns ArrayBuffer backing-store access, resizing,
  copying, transfer, and detach, plus SharedArrayBuffer handle cloning and
  growth. Observable coercions and view validation remain in the runtime.
- `src/heap/collections.rs` owns Map/Set and weak-collection records, their
  storage mutations, and live collection iterator state. Key comparison and
  the JavaScript iterator protocol remain runtime responsibilities.
- `src/heap/gc.rs` owns reference counting, edge and atom traversal, the
  ordered weak-reference pass, finalization, and cycle collection. Publication
  and mutation reuse the same ownership primitives and graph traversal.
- `src/heap/bytecode_validation.rs` validates frame pseudo bindings, parameter
  layouts, and eval environments before bytecode publication.
- `src/heap/private_validation.rs` authenticates private binding metadata and
  private callable initialization protocols before publication.
- `src/heap/native.rs` defines builtin selectors and native call descriptors.
  It describes callable metadata; builtin execution belongs to the runtime.
- `src/heap/tests.rs` provides shared test builders. Its child modules group
  tests by storage, payload, collection, buffer, module, and bytecode behavior.

Heap validation checks whether a stored representation is valid. It must not
execute JavaScript or invoke host callbacks while borrowing heap storage.
Public heap types retain their `quickjs_oxide::heap` paths through re-exports.

## Compiler

- `src/compiler.rs` owns compilation entry points, shared IR, and the parser.
  Existing syntax-specific child modules extend the parser.
- `src/compiler/scope_validation.rs` checks the completed scope and binding
  graph before name resolution.
- `src/compiler/resolution.rs` resolves names, declaration hoists, eval
  environments, and closure captures.
- `src/compiler/lowering.rs` converts resolved IR into verified bytecode and
  debug information. Its private scope-lifetime representation stays local.

These passes operate on compiler-owned IR and produce unlinked functions.
Publishing functions into a realm remains a runtime responsibility.

## Runtime

- `src/runtime.rs` owns runtime state and engine orchestration.
- `src/runtime/bootstrap.rs` installs primitive and function intrinsics and
  their initial property relationships.
- `src/runtime/context.rs` provides the realm-facing embedding operations.
- `src/runtime/intrinsics/` implements builtin behavior.
- Other existing child modules handle jobs, module execution, property
  semantics, callable state, and host integration.

The parent heap, compiler, and runtime files still contain multiple
responsibilities. Further extraction should follow ownership and execution
boundaries, preserve feature gates, and keep helper visibility within the
owning module. Splitting these modules into workspace crates is a separate step.
