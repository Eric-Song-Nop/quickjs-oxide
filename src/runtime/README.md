# Runtime responsibilities

`Runtime` owns shared engine state; `Context` owns a realm handle. Public types
remain exported through `runtime`, regardless of the file implementing them.

| Location | Responsibility |
| --- | --- |
| `runtime.rs` | Shared state, rooted handles, and internal runtime operations |
| `lifecycle.rs` | Runtime creation, identity, and host configuration |
| `error.rs` | Public runtime error types and conversions |
| `bootstrap.rs`, `bootstrap/realm.rs` | Intrinsic installation and realm construction |
| `context.rs` | Context handle ownership and the shared completion/exception boundary |
| `context/realm.rs` | Access to globals and realm intrinsics |
| `context/objects.rs` | Context object creation and property operations |
| `context/script.rs` | Compilation and script evaluation, including evaluation options |
| `context/calls.rs` | Bytecode execution, calls, and construction |
| `context/bytecode.rs` | Trusted bytecode loading through the Context API |
| `context/test262.rs` | Feature-gated Test262 helper constructors |
| `module.rs`, `jobs.rs`, `properties.rs` | Module execution, job processing, and property semantics |
| `qjs_host.rs`, `test262_host.rs`, `test262_agent.rs` | Existing host-specific implementations and installers |

Keep an operation with the subsystem that implements its semantics. A public
method does not need a separate forwarding layer merely because it is public.
Context child modules share the parent's completion handling rather than defining
separate exception protocols. Host-specific functions stay with their host;
engine execution and object semantics stay in the runtime.

This organization preserves the existing API, handle ownership, value conversion,
and pending-exception behavior. It does not introduce a new binding abstraction
or require callers to depend on these private implementation modules.
