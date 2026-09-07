//! A pure-Rust rewrite of `QuickJS` aiming at semantic feature parity with the
//! pinned upstream release.
//!
//! The implementation deliberately follows `QuickJS`'s major runtime boundaries:
//! source is compiled to stack bytecode, bytecode executes inside a context,
//! and contexts share a runtime-owned heap and atom table.

pub use quickjs_oxide_core::atom;
pub use quickjs_oxide_core::bigint;
pub mod bytecode;
pub use quickjs_oxide_compiler::compiler;
pub use quickjs_oxide_core::debug;
pub use quickjs_oxide_core::error;
pub mod function;
pub mod heap;
pub use quickjs_oxide_compiler::lexer;
pub use quickjs_oxide_core::module;
pub use quickjs_oxide_core::number;
pub use quickjs_oxide_core::number_parse;
pub mod object;
pub mod property;
pub use quickjs_oxide_core::regexp;
pub mod runtime;
pub mod shape;
pub mod shared_memory;
pub use quickjs_oxide_core::source_text;
pub use quickjs_oxide_core::unicode;
pub use quickjs_oxide_core::unicode_case;
pub use quickjs_oxide_core::unicode_normalize;
pub use quickjs_oxide_core::unicode_property;
pub use quickjs_oxide_core::uri;
pub mod value;
pub mod vm;

pub use bigint::{BigIntError, JsBigInt};
pub use compiler::CompileOptions;
pub use debug::{
    DebugInfoMode, LineColumn, Pc2LineEntry, Pc2LineTable, QuickJsSourceLocator, SourceOffset,
};
pub use error::{Error, ErrorKind, SourceLocation, SourceSpan};
pub use function::FunctionBytecodeRef;
pub use heap::ContextId;
pub use heap::PromiseState;
pub use object::{
    AccessorValue, CallableRef, CompleteOrdinaryPropertyDescriptor, DescriptorField, ObjectRef,
    OrdinaryPropertyDescriptor, PropertyKey, SymbolRef, WellKnownSymbol,
};
pub use runtime::{
    Context, EvalOptions, HostServices, ModuleBytecodeRef, ModuleImportAttribute,
    ModuleImportAttributes, ModuleImportMetaProperty, ModuleLoadResult, ModuleLoader,
    ModuleLoaderError, ModuleLoaderRegistration, PendingJobError, PendingJobOutcome,
    PromiseRejectionEvent, PromiseSnapshot, Runtime, RuntimeError,
};
#[cfg(feature = "test262-host")]
pub use runtime::{Test262AgentError, Test262AgentSession};
pub use value::{JsString, JsStringError, Value};

/// The version of this quickjs-oxide engine crate.
pub const QUICKJS_OXIDE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The exact upstream release whose observable behavior is the compatibility
/// baseline for this crate.
pub const QUICKJS_COMPAT_VERSION: &str = "2026-06-04";
