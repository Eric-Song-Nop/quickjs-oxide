//! Runtime-owned garbage-collected heap primitives.
//!
//! QuickJS combines explicit reference counts with a trial-deletion cycle
//! collector.  This module keeps the same ownership model while replacing raw
//! pointers with typed generational handles:
//!
//! - every heap edge is a raw, runtime-internal owned handle;
//! - publishing any node retains all of its outgoing heap edges;
//! - zero-reference destruction is driven by an iterative queue;
//! - cycle collection computes external references as
//!   `strong_count - internal_incoming_count`, marks their closure, and then
//!   actively dismantles unreachable object, function-bytecode, and context
//!   anchors;
//! - shapes and variable-reference cells participate in the graph, but
//!   cascade from active anchor destruction rather than acting as cycle
//!   anchors.
//!
//! Atom ownership remains at the runtime boundary.  A shape is expected to
//! arrive with one atom reference for every entry.  Finalization returns those
//! atoms in [`HeapCleanup::atoms`] so the caller can release them without
//! making this low-level arena depend on `AtomTable` mutability or callbacks.

use crate::function::metadata::{
    ClassInitializerKind, ClosureSource, ClosureVariable, ClosureVariableKind, ClosureVariableName,
    ConstructorKind, EvalBindingSource, EvalEnvironment, EvalKind, EvalScopeKind,
    EvalVariableEnvironment, FunctionKind, FunctionMetadata, ParameterEnvironmentLayout,
    VariableDefinition,
};

#[cfg(test)]
use crate::function::metadata::{EvalBinding, EvalScope, ParameterArgumentCell};

mod buffers;
mod gc;
#[cfg(test)]
use gc::object_atoms;
pub(crate) use gc::{FinalizationJobSink, PreparedFinalizationJob};
pub use gc::{GcStats, HeapCleanup, WeakSymbolGcEvent};
use gc::{
    async_generator_request_edges, context_edges, function_bytecode_edges,
    generator_activation_atoms, generator_activation_edges, object_edges, object_layout_edges,
    promise_reaction_edges, property_slot_atoms, property_slot_edges, raw_module_record_atoms,
    raw_module_record_edges, raw_value_atom, raw_value_edges, raw_value_matches_weak_key,
    shape_edges, var_ref_edges,
};
mod collections;
pub(crate) use bytecode_validation::{
    EvalEnvironmentPhaseContext, parameter_initializer_visible_locals,
    validate_class_initializer_bytecode_layout, validate_derived_constructor_bytecode_layout,
    validate_eval_environment_phase_layout, validate_parameter_bytecode_layout,
    validate_parameter_initializer_scope_layout, validate_pattern_parameter_bytecode_layout,
};
pub(crate) use collections::CollectionIteratorCurrentIndices;
pub use collections::{MapRecord, WeakCollectionKey, WeakCollectionRecords};
use quickjs_oxide_core::bytecode_validation;
mod private_validation;
use private_validation::validate_published_private_elements;
mod native;
#[cfg(feature = "test262-host")]
pub use native::Test262AgentKind;
pub use native::{
    ArrayBufferNativeKind, ArrayFindKind, ArrayFlattenKind, ArrayIterationKind, ArrayIteratorKind,
    ArrayJoinKind, ArrayPopKind, ArrayPushKind, ArrayReduceKind, ArraySearchKind, ArraySliceKind,
    AtomicsNativeKind, AtomicsOperationKind, BigIntAsNKind, DataViewElementKind,
    DataViewNativeKind, DateGetFieldKind, DateNativeKind, DateSetFieldKind, DateStringMethod,
    DynamicFunctionKind, ErrorConstructorKind, FinalizationRegistryNativeKind,
    FunctionDebugPosition, GeneratorResumeKind, GlobalNumberPredicateKind, GlobalUriCodecKind,
    JsonNativeKind, MapIteratorKind, MapNativeKind, MathBinaryKind, MathMinMaxKind, MathUnaryKind,
    NativeCProto, NativeFunctionData, NativeFunctionDescriptor, NativeFunctionId, NumberFormatKind,
    NumberParseKind, NumberPredicateKind, ObjectAccessorKind, ObjectExtensibilityKind,
    ObjectIntegrityKind, ObjectKeysKind, ObjectOwnPropertyKeysKind, PrimitiveKind,
    PromiseNativeKind, PromiseResolvingKind, ReflectKind, RegExpFlagKind, RegExpNativeKind,
    SetIteratorKind, SetNativeKind, SharedArrayBufferNativeKind, StringCaseKind, StringCharAtKind,
    StringCreateHtmlKind, StringIncludesKind, StringIndexOfKind, StringPadKind, StringReplaceKind,
    StringStaticKind, StringSubrangeKind, StringTrimKind, StringWellFormedKind, SymbolRegistryKind,
    TypedArrayElementKind, TypedArrayNativeKind, Uint8ArrayCodecKind, WeakMapNativeKind,
    WeakRefNativeKind, WeakSetNativeKind,
};
pub(crate) use native::{DynamicImportHandlerKind, ModuleEvaluationKind};

use std::cell::Cell;
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::error::Error;
use std::fmt;
use std::hash::Hash;
use std::rc::Rc;

use crate::atom::Atom;
use crate::bigint::JsBigInt;
use crate::bytecode::{Instruction, MAX_LOCAL_SLOTS, PrivateNameSource};
use crate::debug::Pc2LineTable;
use crate::error::NativeErrorKind;
use crate::module::{
    ModuleImport, ModuleImportCollision, ModuleImportName, ModuleLinkInitializer, ModuleRequest,
    ModuleRequestIndex, ModuleStarExport,
};
use crate::regexp::CompiledRegExp;
use crate::shape::{PropertyFlags, PropertyStorageKind, Shape, ShapeError};
use crate::shared_memory::SharedBufferHandle;
use crate::value::JsString;

/// Stable identity of an object slot until that slot is reclaimed.
///
/// The parts are exposed only for diagnostics.  There is intentionally no
/// public constructor: identities must originate from [`Heap::allocate_object`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectId {
    index: u32,
    generation: u32,
}

impl ObjectId {
    /// Arena index, intended for diagnostics and serialized debug traces only.
    #[must_use]
    pub const fn debug_index(self) -> u32 {
        self.index
    }

    /// Slot generation, intended for diagnostics and serialized debug traces.
    #[must_use]
    pub const fn debug_generation(self) -> u32 {
        self.generation
    }
}

impl fmt::Debug for ObjectId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ObjectId")
            .field("index", &self.index)
            .field("generation", &self.generation)
            .finish()
    }
}

/// Stable identity of a shape slot until that slot is reclaimed.
///
/// Shapes and objects share one arena, but their typed handles prevent normal
/// callers from mixing the two node kinds.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ShapeId {
    index: u32,
    generation: u32,
}

impl ShapeId {
    /// Arena index, intended for diagnostics and serialized debug traces only.
    #[must_use]
    pub const fn debug_index(self) -> u32 {
        self.index
    }

    /// Slot generation, intended for diagnostics and serialized debug traces.
    #[must_use]
    pub const fn debug_generation(self) -> u32 {
        self.generation
    }
}

impl fmt::Debug for ShapeId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ShapeId")
            .field("index", &self.index)
            .field("generation", &self.generation)
            .finish()
    }
}

/// Stable identity of one captured-variable cell.
///
/// QuickJS initially lets a `JSVarRef` point into a live stack frame and moves
/// the value into the `JSVarRef` when that frame closes.  This arena uses the
/// equivalent safe representation in which a captured local lives in its
/// `VarRefData` cell from the moment it is captured.  An active frame owns one
/// `VarRefId` root and every closure slot owns another reference to that same
/// identity, so reads and writes remain shared without storing stack pointers.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VarRefId {
    index: u32,
    generation: u32,
}

impl VarRefId {
    /// Arena index, intended for diagnostics and serialized debug traces only.
    #[must_use]
    pub const fn debug_index(self) -> u32 {
        self.index
    }

    /// Slot generation, intended for diagnostics and serialized debug traces.
    #[must_use]
    pub const fn debug_generation(self) -> u32 {
        self.generation
    }
}

impl fmt::Debug for VarRefId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VarRefId")
            .field("index", &self.index)
            .field("generation", &self.generation)
            .finish()
    }
}

/// Stable identity of a realm/context node until its arena slot is reclaimed.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContextId {
    index: u32,
    generation: u32,
}

impl ContextId {
    /// Arena index, intended for diagnostics and serialized debug traces only.
    #[must_use]
    pub const fn debug_index(self) -> u32 {
        self.index
    }

    /// Slot generation, intended for diagnostics and serialized debug traces.
    #[must_use]
    pub const fn debug_generation(self) -> u32 {
        self.generation
    }
}

impl fmt::Debug for ContextId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ContextId")
            .field("index", &self.index)
            .field("generation", &self.generation)
            .finish()
    }
}

/// Stable identity of immutable executable bytecode and its constant pool.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FunctionBytecodeId {
    index: u32,
    generation: u32,
}

impl FunctionBytecodeId {
    /// Arena index, intended for diagnostics and serialized debug traces only.
    #[must_use]
    pub const fn debug_index(self) -> u32 {
        self.index
    }

    /// Slot generation, intended for diagnostics and serialized debug traces.
    #[must_use]
    pub const fn debug_generation(self) -> u32 {
        self.generation
    }
}

impl fmt::Debug for FunctionBytecodeId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FunctionBytecodeId")
            .field("index", &self.index)
            .field("generation", &self.generation)
            .finish()
    }
}

/// Runtime heap node category.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HeapNodeKind {
    Object,
    Shape,
    VarRef,
    Context,
    FunctionBytecode,
}

/// Observable arena lifecycle state used by diagnostics and invariant tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HeapSlotState {
    Initializing,
    Live,
    ZeroQueued,
    Finalizing,
    Zombie,
    Vacant,
    Retired,
}

/// Failure of a checked heap ownership operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HeapError {
    WrongKind {
        expected: HeapNodeKind,
        actual: HeapNodeKind,
    },
    Stale {
        index: u32,
        generation: u32,
    },
    Overflow {
        operation: &'static str,
    },
    Allocation {
        operation: &'static str,
    },
    Underflow {
        kind: HeapNodeKind,
        index: u32,
        generation: u32,
    },
    Invariant(&'static str),
}

impl fmt::Display for HeapError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongKind { expected, actual } => {
                write!(
                    formatter,
                    "expected {expected:?} heap node, found {actual:?}"
                )
            }
            Self::Stale { index, generation } => {
                write!(
                    formatter,
                    "stale heap handle at slot {index}, generation {generation}"
                )
            }
            Self::Overflow { operation } => write!(formatter, "heap overflow during {operation}"),
            Self::Allocation { operation } => {
                write!(formatter, "heap allocation failed while {operation}")
            }
            Self::Underflow {
                kind,
                index,
                generation,
            } => write!(
                formatter,
                "{kind:?} reference-count underflow at slot {index}, generation {generation}"
            ),
            Self::Invariant(message) => write!(formatter, "heap invariant failed: {message}"),
        }
    }
}

impl Error for HeapError {}

/// Heap-internal value payload.
///
/// `Clone` duplicates raw payload bytes and primitive backing stores; it does
/// **not** retain an object edge.  Owned clones may enter the heap only through
/// checked methods such as [`Heap::allocate_object`] and
/// [`Heap::replace_object_slot`], which retain their edges transactionally.
#[derive(Clone, Debug, PartialEq)]
pub enum RawValue {
    Undefined,
    Null,
    Bool(bool),
    Int(i32),
    Float(f64),
    BigInt(JsBigInt),
    String(JsString),
    Symbol(Atom),
    /// Heap-internal class-private identity. This owns one private-atom
    /// reference exactly like `Symbol`, but it is not an ECMAScript Value and
    /// must never cross `Runtime::root_raw_value` or enter ordinary storage.
    Private(Atom),
    Object(ObjectId),
    Uninitialized,
    Exception,
}

/// Append-only identity of one module record in a Context-owned loaded-module
/// cache. A removed record leaves a tombstone and its identity is never
/// reused, matching the construction-order identity of QuickJS's
/// `JSContext.loaded_modules` list.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ModuleId(pub(crate) usize);

/// Runtime-internal module identity. `cache` is the defining Context whose
/// loaded-module cache owns `module`; all dependency indices in that record
/// refer to the same cache.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct RawModuleRef {
    pub(crate) cache: ContextId,
    pub(crate) module: ModuleId,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RawPublishedModuleExport {
    pub(crate) export_name: JsString,
    pub(crate) target: RawPublishedModuleExportTarget,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum RawPublishedModuleExportTarget {
    SourceTextLocal {
        closure_index: u16,
    },
    SyntheticLocal {
        cell_index: u16,
    },
    Indirect {
        request: ModuleRequestIndex,
        import_name: ModuleImportName,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum RawModuleRecordBody {
    /// Source-text module definition published before parsing begins.
    ///
    /// The record may expose only its source-order requested-module prefix;
    /// every heap-owning and executable field remains pristine until the
    /// compiler atomically replaces this body with `SourceText`.
    Parsing,
    SourceText {
        function: FunctionBytecodeId,
    },
    Json {
        default_value: RawValue,
    },
    /// Stable append-only identity retained after construction rollback.
    ///
    /// This state is used only when another live module already names the
    /// identity. It owns no heap edges and is excluded from name lookup.
    Aborted,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum RawModuleResolutionState {
    Unresolved,
    Resolving,
    /// QuickJS latches `resolved` before invoking host callbacks and does not
    /// retry after a callback failure which the host subsequently clears.
    /// Rust keeps that state explicit instead of retaining unsafe partial raw
    /// dependency pointers.
    Failed,
    Resolved(Rc<[ModuleId]>),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RawModuleInstance {
    pub(crate) slots: Vec<Option<VarRefId>>,
    pub(crate) callable: Option<ObjectId>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum RawModuleNamespaceState {
    Empty,
    Building(ObjectId),
    Ready(ObjectId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RawModuleLinkStatus {
    Unlinked,
    Linking,
    Linked,
    Poisoned,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum RawModuleEvaluationState {
    Unevaluated,
    Evaluating,
    EvaluatingAsync,
    Evaluated,
    Errored(RawValue),
    Poisoned,
}

/// First-execution realm retained by a linked module record. The defining
/// cache realm is represented without a heap edge because the Context already
/// owns the loaded-module record; only a distinct realm is an outgoing edge.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum RawModuleLinkRealm {
    Cache,
    Other(ContextId),
}

/// Sealed, allocation-free metadata transitions for one loaded module. Every
/// variant changes exactly one non-owning field after authenticating its
/// precise predecessor state; ownership-bearing record changes must use the
/// transactional publication APIs instead.
pub(crate) enum RawModuleTransition {
    BeginResolution,
    FinishResolution(Rc<[ModuleId]>),
    FailResolution,
    ResetResolution,
    BeginLink,
    FinishLink,
    ResetLink,
    PoisonLink,
    BeginEvaluation,
    /// Publish one completed evaluation SCC. An already-assigned async order
    /// selects `EvaluatingAsync`; its absence selects `Evaluated`.
    FinishEvaluation {
        cycle_root: ModuleId,
    },
    /// Mark an active module as transitively async before its SCC is
    /// published. The order is the runtime-global QuickJS evaluation stamp.
    BeginAsyncEvaluation {
        order: u64,
    },
    /// Complete a successfully executed async module after every dependency
    /// has become available.
    FinishAsyncEvaluation,
    PoisonEvaluation,
    FinishNamespace(ObjectId),
}

/// Raw Context-owned counterpart of QuickJS's `JSModuleDef`.
///
/// Cloning this structure creates only a borrowed snapshot: arena identities
/// and Symbol atoms are retained exclusively by the containing ContextData.
/// A snapshot must therefore not outlive the cache root or be used after a
/// mutation releases one of its raw fields unless that field was promoted to
/// an owning runtime root first.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RawModuleRecord {
    pub(crate) name: JsString,
    pub(crate) body: RawModuleRecordBody,
    /// Lazily allocated canonical `import.meta` object.
    ///
    /// This is the direct counterpart of QuickJS `JSModuleDef::meta_obj`:
    /// the module record owns the object independently from the hidden
    /// closure cell used by source text which actually reads `import.meta`.
    pub(crate) import_meta: Option<ObjectId>,
    pub(crate) declaration_order: Rc<[u16]>,
    pub(crate) link_initializers: Rc<[ModuleLinkInitializer]>,
    pub(crate) import_collisions: Rc<[ModuleImportCollision]>,
    pub(crate) requested_modules: Rc<Vec<ModuleRequest>>,
    pub(crate) imports: Rc<[ModuleImport]>,
    pub(crate) exports: Rc<[RawPublishedModuleExport]>,
    pub(crate) star_exports: Rc<[ModuleStarExport]>,
    pub(crate) resolution: RawModuleResolutionState,
    pub(crate) instance: Option<RawModuleInstance>,
    pub(crate) namespace: RawModuleNamespaceState,
    pub(crate) link_status: RawModuleLinkStatus,
    pub(crate) evaluation: RawModuleEvaluationState,
    /// Exact counterpart of QuickJS `JSModuleDef::has_tla`. Every source-text
    /// module executes through async bytecode, so function kind alone cannot
    /// distinguish authored top-level await.
    pub(crate) has_top_level_await: bool,
    /// Synchronous evaluation SCC root, assigned when the SCC completes (or
    /// to the requested root for active records on abrupt completion).
    pub(crate) evaluation_cycle_root: Option<ModuleId>,
    /// Cached Promise created for the first module evaluation attempt.
    ///
    /// This mirrors QuickJS's `JSModuleDef::promise`: the Context-owned
    /// record retains it independently of callers, and every later dynamic
    /// import observes the same evaluation identity and settlement history.
    pub(crate) evaluation_promise: Option<ObjectId>,
    /// The resolving pair belonging to `evaluation_promise`. The three fields
    /// are published atomically and retained for the lifetime of the cycle
    /// root, matching QuickJS `JSModuleDef::resolving_funcs`.
    pub(crate) evaluation_resolve: Option<ObjectId>,
    pub(crate) evaluation_reject: Option<ObjectId>,
    /// Number of async dependency roots which have not yet become available.
    pub(crate) pending_async_dependencies: u32,
    /// Reverse dependency edges. Duplicates are semantically significant:
    /// each occurrence balances one pending dependency count on its parent.
    pub(crate) async_parent_modules: Vec<ModuleId>,
    /// Runtime-global ordering stamp while `async_evaluation` is true in
    /// QuickJS. Successful or abrupt completion clears the stamp.
    pub(crate) async_evaluation_order: Option<u64>,
    pub(crate) link_realm: Option<RawModuleLinkRealm>,
    pub(crate) compile_realm: ContextId,
}

fn module_record_has_pristine_construction_metadata(record: &RawModuleRecord) -> bool {
    record.import_meta.is_none()
        && record.declaration_order.is_empty()
        && record.link_initializers.is_empty()
        && record.import_collisions.is_empty()
        && record.imports.is_empty()
        && record.exports.is_empty()
        && record.star_exports.is_empty()
        && record.instance.is_none()
        && matches!(record.namespace, RawModuleNamespaceState::Empty)
        && matches!(record.link_status, RawModuleLinkStatus::Unlinked)
        && matches!(record.evaluation, RawModuleEvaluationState::Unevaluated)
        && !record.has_top_level_await
        && record.evaluation_cycle_root.is_none()
        && record.evaluation_promise.is_none()
        && record.evaluation_resolve.is_none()
        && record.evaluation_reject.is_none()
        && record.pending_async_dependencies == 0
        && record.async_parent_modules.is_empty()
        && record.async_evaluation_order.is_none()
        && record.link_realm.is_none()
}

fn module_record_references_identity(record: &RawModuleRecord, module: ModuleId) -> bool {
    matches!(
        &record.resolution,
        RawModuleResolutionState::Resolved(dependencies)
            if dependencies.contains(&module)
    ) || record.async_parent_modules.contains(&module)
        || record.evaluation_cycle_root == Some(module)
}

fn validate_module_body_replacement(
    current: &RawModuleRecord,
    replacement: &RawModuleRecord,
) -> Result<(), HeapError> {
    match (&current.body, &replacement.body) {
        (RawModuleRecordBody::Parsing, RawModuleRecordBody::Parsing) => {
            if replacement.requested_modules.len() < current.requested_modules.len()
                || !replacement
                    .requested_modules
                    .starts_with(current.requested_modules.as_slice())
            {
                return Err(HeapError::Invariant(
                    "parse-in-progress module replacement changed its request prefix",
                ));
            }
            if replacement.resolution != current.resolution {
                return Err(HeapError::Invariant(
                    "parse-in-progress module replacement changed its resolution state",
                ));
            }
        }
        (RawModuleRecordBody::Parsing, RawModuleRecordBody::SourceText { .. }) => {
            if replacement.requested_modules != current.requested_modules {
                return Err(HeapError::Invariant(
                    "completed module changed its published request prefix",
                ));
            }
            if replacement.resolution != current.resolution {
                return Err(HeapError::Invariant(
                    "completed module changed its parse-time resolution state",
                ));
            }
        }
        (RawModuleRecordBody::Parsing, RawModuleRecordBody::Aborted) => {
            return Err(HeapError::Invariant(
                "module construction abort bypassed its ownership primitive",
            ));
        }
        (RawModuleRecordBody::SourceText { .. }, RawModuleRecordBody::SourceText { .. })
        | (RawModuleRecordBody::Json { .. }, RawModuleRecordBody::Json { .. }) => {}
        (RawModuleRecordBody::Aborted, _) => {
            return Err(HeapError::Invariant(
                "aborted module identity cannot be replaced",
            ));
        }
        _ => {
            return Err(HeapError::Invariant(
                "loaded-module replacement changed its body state illegally",
            ));
        }
    }
    Ok(())
}

fn aborted_module_record(record: &RawModuleRecord) -> RawModuleRecord {
    RawModuleRecord {
        name: record.name.clone(),
        body: RawModuleRecordBody::Aborted,
        import_meta: None,
        declaration_order: Rc::from([]),
        link_initializers: Rc::from([]),
        import_collisions: Rc::from([]),
        requested_modules: Rc::new(Vec::new()),
        imports: Rc::from([]),
        exports: Rc::from([]),
        star_exports: Rc::from([]),
        resolution: RawModuleResolutionState::Unresolved,
        instance: None,
        namespace: RawModuleNamespaceState::Empty,
        link_status: RawModuleLinkStatus::Unlinked,
        evaluation: RawModuleEvaluationState::Unevaluated,
        has_top_level_await: false,
        evaluation_cycle_root: None,
        evaluation_promise: None,
        evaluation_resolve: None,
        evaluation_reject: None,
        pending_async_dependencies: 0,
        async_parent_modules: Vec::new(),
        async_evaluation_order: None,
        link_realm: None,
        compile_realm: record.compile_realm,
    }
}

/// Construction-ordered loaded modules for one Context. Slots are never
/// compacted or reused: rollback changes an unreferenced slot to `None` and a
/// referenced slot to `Aborted`, while the name map continues to point at the
/// oldest remaining live record.
#[derive(Debug, PartialEq)]
pub(crate) struct LoadedModuleCache {
    records: Vec<Option<RawModuleRecord>>,
    first_by_name: HashMap<JsString, ModuleId>,
}

impl LoadedModuleCache {
    fn new() -> Self {
        Self {
            records: Vec::new(),
            first_by_name: HashMap::new(),
        }
    }

    fn validate_first_by_name(&self) -> Result<(), HeapError> {
        for record in self.records.iter().flatten() {
            if matches!(&record.body, RawModuleRecordBody::Aborted) {
                continue;
            }
            if !self.first_by_name.contains_key(&record.name) {
                return Err(HeapError::Invariant(
                    "loaded-module oldest-name index is incomplete",
                ));
            }
        }
        for (name, first) in &self.first_by_name {
            let record =
                self.records
                    .get(first.0)
                    .and_then(Option::as_ref)
                    .ok_or(HeapError::Invariant(
                        "loaded-module oldest-name index references a tombstone",
                    ))?;
            if matches!(&record.body, RawModuleRecordBody::Aborted) {
                return Err(HeapError::Invariant(
                    "loaded-module oldest-name index references an aborted record",
                ));
            }
            if &record.name != name {
                return Err(HeapError::Invariant(
                    "loaded-module oldest-name index references another name",
                ));
            }
            if self.records[..first.0].iter().flatten().any(|earlier| {
                !matches!(&earlier.body, RawModuleRecordBody::Aborted) && &earlier.name == name
            }) {
                return Err(HeapError::Invariant(
                    "loaded-module name index does not reference the oldest live record",
                ));
            }
        }
        Ok(())
    }

    fn rebuild_first_by_name_excluding(
        &self,
        mut excluded: impl FnMut(ModuleId) -> bool,
    ) -> Result<HashMap<JsString, ModuleId>, HeapError> {
        // `JsString` mutates only transparent rope caches; its UTF-16
        // equality/hash identity is stable while used as a module-name key.
        #[allow(clippy::mutable_key_type)]
        let mut rebuilt = HashMap::new();
        rebuilt
            .try_reserve(self.first_by_name.len())
            .map_err(|_| HeapError::Allocation {
                operation: "rebuilding loaded-module oldest-name index",
            })?;
        for (index, record) in self.records.iter().enumerate() {
            let id = ModuleId(index);
            if excluded(id) {
                continue;
            }
            if let Some(record) = record
                && !matches!(&record.body, RawModuleRecordBody::Aborted)
            {
                rebuilt.entry(record.name.clone()).or_insert(id);
            }
        }
        Ok(rebuilt)
    }
}

/// Parallel property payload for one shape entry.
#[derive(Clone, Debug, PartialEq)]
pub enum PropertySlot {
    Data(RawValue),
    /// QuickJS `JS_PROP_VARREF`: an ordinary data descriptor whose mutable
    /// payload lives in a shared variable cell.
    VarRef(VarRefId),
    Accessor {
        get: Option<ObjectId>,
        set: Option<ObjectId>,
    },
    /// QuickJS-style lazy intrinsic property. It has ordinary data-property
    /// flags in the shape; only the payload and owned realm edge are deferred.
    AutoInit(AutoInitProperty),
}

/// Typed autoinit payloads. Keeping the creation realm in the per-object slot
/// mirrors QuickJS's `JSProperty.u.init.realm_and_id` and allows objects which
/// share a shape to retain different realms.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AutoInitProperty {
    FunctionPrototype {
        realm: ContextId,
    },
    NativeBuiltin {
        realm: ContextId,
        target: NativeFunctionId,
        name: &'static str,
        length: u8,
        min_readable_args: u8,
    },
    String {
        realm: ContextId,
        value: &'static str,
    },
    ArrayUnscopables {
        realm: ContextId,
    },
    /// QuickJS `JS_OBJECT_DEF` payload for the realm's global `Math` object.
    Math {
        realm: ContextId,
    },
    /// QuickJS `JS_OBJECT_DEF` payload for the realm's global `Reflect` object.
    Reflect {
        realm: ContextId,
    },
    /// QuickJS `JS_OBJECT_DEF` payload for the realm's global `JSON` object.
    Json {
        realm: ContextId,
    },
    /// QuickJS `JS_OBJECT_DEF` payload for the realm's global `Atomics` object.
    Atomics {
        realm: ContextId,
    },
    #[cfg(test)]
    FailureProbe {
        realm: ContextId,
    },
}

/// Realm-owned identities needed to allocate and dispatch genuine RegExp
/// objects and their string iterators. QuickJS roots the constructor, iterator
/// prototype, and initial instance shape independently from their public
/// property graph because user code may replace or delete those properties
/// after bootstrap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegExpRealmData {
    pub prototype: ObjectId,
    pub constructor: ObjectId,
    /// Realm-local `%RegExpStringIteratorPrototype%`, created with the RegExp
    /// intrinsic and inheriting from this realm's `%IteratorPrototype%`.
    pub string_iterator_prototype: ObjectId,
    pub object_shape: ShapeId,
}

/// Realm-owned identities required to allocate genuine Map objects and their
/// iterators. QuickJS roots the two class prototypes, but not the public Map
/// constructor: deleting the global and `Map.prototype.constructor` edges may
/// therefore make that constructor collectible.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MapRealmData {
    pub prototype: ObjectId,
    /// Realm-local `%MapIteratorPrototype%`, inheriting from this realm's
    /// `%IteratorPrototype%`.
    pub iterator_prototype: ObjectId,
}

/// Realm-owned identities required to allocate genuine Set objects and their
/// iterators. As with Map, QuickJS roots the two class prototypes without
/// independently rooting the public Set constructor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SetRealmData {
    pub prototype: ObjectId,
    /// Realm-local `%SetIteratorPrototype%`, inheriting from this realm's
    /// `%IteratorPrototype%`.
    pub iterator_prototype: ObjectId,
}

/// Realm-owned `%WeakMap.prototype%` class root.
///
/// Weak collections have no iterator prototype. As in pinned QuickJS, the
/// public constructor remains reachable through the ordinary property graph
/// rather than through an additional Context edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WeakMapRealmData {
    pub prototype: ObjectId,
}

/// Realm-owned `%WeakSet.prototype%` class root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WeakSetRealmData {
    pub prototype: ObjectId,
}

/// Realm-owned class prototypes installed together by QuickJS's
/// `JS_AddIntrinsicWeakRef` bootstrap step. The public constructors remain
/// reachable through their ordinary prototype/global property graph and are
/// therefore not duplicated as Context roots.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WeakRefRealmData {
    pub weak_ref_prototype: ObjectId,
    pub finalization_registry_prototype: ObjectId,
}

/// Realm-owned identities required by synchronous generator functions and
/// generator instances.
///
/// QuickJS roots both class prototypes independently: generator function
/// objects inherit from `function_prototype`, while generator instances use
/// `prototype` as the cross-realm fallback when a callable's public
/// `.prototype` is not an object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GeneratorRealmData {
    pub prototype: ObjectId,
    pub function_prototype: ObjectId,
}

/// Realm-owned `%AsyncFunction.prototype%` identity.
///
/// Async function objects inherit from this ordinary object, which is itself
/// a direct child of the realm's `%Function.prototype%`. The hidden
/// `AsyncFunction` constructor remains reachable through the reciprocal
/// property graph and therefore needs no independent Context root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AsyncFunctionRealmData {
    pub function_prototype: ObjectId,
}

/// Realm-owned identities required by async-generator functions and objects.
///
/// QuickJS keeps `%AsyncIteratorPrototype%`, `%AsyncGeneratorPrototype%`, and
/// `%AsyncGeneratorFunction.prototype%` as independent context roots. The
/// hidden dynamic constructor remains reachable from the reciprocal graph.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AsyncGeneratorRealmData {
    pub async_iterator_prototype: ObjectId,
    /// Realm-local `%AsyncFromSyncIteratorPrototype%`, inheriting from this
    /// realm's `%AsyncIteratorPrototype%`.
    pub async_from_sync_iterator_prototype: ObjectId,
    pub prototype: ObjectId,
    pub function_prototype: ObjectId,
}

/// Realm-owned Promise identities used by allocation and species fallback.
///
/// Both identities remain explicit Context roots.  User code may delete the
/// public global and constructor/prototype properties without changing the
/// intrinsic identities used by Promise abstract operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PromiseRealmData {
    pub prototype: ObjectId,
    pub constructor: ObjectId,
}

/// Realm-owned identities used by the synchronous Iterator helpers proposal.
///
/// `%IteratorPrototype%` is already a mandatory [`ContextData`] root because
/// concrete built-in iterators depend on it before the public `%Iterator%`
/// constructor is installed. The four identities here are attached later as
/// one transaction, matching QuickJS's `iterator_ctor` and class-prototype
/// roots for `JS_CLASS_ITERATOR_CONCAT`, `JS_CLASS_ITERATOR_HELPER`, and
/// `JS_CLASS_ITERATOR_WRAP`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IteratorRealmData {
    pub constructor: ObjectId,
    pub concat_prototype: ObjectId,
    pub helper_prototype: ObjectId,
    pub wrap_prototype: ObjectId,
}

/// Realm-local `%ArrayBuffer.prototype%` class root.
///
/// The backing bytes live on each branded object, but constructor-realm
/// fallback must retain the original prototype even after authored code
/// replaces or deletes the writable global `ArrayBuffer` binding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArrayBufferRealmData {
    pub prototype: ObjectId,
}

/// Realm-local `%SharedArrayBuffer.prototype%` class root.
///
/// Shared wrappers may outlive and share backing stores across runtimes, but
/// their JavaScript prototype identity remains owned by the importing realm.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SharedArrayBufferRealmData {
    pub prototype: ObjectId,
}

/// Realm-local `%DataView.prototype%` class root.
///
/// DataView instances retain their backing ArrayBuffer-family object directly. The realm
/// keeps only the original prototype identity used by constructor fallback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DataViewRealmData {
    pub prototype: ObjectId,
}

/// Realm-local concrete TypedArray class prototypes.
///
/// QuickJS roots these twelve identities in `ctx->class_proto`. The hidden
/// abstract prototype stays reachable through their `[[Prototype]]` edges;
/// retaining it separately here would add an arena root that upstream lacks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TypedArrayRealmData {
    pub prototypes: [ObjectId; TypedArrayElementKind::COUNT],
}

/// Realm-owned roots which participate in QuickJS's cycle graph.
///
/// The bootstrap roots needed by ordinary script evaluation are explicit;
/// additional intrinsic and module roots can extend the vectors without
/// changing `ContextId` ownership.
#[derive(Debug, PartialEq)]
pub struct ContextData {
    /// Stable public handle identity assigned by `Runtime::new_context`.
    ///
    /// Low-level heap tests and internal job-only realms deliberately leave
    /// this absent; only a user-created Context may be reconstructed for a
    /// host callback.
    pub(crate) public_id: Option<u64>,
    pub object_prototype: ObjectId,
    pub function_prototype: ObjectId,
    /// Realm-local `%Array.prototype%`. QuickJS creates the prototype itself
    /// as a genuine Array exotic object, so this root is never substituted by
    /// an ordinary object even before the global `%Array%` constructor is
    /// published.
    pub array_prototype: ObjectId,
    /// Realm-local `%IteratorPrototype%`.  It is rooted independently rather
    /// than rediscovered through a concrete iterator so empty realms retain
    /// the intrinsic identity required by cross-realm iterator creation.
    pub iterator_prototype: ObjectId,
    /// Realm-local `%ArrayIteratorPrototype%`, whose prototype is the realm's
    /// `%IteratorPrototype%`.
    pub array_iterator_prototype: ObjectId,
    /// Realm-local `%Array%` constructor, attached after the cyclic Context
    /// has been published.
    pub array_constructor: Option<ObjectId>,
    /// Original realm-local `Array.prototype.values` identity. QuickJS caches
    /// this callable in `JSContext.array_proto_values` and installs that
    /// cached value on every later arguments object even if user code mutates
    /// or deletes `Array.prototype.values`.
    pub array_prototype_values: Option<ObjectId>,
    /// Realm-local `%StringIteratorPrototype%`, whose prototype is the realm's
    /// `%IteratorPrototype%`.
    pub string_iterator_prototype: ObjectId,
    /// Realm-local equivalents of QuickJS `class_proto[JS_CLASS_*]` for the
    /// five primitive wrapper classes. An absent entry remains an explicit
    /// implementation gap rather than inheriting from the wrong prototype.
    pub primitive_prototypes: [Option<ObjectId>; PrimitiveKind::COUNT],
    /// Realm-local `%Date.prototype%`. Pinned QuickJS creates this as an
    /// ordinary object without a Date time-value slot, then uses it as the
    /// default prototype for genuine Date instances.
    pub date_prototype: Option<ObjectId>,
    /// Realm-local RegExp constructor, ordinary prototype, RegExp String
    /// Iterator prototype, and canonical one-slot instance shape. They are
    /// attached atomically after the cyclic Context has been published.
    pub regexp: Option<RegExpRealmData>,
    /// Realm-local Map constructor, ordinary prototype, and Map Iterator
    /// prototype, attached atomically after the cyclic Context is published.
    pub map: Option<MapRealmData>,
    /// Realm-local Set ordinary prototype and Set Iterator prototype,
    /// attached atomically after the cyclic Context is published.
    pub set: Option<SetRealmData>,
    /// Realm-local WeakMap ordinary prototype, attached after its public
    /// constructor/prototype cycle is initialized.
    pub weak_map: Option<WeakMapRealmData>,
    /// Realm-local WeakSet ordinary prototype, attached after its public
    /// constructor/prototype cycle is initialized.
    pub weak_set: Option<WeakSetRealmData>,
    /// Realm-local WeakRef and FinalizationRegistry class prototypes,
    /// attached atomically by the shared weak-reference bootstrap step.
    pub weak_ref: Option<WeakRefRealmData>,
    /// Realm-local `%ArrayBuffer.prototype%` class root, attached after the
    /// public constructor/prototype cycle has been initialized and validated.
    pub array_buffer: Option<ArrayBufferRealmData>,
    /// Realm-local `%SharedArrayBuffer.prototype%` class root, attached after
    /// the public constructor/prototype cycle has been initialized.
    pub shared_array_buffer: Option<SharedArrayBufferRealmData>,
    /// Realm-local `%DataView.prototype%` class root, attached after the
    /// public constructor/prototype cycle has been initialized and validated.
    pub data_view: Option<DataViewRealmData>,
    /// Realm-local concrete TypedArray prototype roots. The hidden abstract
    /// prototype stays reachable through the concrete prototype graph.
    pub typed_array: Option<TypedArrayRealmData>,
    /// Realm-local `%GeneratorPrototype%` and
    /// `%GeneratorFunction.prototype%`, attached after their reciprocal
    /// constructor/prototype graph has been initialized.
    pub generator: Option<GeneratorRealmData>,
    /// Realm-local `%AsyncFunction.prototype%`, attached after its hidden
    /// constructor/prototype graph has been initialized.
    pub async_function: Option<AsyncFunctionRealmData>,
    /// Realm-local async-iterator and async-generator intrinsic graph.
    pub async_generator: Option<AsyncGeneratorRealmData>,
    /// Realm-local `%Promise.prototype%` and `%Promise%`, attached atomically
    /// after their reciprocal public property graph is initialized.
    pub promise: Option<PromiseRealmData>,
    /// Realm-local `%Iterator%` constructor plus the hidden Iterator Concat,
    /// Iterator Helper, and Iterator Wrap class prototypes.
    pub iterator: Option<IteratorRealmData>,
    /// `%Function%`, published after the cyclic realm bootstrap has created
    /// `%Function.prototype%` and the global object.
    pub function_constructor: Option<ObjectId>,
    /// Shared frozen poison callable used by legacy restricted function
    /// accessors and strict arguments objects.
    pub throw_type_error: Option<ObjectId>,
    /// Original realm-local `%eval%` identity. QuickJS caches this callable
    /// separately from the writable/configurable global `eval` property so
    /// direct-eval dispatch can compare identity after user mutation.
    pub eval_function: Option<ObjectId>,
    pub global_object: ObjectId,
    /// Null-prototype storage for global lexical bindings (`let`/`const`).
    pub global_var_object: ObjectId,
    pub error_prototype: Option<ObjectId>,
    pub native_error_prototypes: [Option<ObjectId>; NativeErrorKind::COUNT],
    /// QuickJS keeps Math.random's xorshift64* state on each JSContext.  Zero
    /// is reserved for the not-yet-seeded bootstrap state.
    math_random_state: u64,
    pub global_objects: Vec<ObjectId>,
    pub intrinsics: Vec<RawValue>,
    pub initial_shapes: Vec<ShapeId>,
    /// Context-local `JSModuleDef` ownership, matching QuickJS's
    /// `JSContext.loaded_modules` rather than a Rust-side parallel graph.
    pub(crate) loaded_modules: LoadedModuleCache,
}

impl ContextData {
    /// Construct the complete mandatory realm root set.
    ///
    /// Iterator prototype identities are required arguments rather than
    /// builder-populated placeholders so the public low-level allocator
    /// cannot publish a realm whose `%IteratorPrototype%` silently aliases
    /// `%Object.prototype%`.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        object_prototype: ObjectId,
        function_prototype: ObjectId,
        array_prototype: ObjectId,
        iterator_prototype: ObjectId,
        array_iterator_prototype: ObjectId,
        string_iterator_prototype: ObjectId,
        global_object: ObjectId,
        global_var_object: ObjectId,
    ) -> Self {
        Self {
            public_id: None,
            object_prototype,
            function_prototype,
            array_prototype,
            iterator_prototype,
            array_iterator_prototype,
            array_constructor: None,
            array_prototype_values: None,
            string_iterator_prototype,
            primitive_prototypes: [None; PrimitiveKind::COUNT],
            date_prototype: None,
            regexp: None,
            map: None,
            set: None,
            weak_map: None,
            weak_set: None,
            weak_ref: None,
            array_buffer: None,
            shared_array_buffer: None,
            data_view: None,
            typed_array: None,
            generator: None,
            async_function: None,
            async_generator: None,
            promise: None,
            iterator: None,
            function_constructor: None,
            throw_type_error: None,
            eval_function: None,
            global_object,
            global_var_object,
            error_prototype: None,
            native_error_prototypes: [None; NativeErrorKind::COUNT],
            math_random_state: 0,
            global_objects: Vec::new(),
            intrinsics: Vec::new(),
            initial_shapes: Vec::new(),
            loaded_modules: LoadedModuleCache::new(),
        }
    }

    /// Attach the stable public identity of a user-created Context.
    #[must_use]
    pub(crate) const fn with_public_id(mut self, id: u64) -> Self {
        self.public_id = Some(id);
        self
    }

    /// Attach one implemented primitive wrapper prototype to this realm.
    #[must_use]
    pub const fn with_primitive_prototype(
        mut self,
        kind: PrimitiveKind,
        prototype: ObjectId,
    ) -> Self {
        self.primitive_prototypes[kind.index()] = Some(prototype);
        self
    }

    /// Attach the ordinary Date prototype to this realm before publish.
    #[must_use]
    pub const fn with_date_prototype(mut self, prototype: ObjectId) -> Self {
        self.date_prototype = Some(prototype);
        self
    }

    /// Attach the Error intrinsic prototype graph to this realm.
    #[must_use]
    pub const fn with_error_prototypes(
        mut self,
        error_prototype: ObjectId,
        native_error_prototypes: [ObjectId; NativeErrorKind::COUNT],
    ) -> Self {
        self.error_prototype = Some(error_prototype);
        let mut index = 0;
        while index < NativeErrorKind::COUNT {
            self.native_error_prototypes[index] = Some(native_error_prototypes[index]);
            index += 1;
        }
        self
    }
}

/// Constant-pool entry owned by a [`FunctionBytecodeData`] node.
#[derive(Clone, Debug, PartialEq)]
pub enum BytecodeConstant {
    Value(RawValue),
    /// Compile-once RegExp literal payload. These reference-counted Rust
    /// leaves own no arena or atom edge; executing `Instruction::RegExp`
    /// clones them into a fresh realm-local RegExp object.
    RegExp {
        pattern: JsString,
        program: Rc<CompiledRegExp>,
    },
    Function(FunctionBytecodeId),
}

/// Publication-authenticated role of one class-private lexical capability.
///
/// Setter primary and synthetic `<set>` cells deliberately share
/// [`ClosureVariableKind::PrivateSetter`]. The runtime publisher is the only
/// layer which still owns both the exact source spelling and its interned
/// [`Atom`], so it seals that distinction here before handing linked bytecode
/// to the atom-table-independent heap.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum PublishedPrivateBindingRole {
    Primary,
    SetterStorage,
}

/// One non-owning identity authenticated by the runtime publisher. `name`
/// must equal the atom already owned by the corresponding variable definition
/// or closure descriptor. Local setter halves also carry reciprocal `pair`
/// indices; closure captures may legitimately retain only one half and leave
/// it absent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct PublishedPrivateBinding {
    name: Atom,
    role: PublishedPrivateBindingRole,
    pair: Option<u16>,
}

impl PublishedPrivateBinding {
    #[must_use]
    pub(crate) const fn primary(name: Atom, pair: Option<u16>) -> Self {
        Self {
            name,
            role: PublishedPrivateBindingRole::Primary,
            pair,
        }
    }

    #[must_use]
    pub(crate) const fn setter_storage(name: Atom, pair: Option<u16>) -> Self {
        Self {
            name,
            role: PublishedPrivateBindingRole::SetterStorage,
            pair,
        }
    }
}

#[derive(Debug)]
struct AuthenticatedPrivateBindings {
    locals: Box<[Option<PublishedPrivateBinding>]>,
    closures: Box<[Option<PublishedPrivateBinding>]>,
}

/// Sealed bridge between name-aware bytecode publication and the heap.
///
/// Public linked-bytecode callers can construct only [`Self::none`]. Any
/// private definition or closure descriptor requires the crate-internal
/// authenticated constructor, preventing a forged `FunctionBytecodeData`
/// from choosing whether a `PrivateSetter` cell is a primary name or its
/// synthetic write capability.
#[derive(Debug)]
pub struct PublishedPrivateBindings {
    authenticated: Option<AuthenticatedPrivateBindings>,
}

impl PublishedPrivateBindings {
    #[must_use]
    pub const fn none() -> Self {
        Self {
            authenticated: None,
        }
    }

    #[must_use]
    pub(crate) fn authenticated(
        locals: Vec<Option<PublishedPrivateBinding>>,
        closures: Vec<Option<PublishedPrivateBinding>>,
    ) -> Self {
        Self {
            authenticated: Some(AuthenticatedPrivateBindings {
                locals: locals.into_boxed_slice(),
                closures: closures.into_boxed_slice(),
            }),
        }
    }
}

impl Default for PublishedPrivateBindings {
    fn default() -> Self {
        Self::none()
    }
}

/// Runtime-owned immutable bytecode, constant pool, and function realm.
///
/// `code` is an `Rc` leaf with no runtime edges.  The VM may cheaply clone it
/// before dropping the runtime's `RefCell` borrow, while a bytecode root keeps
/// the raw constant pool alive for the duration of execution.
#[derive(Debug)]
pub struct FunctionBytecodeData {
    pub code: Rc<[Instruction]>,
    pub constants: Rc<[BytecodeConstant]>,
    pub realm: ContextId,
    pub metadata: FunctionMetadata,
    pub parameter_environment: Option<ParameterEnvironmentLayout>,
    /// Intrinsic source-level name. Contextual `SetName` inference remains a
    /// separate opcode and is only emitted for anonymous definitions.
    pub func_name: Option<JsString>,
    pub argument_definitions: Rc<[VariableDefinition]>,
    pub local_definitions: Rc<[VariableDefinition]>,
    pub closure_variables: Rc<[ClosureVariable]>,
    /// Name-bound private capability roles sealed by the runtime publisher.
    /// This metadata owns no atoms; identities alias definition/descriptor
    /// atoms whose references remain in `auxiliary_atoms`.
    pub private_bindings: PublishedPrivateBindings,
    pub eval_environments: Rc<[EvalEnvironment<Atom>]>,
    pub debug: Option<FunctionDebugInfo>,
    /// Atom references owned by bytecode metadata/opcode operands.
    ///
    /// Symbol constants are excluded: every `RawValue::Symbol` occurrence owns
    /// and releases its own separate atom reference.
    pub auxiliary_atoms: Box<[Atom]>,
}

/// Runtime-owned debug metadata for one bytecode function.
///
/// `filename` is backed by one distinct reference in `auxiliary_atoms`; it is
/// intentionally not released separately when the bytecode node dies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FunctionDebugInfo {
    pub filename: Atom,
    pub pc2line: Option<Pc2LineTable>,
    pub source: Option<Box<[u8]>>,
}

/// Mutable storage shared by an active frame and all closures capturing one
/// argument or local.
///
/// `is_lexical` and `is_const` mirror the metadata carried by QuickJS
/// `JSVarRef`.  Enforcement belongs to the VM; the heap owns and traces the
/// current value.  The root returned by [`Heap::allocate_var_ref`] is intended
/// to be the active frame's ownership.  Function-object closure slots retain
/// the same identity and therefore keep the cell alive after frame teardown.
#[derive(Clone, Debug, PartialEq)]
pub struct VarRefData {
    pub value: RawValue,
    pub is_lexical: bool,
    pub is_const: bool,
    pub kind: ClosureVariableKind,
}

impl VarRefData {
    /// Construct a captured argument/local cell.
    #[must_use]
    pub const fn local(value: RawValue) -> Self {
        Self {
            value,
            is_lexical: false,
            is_const: false,
            kind: ClosureVariableKind::Normal,
        }
    }

    /// Construct a lexical/global-style cell with explicit const metadata.
    #[must_use]
    pub const fn lexical(value: RawValue, is_const: bool) -> Self {
        Self {
            value,
            is_lexical: true,
            is_const,
            kind: ClosureVariableKind::Normal,
        }
    }

    /// Construct a cell from one compiler-produced closure descriptor.
    #[must_use]
    pub const fn captured(
        value: RawValue,
        is_lexical: bool,
        is_const: bool,
        kind: ClosureVariableKind,
    ) -> Self {
        Self {
            value,
            is_lexical,
            is_const,
            kind,
        }
    }
}

/// Internal primitive payload carried by implemented wrapper classes.
///
/// New variants are added only with their complete class slice so Symbol atom
/// ownership and String exotic storage cannot be accidentally skipped by a
/// prematurely generic raw-value container.
#[derive(Clone, Debug, PartialEq)]
pub enum PrimitiveObjectData {
    Number(f64),
    /// Exact UTF-16 backing store for a genuine String wrapper. Unlike Symbol,
    /// the reference-counted string payload owns no atom or heap edge.
    String(JsString),
    Boolean(bool),
    /// One owned atom reference for a genuine local, global, or well-known
    /// Symbol. `object_atoms` returns it during wrapper finalization.
    Symbol(Atom),
    BigInt(JsBigInt),
}

impl PrimitiveObjectData {
    #[must_use]
    pub const fn kind(&self) -> PrimitiveKind {
        match self {
            Self::Number(_) => PrimitiveKind::Number,
            Self::String(_) => PrimitiveKind::String,
            Self::Boolean(_) => PrimitiveKind::Boolean,
            Self::Symbol(_) => PrimitiveKind::Symbol,
            Self::BigInt(_) => PrimitiveKind::BigInt,
        }
    }
}

/// Internal payload of one genuine `JS_CLASS_REGEXP` object.
///
/// QuickJS allocates the branded object before compiling its pattern, so the
/// explicit uninitialized state preserves that observable allocation/error
/// order. Compiled programs and their source strings are reference-counted
/// leaves outside the GC arena and own no heap or atom edge.
#[derive(Clone, Debug, PartialEq)]
pub enum RegExpObjectData {
    Uninitialized,
    Compiled {
        pattern: JsString,
        program: Rc<CompiledRegExp>,
    },
}

/// One live registration owned by a genuine `FinalizationRegistry`.
///
/// `target` and `unregister_token` are non-owning generational identities.
/// `held_value` owns its ordinary object or Symbol edge until the registration
/// is unregistered, finalized with its registry, or moved into a prepared
/// finalization job by the ordered weak-object pass.
#[derive(Clone, Debug, PartialEq)]
struct FinalizationRegistryEntry {
    target: WeakCollectionKey,
    held_value: RawValue,
    unregister_token: Option<WeakCollectionKey>,
}

/// QuickJS-shaped opaque data for one genuine `FinalizationRegistry`.
///
/// The cleanup callback and creation realm are strong arena edges. Entries
/// remain in registration order so token clearing, target clearing, and job
/// preparation follow pinned QuickJS's single forward traversal.
#[doc(hidden)]
#[derive(Clone, Debug, PartialEq)]
pub struct FinalizationRegistryData {
    callback: ObjectId,
    realm: ContextId,
    entries: Vec<FinalizationRegistryEntry>,
}

/// ECMAScript-visible lifecycle of a branded synchronous generator object.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GeneratorState {
    SuspendedStart,
    SuspendedYield,
    SuspendedYieldStar,
    Executing,
    Completed,
}

/// Heap-native representation of one argument or local binding retained by a
/// dormant generator frame. Runtime-owning root wrappers must never enter this
/// structure: every GC identity is stored as a raw arena edge instead.
#[derive(Clone, Debug, PartialEq)]
pub enum GeneratorFrameBinding {
    Direct(RawValue),
    Private(Atom),
    PrivateCallable(ObjectId),
    Uninitialized,
    Captured(VarRefId),
}

/// Raw VM fields retained across a synchronous-generator suspension.
#[derive(Clone, Debug, PartialEq)]
pub struct GeneratorVmActivation {
    pub stack: Vec<RawValue>,
    pub regions: Vec<crate::vm::VmUnwindRegion>,
    pub pc: usize,
    pub callee_realm: ContextId,
    pub current_function: ObjectId,
    pub this_value: RawValue,
    pub normalized_this: Option<RawValue>,
    pub new_target: RawValue,
    pub strict: bool,
    pub callee_global: ObjectId,
}

/// Complete dormant execution state owned by one generator object.
#[derive(Clone, Debug, PartialEq)]
pub struct GeneratorActivationData {
    pub bytecode: FunctionBytecodeId,
    pub vm: GeneratorVmActivation,
    pub actual_argument_count: usize,
    pub arguments: Vec<GeneratorFrameBinding>,
    pub locals: Vec<GeneratorFrameBinding>,
    pub reusable_captured_locals: Vec<bool>,
}

/// ECMAScript-visible lifecycle of a branded async-generator object.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AsyncGeneratorState {
    SuspendedStart,
    SuspendedYield,
    SuspendedYieldStar,
    Executing,
    AwaitingReturn,
    Completed,
}

/// One queued `.next`, `.return`, or `.throw` request and its Promise
/// capability. Every identity is stored as a raw traced edge.
#[derive(Clone, Debug, PartialEq)]
pub struct AsyncGeneratorRequestData {
    pub completion: GeneratorResumeKind,
    pub result: RawValue,
    pub promise: ObjectId,
    pub resolve: ObjectId,
    pub reject: ObjectId,
}

/// Complete hidden state of one genuine AsyncGenerator.
#[derive(Clone, Debug, PartialEq)]
pub struct AsyncGeneratorData {
    pub state: AsyncGeneratorState,
    pub activation: Option<Box<GeneratorActivationData>>,
    pub queue: VecDeque<AsyncGeneratorRequestData>,
    /// Realm whose intrinsic Promise machinery owns the currently installed
    /// await/return reaction callbacks.
    pub resume_realm: Option<ContextId>,
}

/// Settlement branch selected by an internal async-function resume callback.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AsyncFunctionResumeKind {
    Fulfill,
    Reject,
}

/// Settlement branch selected by an internal async-generator reaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AsyncGeneratorResumeKind {
    AwaitFulfill,
    AwaitReject,
    ReturnFulfill,
    ReturnReject,
}

/// Heap-visible lifecycle of one async-function driver.
///
/// The active VM frame is rooted by the runtime while `Executing`. At an
/// `await`, ownership transfers into the state object and the phase becomes
/// `Awaiting`; `Completed` is absorbing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AsyncFunctionPhase {
    Executing,
    Awaiting,
    Completed,
}

/// Hidden driver state shared by the two callbacks installed for one `await`.
///
/// QuickJS keeps this as a separate GC object so the pending Promise reaction
/// owns the dormant activation without exposing it through authored
/// properties. `driver_realm` is the original caller realm which supplies the
/// returned Promise and await jobs; it may differ from the bytecode activation
/// realm. Every arena identity here is a raw, traced edge.
#[derive(Clone, Debug, PartialEq)]
pub struct AsyncFunctionStateData {
    pub driver_realm: ContextId,
    pub outer_resolve: ObjectId,
    pub outer_reject: ObjectId,
    pub activation: Option<Box<GeneratorActivationData>>,
    pub phase: AsyncFunctionPhase,
}

/// Lazy operation retained by a genuine Iterator Helper object.
///
/// The eager consumers (`every`, `find`, `forEach`, and `some`) do not
/// allocate a helper payload and therefore use [`IteratorConsumerKind`]
/// instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IteratorHelperKind {
    Drop,
    Filter,
    FlatMap,
    Map,
    Take,
}

/// Eager operation selected by the shared Iterator consumer implementation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IteratorConsumerKind {
    Every,
    Find,
    ForEach,
    Some,
}

/// Resume operation shared by Iterator Helper and Iterator Wrap prototypes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IteratorResumeKind {
    Next,
    Return,
}

/// Complete hidden state of one genuine Iterator Helper.
///
/// Object-only fields use typed handles; the cached `next` and callback remain
/// raw arena-owned values because property lookup can produce any ECMAScript
/// value. QuickJS keeps all four edges alive until finalization even after
/// `done` becomes true.
#[derive(Clone, Debug, PartialEq)]
pub struct IteratorHelperData {
    pub source: ObjectId,
    pub next: RawValue,
    pub callback: RawValue,
    pub inner: Option<ObjectId>,
    pub count: i64,
    pub kind: IteratorHelperKind,
    pub executing: bool,
    pub done: bool,
}

/// Hidden state of an Iterator created by `Iterator.from`.
#[derive(Clone, Debug, PartialEq)]
pub struct IteratorWrapData {
    pub source: RawValue,
    pub next: RawValue,
}

/// Hidden state of the branded adapter created when `GetAsyncIterator`
/// falls back to a synchronous iterator.
///
/// The source is known to be an object after `GetIterator`, while `next`
/// remains an arbitrary cached ECMAScript value until the first call.
#[derive(Clone, Debug, PartialEq)]
pub struct AsyncFromSyncIteratorData {
    pub sync_iterator: ObjectId,
    pub next: RawValue,
}

/// One eagerly validated iterable/method pair retained by `Iterator.concat`.
///
/// Consumed slots become `None` so their edges can be released immediately,
/// matching QuickJS's advancing finalizer boundary without shifting the
/// remaining vector.
#[derive(Clone, Debug, PartialEq)]
pub struct IteratorConcatItem {
    pub iterable: ObjectId,
    pub method: RawValue,
}

/// Hidden state of the lazy iterator returned by `Iterator.concat`.
#[derive(Clone, Debug, PartialEq)]
pub struct IteratorConcatData {
    pub items: Vec<Option<IteratorConcatItem>>,
    pub index: usize,
    pub iterator: Option<ObjectId>,
    pub next: RawValue,
    pub running: bool,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IteratorHelperRawValueField {
    Next,
    Callback,
}

#[cfg(test)]
impl IteratorHelperRawValueField {
    fn get_mut(self, data: &mut IteratorHelperData) -> &mut RawValue {
        match self {
            Self::Next => &mut data.next,
            Self::Callback => &mut data.callback,
        }
    }
}

/// ECMAScript-visible state of one genuine Promise object.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PromiseState {
    Pending,
    Fulfilled,
    Rejected,
}

/// Which settlement path owns a Promise reaction record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PromiseReactionKind {
    Fulfill,
    Reject,
}

/// Raw, heap-owned resolving functions retained by a Promise reaction.
///
/// These are internal arena edges, not public runtime-owning wrappers.  A
/// reaction keeps both callables alive until it is detached or its owning
/// Promise is finalized. The result Promise itself is returned synchronously
/// from `then`; QuickJS does not retain it as a separate reaction edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PromiseCapabilityData {
    pub resolve: ObjectId,
    pub reject: ObjectId,
}

/// One `PerformPromiseThen` reaction retained by a pending Promise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PromiseReaction {
    pub kind: PromiseReactionKind,
    pub handler: Option<ObjectId>,
    /// `None` is QuickJS's thrown-away capability optimization used by the
    /// internal reactions which resume a suspended async function.
    pub capability: Option<PromiseCapabilityData>,
}

/// Complete hidden state of one genuine Promise object.
///
/// `result` is `undefined` while pending.  Reaction vectors are kept separate
/// to preserve QuickJS's fulfill/reject list order, while each record also
/// carries its kind so queued jobs remain self-describing after detachment.
#[derive(Clone, Debug, PartialEq)]
pub struct PromiseData {
    pub state: PromiseState,
    pub result: RawValue,
    pub fulfill_reactions: Vec<PromiseReaction>,
    pub reject_reactions: Vec<PromiseReaction>,
    pub is_handled: bool,
}

/// Mutable edge capture owned by an internal NewPromiseCapability executor.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PromiseCapabilityExecutorData {
    pub resolve: Option<RawValue>,
    pub reject: Option<RawValue>,
}

/// Hidden state of one genuine `JS_CLASS_PROXY` object.
///
/// QuickJS retains both edges after revocation because either value may still
/// be referenced by an active native call. `is_callable` is fixed at creation
/// time, while the object's constructor bit independently mirrors the target's
/// initial `[[Construct]]` capability.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProxyData {
    pub target: ObjectId,
    pub handler: ObjectId,
    pub is_callable: bool,
    pub is_revoked: bool,
}

/// QuickJS `JSArrayBuffer` storage owned directly by one branded object.
///
/// `max_byte_length == None` denotes a fixed-length buffer. A detached buffer
/// keeps its resizable/fixed identity and maximum while releasing all bytes;
/// its observable byte length is therefore zero without conflating an
/// attached empty buffer with a detached one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArrayBufferData {
    pub bytes: Vec<u8>,
    pub max_byte_length: Option<u32>,
    pub detached: bool,
}

/// QuickJS `JS_CLASS_SHARED_ARRAY_BUFFER` wrapper-local state.
///
/// The handle owns no arena edge: it keeps the shared backing alive through
/// `Arc`, while byte length and maximum length remain local to this wrapper.
/// Cloning the handle therefore mirrors QuickJS's SAB structured-clone hook.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SharedArrayBufferData {
    pub handle: SharedBufferHandle,
}

/// Borrow-free snapshot of one ArrayBuffer backing-store state.
///
/// Runtime DataView operations use this value to finish observable validation
/// without retaining a heap borrow across coercions or subsequent mutations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ArrayBufferState {
    pub byte_length: u32,
    pub max_byte_length: Option<u32>,
    pub detached: bool,
}

/// Common borrow-free state used by future ArrayBuffer and SharedArrayBuffer
/// views. Shared buffers are never detached.
pub(crate) type BufferState = ArrayBufferState;

/// Shared ArrayBuffer-view layout carried by a genuine DataView.
///
/// `fixed_byte_length == None` denotes a length-tracking view over a resizable
/// ArrayBuffer. Detach and resize may make this structurally valid view
/// temporarily out of bounds; that observable state is checked at access time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArrayBufferViewData {
    pub buffer: ObjectId,
    pub byte_offset: u32,
    pub fixed_byte_length: Option<u32>,
}

/// Shared integer-indexed view layout carried by all twelve TypedArray classes.
///
/// The nested byte-oriented view is deliberately the same substrate used by
/// DataView and mirrors QuickJS's `JSTypedArray.length`. `None` denotes a
/// length-tracking view over a resizable ArrayBuffer; fixed byte lengths are
/// validated to be divisible by the selected element width.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TypedArrayData {
    pub view: ArrayBufferViewData,
    pub element: TypedArrayElementKind,
}

/// Typed hidden payload carried only by runtime-created native functions.
///
/// The shared `already_resolved` cell is intentionally a non-arena leaf: the
/// resolve/reject pair shares one first-call-wins bit without introducing a
/// `Runtime -> heap -> Runtime` ownership cycle.  Every raw object identity in
/// this enum is still an ordinary traced and reference-counted heap edge.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum InternalCallableData {
    /// `Proxy.revocable`'s one-shot revocation closure. The edge is released
    /// after the first call, matching QuickJS's `func_data[0] = JS_NULL`.
    ProxyRevoke {
        proxy: Option<ObjectId>,
    },
    AsyncFunctionResume {
        state: ObjectId,
        kind: AsyncFunctionResumeKind,
    },
    AsyncGeneratorResume {
        generator: ObjectId,
        kind: AsyncGeneratorResumeKind,
    },
    PromiseResolving {
        promise: ObjectId,
        already_resolved: Rc<Cell<bool>>,
        kind: PromiseResolvingKind,
    },
    PromiseCapabilityExecutor(PromiseCapabilityExecutorData),
    PromiseFinallyHandler {
        /// `None` preserves QuickJS's `JS_UNDEFINED` default-constructor
        /// sentinel instead of eagerly materializing the intrinsic Promise.
        constructor: Option<ObjectId>,
        on_finally: ObjectId,
    },
    PromiseFinallyThunk {
        value: RawValue,
    },
    PromiseAllResolveElement {
        values: ObjectId,
        resolve: ObjectId,
        remaining: Rc<Cell<i32>>,
        already_called: Rc<Cell<bool>>,
        index: u32,
    },
    PromiseAllSettledElement {
        values: ObjectId,
        resolve: ObjectId,
        remaining: Rc<Cell<i32>>,
        already_called: Rc<Cell<bool>>,
        index: u32,
        outcome: PromiseReactionKind,
    },
    PromiseAnyRejectElement {
        errors: ObjectId,
        reject: ObjectId,
        remaining: Rc<Cell<i32>>,
        already_called: Rc<Cell<bool>>,
        index: u32,
    },
    AsyncFromSyncIteratorUnwrap {
        done: bool,
    },
    AsyncFromSyncIteratorClose {
        sync_iterator: ObjectId,
    },
    /// Fulfill/reject callback attached to an authored top-level-await body
    /// Promise. The Context edge keeps the loaded-module cache alive; the
    /// append-only ModuleId remains non-owning within that cache.
    ModuleEvaluation {
        module: RawModuleRef,
        kind: ModuleEvaluationKind,
    },
    /// Handler attached to a pending module-evaluation Promise by dynamic
    /// import. It retains both caller-facing settling functions and the
    /// Context-owned module cache until the evaluation settles.
    DynamicImportHandler {
        module: RawModuleRef,
        resolve: ObjectId,
        reject: ObjectId,
        kind: DynamicImportHandlerKind,
    },
}

/// Class-specific edges stored alongside an object's ordinary properties.
// `ObjectPayload` remains a public diagnostic envelope, but native-function
// captures are deliberately crate-authenticated and cannot be forged by an
// embedder. Public callers may still inspect that a payload is native with
// `internal: _` without naming the hidden capture type.
#[allow(private_interfaces)]
#[derive(Clone, Debug, PartialEq)]
pub enum ObjectPayload {
    Ordinary,
    /// Runtime-wide, unforgeable `JS_CLASS_RAWJSON` brand. The exact source
    /// text remains in the object's frozen ordinary `rawJSON` data slot, so
    /// the payload needs no duplicate GC edge or string owner.
    RawJson,
    /// A genuine `JS_CLASS_ARRAY` exotic object. The mandatory `length`
    /// property and every named or slow indexed property remain in the
    /// ordinary shape/slot arrays. QuickJS's contiguous C/W/E prefix lives in
    /// `dense` instead and therefore does not allocate decimal property atoms.
    Array {
        /// QuickJS `u.array.values[0..count]`. `Some` is the fast form and
        /// `None` records its irreversible conversion to ordinary properties.
        /// A fast Array has no holes inside this vector; its logical `length`
        /// slot may nevertheless be greater than `dense.len()`.
        dense: Option<Vec<RawValue>>,
    },
    /// QuickJS's two arguments classes share the same fast indexed storage
    /// protocol. Mapped entries use `PropertySlot::VarRef`; unmapped entries
    /// use ordinary data slots. `None` records the irreversible fast-to-slow
    /// transition caused by redefining or deleting a non-tail index.
    Arguments {
        mapped: bool,
        fast_len: Option<u32>,
    },
    /// `JS_CLASS_ARRAY_ITERATOR`: the boxed source is released permanently at
    /// exhaustion, while `kind` selects keys, values, or entry pairs.
    ArrayIterator {
        object: Option<ObjectId>,
        next_index: u32,
        kind: ArrayIteratorKind,
    },
    /// QuickJS `JS_CLASS_FOR_IN_ITERATOR`. Property names and enumerable bits
    /// are snapshotted one prototype level at a time; the current object is
    /// the payload's only GC edge.
    ForInIterator(ForInIteratorData),
    /// QuickJS `JSObject.u.object_data` for implemented primitive wrappers.
    Primitive(PrimitiveObjectData),
    /// QuickJS `JS_CLASS_DATE`'s internal millisecond time value. NaN is the
    /// required invalid-Date sentinel for genuine Date instances.
    Date(f64),
    /// QuickJS `JS_CLASS_REGEXP`'s source and compiled matcher program.
    RegExp(RegExpObjectData),
    /// `JS_CLASS_REGEXP_STRING_ITERATOR`: the species-created matcher is the
    /// payload's sole arena edge. The iterated UTF-16 string is reference
    /// counted outside the arena. Completion deliberately retains both values
    /// until finalization, matching QuickJS's class finalizer.
    RegExpStringIterator {
        regexp: ObjectId,
        string: JsString,
        global: bool,
        full_unicode: bool,
        done: bool,
    },
    /// `JS_CLASS_MAP`: stable insertion-order records plus the live-entry
    /// count used by the `size` getter. Tombstones are never compacted while
    /// the Map is live, preserving mutation-sensitive iterator semantics.
    Map {
        records: Vec<MapRecord>,
        /// Ordered stable indices of live records. Historical tombstones stay
        /// in `records` for iterators, but bounded diagnostic traversal does
        /// not need to rescan them.
        live_indices: BTreeSet<usize>,
        size: usize,
    },
    /// `JS_CLASS_MAP_ITERATOR`: the source Map remains an owned edge until
    /// exhaustion, while `next_index` walks stable record indices and skips
    /// tombstones in the runtime layer.
    MapIterator {
        object: Option<ObjectId>,
        next_index: usize,
        /// Stable record returned by the most recent successful `next()`.
        /// QuickJS retains that record until the iterator advances again, so
        /// deleting it can leave a printer-visible zombie in the source Map.
        current_index: Option<usize>,
        kind: MapIteratorKind,
    },
    /// `JS_CLASS_SET`: the ordered record layout is shared with Map, but each
    /// live record stores its element in `key` and keeps `value` exactly
    /// `undefined`. A distinct payload preserves the unforgeable Set brand.
    Set {
        records: Vec<MapRecord>,
        /// Ordered stable indices of live elements; see the Map counterpart.
        live_indices: BTreeSet<usize>,
        size: usize,
    },
    /// `JS_CLASS_SET_ITERATOR`: the source Set remains an owned edge until
    /// exhaustion. `kind` distinguishes value iteration from entry-pair
    /// iteration while both `keys` and `values` use the value projection.
    SetIterator {
        object: Option<ObjectId>,
        next_index: usize,
        /// Stable record returned by the most recent successful `next()`.
        /// This mirrors `JSMapIteratorData.cur_record` for Set iterators.
        current_index: Option<usize>,
        kind: SetIteratorKind,
    },
    /// `JS_CLASS_WEAKMAP`: generation-checked weak keys, strongly owned
    /// values, and a hash-indexed intrusive insertion-order list matching
    /// QuickJS's internal weak-record traversal.
    WeakMap {
        records: WeakCollectionRecords<RawValue>,
    },
    /// `JS_CLASS_WEAKSET`: ordered non-owning identities with no tombstones.
    WeakSet {
        records: WeakCollectionRecords<()>,
    },
    /// `JS_CLASS_WEAK_REF`: one non-owning generational target identity. A
    /// dead target is cleared by the ordered weak-object pass, never traced as
    /// an arena edge.
    WeakRef {
        target: Option<WeakCollectionKey>,
    },
    /// `JS_CLASS_FINALIZATION_REGISTRY`: weak registration target/token
    /// identities plus strong callback, creation-realm, and held-value edges.
    /// Entries remain ordered until unregister or finalization preparation.
    FinalizationRegistry(FinalizationRegistryData),
    /// Realm global object and its hidden table of unresolved global VarRefs.
    GlobalObject {
        uninitialized_vars: ObjectId,
    },
    Error,
    /// `%StringIteratorPrototype%` instances own the iterated UTF-16 string
    /// and the next code-unit index.  The string is reference counted outside
    /// the GC arena, so this payload adds no arena edge while still keeping
    /// lone surrogates and rope-backed strings exact.
    StringIterator {
        string: Option<JsString>,
        next_index: usize,
    },
    /// `JS_CLASS_ITERATOR_HELPER`: lazy helper state with independently owned
    /// source, cached-next, callback, and optional inner-iterator edges.
    /// Completion changes only `done`; QuickJS retains all four values until
    /// the helper object is finalized.
    IteratorHelper(IteratorHelperData),
    /// `JS_CLASS_ITERATOR_WRAP`: source iterator and cached `next` method
    /// retained by the wrapper returned from `Iterator.from`.
    IteratorWrap(IteratorWrapData),
    /// `JS_CLASS_ASYNC_FROM_SYNC_ITERATOR`: synchronous source iterator and
    /// cached `next`, exposed only through Promise-returning adapter methods.
    AsyncFromSyncIterator(AsyncFromSyncIteratorData),
    /// `JS_CLASS_ITERATOR_CONCAT`: remaining iterable/method pairs plus the
    /// lazily created current iterator and cached `next` method.
    IteratorConcat(IteratorConcatData),
    /// QuickJS `JS_CLASS_PROXY`. A Proxy has no ordinary prototype of its own;
    /// every observable internal method dispatches through this payload.
    Proxy(ProxyData),
    /// `JS_CLASS_ARRAY_BUFFER`. Backing bytes are non-GC memory, so this
    /// payload introduces no arena edge. TypedArray/DataView objects retain
    /// the owning ArrayBuffer object in their own payloads.
    ArrayBuffer(ArrayBufferData),
    /// `JS_CLASS_SHARED_ARRAY_BUFFER`. The wrapper-local handle is a GC leaf;
    /// its `Arc` backing may outlive this heap object or be sent to a worker.
    SharedArrayBuffer(SharedArrayBufferData),
    /// `JS_CLASS_DATAVIEW`. The backing ArrayBuffer-family object is a strong arena edge;
    /// detached and currently out-of-bounds views retain this structural
    /// payload and become observable errors only when accessed.
    DataView(ArrayBufferViewData),
    /// One of QuickJS's twelve fast integer-indexed TypedArray classes.
    ///
    /// Element bytes remain owned by the branded ArrayBuffer-family object. The durable view
    /// metadata survives detach and resizable-buffer OOB transitions so a view
    /// can recover when its backing store grows into range again.
    TypedArray(TypedArrayData),
    NativeFunction {
        data: NativeFunctionData,
        internal: Option<InternalCallableData>,
    },
    /// QuickJS `JSBoundFunction`: the target, bound receiver and each bound
    /// argument are independently owned edges of the function object.
    BoundFunction {
        target: ObjectId,
        this_value: RawValue,
        arguments: Rc<[RawValue]>,
    },
    BytecodeFunction {
        bytecode: FunctionBytecodeId,
        home_object: Option<ObjectId>,
        /// Hidden instance-field initializer owned by a class constructor.
        /// The edge is deliberately internal: authored code cannot forge or
        /// overwrite QuickJS's `<class_fields_init>` binding.
        class_instance_initializer: Option<ObjectId>,
        /// One-shot guard for the aggregate static-elements program. Authored
        /// loops create a fresh constructor and therefore a fresh guard; forged
        /// bytecode cannot replay static initialization on the same class.
        class_static_initializer_started: bool,
        /// One owned reference per bytecode closure slot, matching QuickJS's
        /// `JSObject.u.func.var_refs[]` ownership.
        closure_slots: Vec<VarRefId>,
    },
    /// `JS_CLASS_GENERATOR`: the branded result object owns the complete
    /// dormant frame while suspended. `Executing` temporarily moves that
    /// activation into rooted Rust values so reentrant calls can observe the
    /// state without creating an invisible side-table root.
    Generator {
        state: GeneratorState,
        activation: Option<Box<GeneratorActivationData>>,
    },
    /// `JS_CLASS_ASYNC_GENERATOR`: a dormant resumable frame plus the FIFO
    /// request queue whose Promise capabilities serialize public resumes.
    AsyncGenerator(AsyncGeneratorData),
    /// Hidden GC-visible async-function driver shared by its pending `await`
    /// reactions. It is never exposed to authored ECMAScript code.
    AsyncFunctionState(AsyncFunctionStateData),
    /// `JS_CLASS_PROMISE`: settlement state, result, and pending reactions are
    /// traced directly in the arena rather than hidden in a runtime side map.
    Promise(PromiseData),
}

/// Object storage category.  Additional QuickJS classes will extend this enum
/// while retaining the same arena and collection protocol.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ObjectKind {
    Ordinary,
    /// `JS_CLASS_MODULE_NS`: null-prototype, non-extensible live export view.
    ModuleNamespace,
    /// `JS_CLASS_ITERATOR`: ordinary internal methods with a distinct class
    /// tag for direct subclasses of the abstract Iterator constructor.
    Iterator,
    Array,
    Arguments,
    ArrayIterator,
    ForInIterator,
    Primitive,
    Date,
    RegExp,
    RegExpStringIterator,
    Map,
    MapIterator,
    Set,
    SetIterator,
    WeakMap,
    WeakSet,
    WeakRef,
    FinalizationRegistry,
    GlobalObject,
    Error,
    StringIterator,
    IteratorHelper,
    IteratorWrap,
    AsyncFromSyncIterator,
    IteratorConcat,
    Proxy,
    ArrayBuffer,
    SharedArrayBuffer,
    DataView,
    TypedArray,
    NativeFunction,
    BoundFunction,
    BytecodeFunction,
    Generator,
    AsyncGenerator,
    AsyncFunctionState,
    Promise,
}

/// One string-key entry captured by QuickJS's `JS_GPN_SET_ENUM` enumeration.
/// `JsString` avoids storing runtime-owning `PropertyKey` roots in the heap.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForInProperty {
    pub name: JsString,
    pub enumerable: bool,
}

/// Mutable state of one hidden for-in enumeration object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForInIteratorData {
    pub object: Option<ObjectId>,
    pub index: usize,
    pub properties: Vec<ForInProperty>,
    /// The iterator, not the source object, remembers whether QuickJS selected
    /// its count-only fast-Array path at loop entry.
    pub fast_array: bool,
    pub array_count: u32,
    pub in_prototype_chain: bool,
    pub visited: HashSet<JsString>,
}

/// One non-observable step selected from a hidden for-in iterator. The
/// runtime performs live property/prototype operations only after the heap
/// borrow used to advance the cursor has ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ForInCandidate {
    Done,
    BaseComplete { object: ObjectId, fast_array: bool },
    LevelComplete(ObjectId),
    ArrayIndex { object: ObjectId, index: u32 },
    Property { object: ObjectId, name: JsString },
}

/// Runtime-owned ordinary object record.
///
/// The shape entries and slots are parallel arrays and must have identical
/// lengths and storage kinds.  Allocation validates that invariant.
#[derive(Clone, Debug, PartialEq)]
pub struct ObjectData {
    pub shape: ShapeId,
    pub slots: Vec<PropertySlot>,
    /// QuickJS's hidden `JS_CLASS_PRIVATE` brand stored on a private method's
    /// HomeObject. The object owns one atom reference independently from any
    /// receiver marker using the same private atom in its shape.
    pub private_brand_home: Option<Atom>,
    /// QuickJS's identity-local Annex B `is_HTMLDDA` bit. This is object
    /// metadata rather than a callable kind: `JS_SetIsHTMLDDA` can mark any
    /// object, although the pinned Test262 host marks one native function.
    pub is_html_dda: bool,
    pub extensible: bool,
    pub immutable_prototype: bool,
    pub is_constructor: bool,
    pub kind: ObjectKind,
    pub payload: ObjectPayload,
}

impl ObjectData {
    /// Construct an ordinary extensible object with a mutable prototype.
    #[must_use]
    pub const fn ordinary(shape: ShapeId, slots: Vec<PropertySlot>) -> Self {
        Self {
            shape,
            slots,
            private_brand_home: None,
            is_html_dda: false,
            extensible: true,
            immutable_prototype: false,
            is_constructor: false,
            kind: ObjectKind::Ordinary,
            payload: ObjectPayload::Ordinary,
        }
    }

    /// Construct one `JS_CLASS_ITERATOR` object. It deliberately shares the
    /// ordinary payload and internal methods; only the QuickJS class tag is
    /// distinct.
    #[must_use]
    pub const fn iterator(shape: ShapeId, slots: Vec<PropertySlot>) -> Self {
        Self {
            shape,
            slots,
            private_brand_home: None,
            is_html_dda: false,
            extensible: true,
            immutable_prototype: false,
            is_constructor: false,
            kind: ObjectKind::Iterator,
            payload: ObjectPayload::Ordinary,
        }
    }

    /// Construct one `JS_CLASS_MODULE_NS` exotic object.
    ///
    /// Export bindings remain ordinary `PropertySlot::VarRef` edges, while
    /// the distinct class marker selects the namespace-only internal methods.
    /// The caller supplies a null-prototype shape and installs the complete
    /// sorted export table through the runtime's private construction path.
    #[must_use]
    pub const fn module_namespace(shape: ShapeId, slots: Vec<PropertySlot>) -> Self {
        Self {
            shape,
            slots,
            private_brand_home: None,
            is_html_dda: false,
            extensible: false,
            immutable_prototype: false,
            is_constructor: false,
            kind: ObjectKind::ModuleNamespace,
            payload: ObjectPayload::Ordinary,
        }
    }

    /// Construct one Raw JSON branded object with ordinary internal methods.
    #[must_use]
    pub const fn raw_json(shape: ShapeId, slots: Vec<PropertySlot>) -> Self {
        Self {
            shape,
            slots,
            private_brand_home: None,
            is_html_dda: false,
            extensible: true,
            immutable_prototype: false,
            is_constructor: false,
            kind: ObjectKind::Ordinary,
            payload: ObjectPayload::RawJson,
        }
    }

    /// Construct one genuine Array exotic object. The caller supplies the
    /// validated `length`-first layout used by QuickJS's initial Array shape.
    #[must_use]
    pub const fn array(shape: ShapeId, slots: Vec<PropertySlot>) -> Self {
        Self {
            shape,
            slots,
            private_brand_home: None,
            is_html_dda: false,
            extensible: true,
            immutable_prototype: false,
            is_constructor: false,
            kind: ObjectKind::Array,
            payload: ObjectPayload::Array {
                dense: Some(Vec::new()),
            },
        }
    }

    /// Construct one mapped or unmapped Arguments exotic object. The caller
    /// installs the exact actual-argument prefix and the class-specific
    /// `length`, `callee`, and `@@iterator` properties after allocation.
    #[must_use]
    pub const fn arguments(
        shape: ShapeId,
        slots: Vec<PropertySlot>,
        mapped: bool,
        fast_len: u32,
    ) -> Self {
        Self {
            shape,
            slots,
            private_brand_home: None,
            is_html_dda: false,
            extensible: true,
            immutable_prototype: false,
            is_constructor: false,
            kind: ObjectKind::Arguments,
            payload: ObjectPayload::Arguments {
                mapped,
                fast_len: Some(fast_len),
            },
        }
    }

    /// Construct a branded Array Iterator at index zero.
    #[must_use]
    pub const fn array_iterator(
        shape: ShapeId,
        slots: Vec<PropertySlot>,
        object: ObjectId,
        kind: ArrayIteratorKind,
    ) -> Self {
        Self {
            shape,
            slots,
            private_brand_home: None,
            is_html_dda: false,
            extensible: true,
            immutable_prototype: false,
            is_constructor: false,
            kind: ObjectKind::ArrayIterator,
            payload: ObjectPayload::ArrayIterator {
                object: Some(object),
                next_index: 0,
                kind,
            },
        }
    }

    /// Construct one hidden QuickJS-compatible for-in enumeration object.
    #[must_use]
    pub const fn for_in_iterator(
        shape: ShapeId,
        slots: Vec<PropertySlot>,
        data: ForInIteratorData,
    ) -> Self {
        Self {
            shape,
            slots,
            private_brand_home: None,
            is_html_dda: false,
            extensible: true,
            immutable_prototype: false,
            is_constructor: false,
            kind: ObjectKind::ForInIterator,
            payload: ObjectPayload::ForInIterator(data),
        }
    }

    /// Construct one extensible primitive wrapper object with its validated
    /// internal primitive data slot.
    #[must_use]
    pub const fn primitive(
        shape: ShapeId,
        slots: Vec<PropertySlot>,
        data: PrimitiveObjectData,
    ) -> Self {
        Self {
            shape,
            slots,
            private_brand_home: None,
            is_html_dda: false,
            extensible: true,
            immutable_prototype: false,
            is_constructor: false,
            kind: ObjectKind::Primitive,
            payload: ObjectPayload::Primitive(data),
        }
    }

    /// Construct one genuine Date object with an internal millisecond value.
    /// The runtime is responsible for applying TimeClip before publication;
    /// NaN remains valid because it represents an invalid Date.
    #[must_use]
    pub const fn date(shape: ShapeId, slots: Vec<PropertySlot>, value: f64) -> Self {
        Self {
            shape,
            slots,
            private_brand_home: None,
            is_html_dda: false,
            extensible: true,
            immutable_prototype: false,
            is_constructor: false,
            kind: ObjectKind::Date,
            payload: ObjectPayload::Date(value),
        }
    }

    /// Construct a branded RegExp object before its pattern is compiled.
    /// This mirrors QuickJS's derived-constructor order, in which object
    /// allocation can succeed before compilation reports a SyntaxError.
    #[must_use]
    pub const fn regexp(shape: ShapeId, slots: Vec<PropertySlot>) -> Self {
        Self {
            shape,
            slots,
            private_brand_home: None,
            is_html_dda: false,
            extensible: true,
            immutable_prototype: false,
            is_constructor: false,
            kind: ObjectKind::RegExp,
            payload: ObjectPayload::RegExp(RegExpObjectData::Uninitialized),
        }
    }

    /// Construct a branded RegExp object whose program is already compiled.
    #[must_use]
    pub fn compiled_regexp(
        shape: ShapeId,
        slots: Vec<PropertySlot>,
        pattern: JsString,
        program: Rc<CompiledRegExp>,
    ) -> Self {
        Self {
            shape,
            slots,
            private_brand_home: None,
            is_html_dda: false,
            extensible: true,
            immutable_prototype: false,
            is_constructor: false,
            kind: ObjectKind::RegExp,
            payload: ObjectPayload::RegExp(RegExpObjectData::Compiled { pattern, program }),
        }
    }

    /// Construct a branded RegExp String Iterator over one species-created
    /// matcher. The matcher and input string remain retained after completion;
    /// only finalization releases them in pinned QuickJS.
    #[must_use]
    pub const fn regexp_string_iterator(
        shape: ShapeId,
        slots: Vec<PropertySlot>,
        regexp: ObjectId,
        string: JsString,
        global: bool,
        full_unicode: bool,
    ) -> Self {
        Self {
            shape,
            slots,
            private_brand_home: None,
            is_html_dda: false,
            extensible: true,
            immutable_prototype: false,
            is_constructor: false,
            kind: ObjectKind::RegExpStringIterator,
            payload: ObjectPayload::RegExpStringIterator {
                regexp,
                string,
                global,
                full_unicode,
                done: false,
            },
        }
    }

    /// Construct one empty genuine Map object. Stable records are appended by
    /// [`Heap::map_insert_record`] after key equality has been resolved by the
    /// runtime's SameValueZero logic.
    #[must_use]
    pub const fn map(shape: ShapeId, slots: Vec<PropertySlot>) -> Self {
        Self {
            shape,
            slots,
            private_brand_home: None,
            is_html_dda: false,
            extensible: true,
            immutable_prototype: false,
            is_constructor: false,
            kind: ObjectKind::Map,
            payload: ObjectPayload::Map {
                records: Vec::new(),
                live_indices: BTreeSet::new(),
                size: 0,
            },
        }
    }

    /// Construct a branded Map Iterator at stable record index zero.
    #[must_use]
    pub const fn map_iterator(
        shape: ShapeId,
        slots: Vec<PropertySlot>,
        object: ObjectId,
        kind: MapIteratorKind,
    ) -> Self {
        Self {
            shape,
            slots,
            private_brand_home: None,
            is_html_dda: false,
            extensible: true,
            immutable_prototype: false,
            is_constructor: false,
            kind: ObjectKind::MapIterator,
            payload: ObjectPayload::MapIterator {
                object: Some(object),
                next_index: 0,
                current_index: None,
                kind,
            },
        }
    }

    /// Construct one empty genuine Set object. Stable records are appended by
    /// [`Heap::set_insert_record`] after the runtime resolves SameValueZero
    /// equality. The shared record value slot remains `undefined`.
    #[must_use]
    pub const fn set(shape: ShapeId, slots: Vec<PropertySlot>) -> Self {
        Self {
            shape,
            slots,
            private_brand_home: None,
            is_html_dda: false,
            extensible: true,
            immutable_prototype: false,
            is_constructor: false,
            kind: ObjectKind::Set,
            payload: ObjectPayload::Set {
                records: Vec::new(),
                live_indices: BTreeSet::new(),
                size: 0,
            },
        }
    }

    /// Construct one empty genuine WeakMap. Record keys are weak identities;
    /// values inserted later through [`Heap::weak_map_set`] retain
    /// their ordinary object and Symbol ownership.
    #[must_use]
    pub fn weak_map(shape: ShapeId, slots: Vec<PropertySlot>) -> Self {
        Self {
            shape,
            slots,
            private_brand_home: None,
            is_html_dda: false,
            extensible: true,
            immutable_prototype: false,
            is_constructor: false,
            kind: ObjectKind::WeakMap,
            payload: ObjectPayload::WeakMap {
                records: WeakCollectionRecords::new(),
            },
        }
    }

    /// Construct one empty genuine WeakSet.
    #[must_use]
    pub fn weak_set(shape: ShapeId, slots: Vec<PropertySlot>) -> Self {
        Self {
            shape,
            slots,
            private_brand_home: None,
            is_html_dda: false,
            extensible: true,
            immutable_prototype: false,
            is_constructor: false,
            kind: ObjectKind::WeakSet,
            payload: ObjectPayload::WeakSet {
                records: WeakCollectionRecords::new(),
            },
        }
    }

    /// Construct one heap-internal genuine WeakRef. The runtime intrinsic
    /// layer supplies the public constructor and selected prototype.
    #[must_use]
    pub(crate) const fn weak_ref(
        shape: ShapeId,
        slots: Vec<PropertySlot>,
        target: WeakCollectionKey,
    ) -> Self {
        Self {
            shape,
            slots,
            private_brand_home: None,
            is_html_dda: false,
            extensible: true,
            immutable_prototype: false,
            is_constructor: false,
            kind: ObjectKind::WeakRef,
            payload: ObjectPayload::WeakRef {
                target: Some(target),
            },
        }
    }

    /// Construct one heap-internal genuine FinalizationRegistry. Its callback
    /// and creation realm are ordinary traced payload edges.
    #[must_use]
    pub(crate) const fn finalization_registry(
        shape: ShapeId,
        slots: Vec<PropertySlot>,
        callback: ObjectId,
        realm: ContextId,
    ) -> Self {
        Self {
            shape,
            slots,
            private_brand_home: None,
            is_html_dda: false,
            extensible: true,
            immutable_prototype: false,
            is_constructor: false,
            kind: ObjectKind::FinalizationRegistry,
            payload: ObjectPayload::FinalizationRegistry(FinalizationRegistryData {
                callback,
                realm,
                entries: Vec::new(),
            }),
        }
    }

    /// Construct a branded Set Iterator at stable record index zero.
    #[must_use]
    pub const fn set_iterator(
        shape: ShapeId,
        slots: Vec<PropertySlot>,
        object: ObjectId,
        kind: SetIteratorKind,
    ) -> Self {
        Self {
            shape,
            slots,
            private_brand_home: None,
            is_html_dda: false,
            extensible: true,
            immutable_prototype: false,
            is_constructor: false,
            kind: ObjectKind::SetIterator,
            payload: ObjectPayload::SetIterator {
                object: Some(object),
                next_index: 0,
                current_index: None,
                kind,
            },
        }
    }

    /// Construct a realm global object with QuickJS's hidden unresolved-name
    /// VarRef table.
    #[must_use]
    pub const fn global_object(
        shape: ShapeId,
        slots: Vec<PropertySlot>,
        uninitialized_vars: ObjectId,
    ) -> Self {
        Self {
            shape,
            slots,
            private_brand_home: None,
            is_html_dda: false,
            extensible: true,
            immutable_prototype: false,
            is_constructor: false,
            kind: ObjectKind::GlobalObject,
            payload: ObjectPayload::GlobalObject { uninitialized_vars },
        }
    }

    /// Construct an Error-class object. Its ordinary `name`/`message`
    /// properties remain in the shape/slot arrays; the payload preserves the
    /// native class tag used by `Error.isError`.
    #[must_use]
    pub const fn error(shape: ShapeId, slots: Vec<PropertySlot>) -> Self {
        Self {
            shape,
            slots,
            private_brand_home: None,
            is_html_dda: false,
            extensible: true,
            immutable_prototype: false,
            is_constructor: false,
            kind: ObjectKind::Error,
            payload: ObjectPayload::Error,
        }
    }

    /// Construct a branded String Iterator at code-unit index zero.
    #[must_use]
    pub const fn string_iterator(
        shape: ShapeId,
        slots: Vec<PropertySlot>,
        string: JsString,
    ) -> Self {
        Self {
            shape,
            slots,
            private_brand_home: None,
            is_html_dda: false,
            extensible: true,
            immutable_prototype: false,
            is_constructor: false,
            kind: ObjectKind::StringIterator,
            payload: ObjectPayload::StringIterator {
                string: Some(string),
                next_index: 0,
            },
        }
    }

    /// Construct one lazy synchronous Iterator Helper.
    ///
    /// Runtime creation passes `inner: None` (the internal `undefined` state);
    /// `flatMap` later replaces it while traversing a mapped iterator. All
    /// supplied edges transfer to the object when allocation succeeds.
    #[must_use]
    pub const fn iterator_helper(
        shape: ShapeId,
        slots: Vec<PropertySlot>,
        data: IteratorHelperData,
    ) -> Self {
        Self {
            shape,
            slots,
            private_brand_home: None,
            is_html_dda: false,
            extensible: true,
            immutable_prototype: false,
            is_constructor: false,
            kind: ObjectKind::IteratorHelper,
            payload: ObjectPayload::IteratorHelper(data),
        }
    }

    /// Construct the branded forwarding iterator used by `Iterator.from`.
    #[must_use]
    pub const fn iterator_wrap(
        shape: ShapeId,
        slots: Vec<PropertySlot>,
        source: RawValue,
        next: RawValue,
    ) -> Self {
        Self {
            shape,
            slots,
            private_brand_home: None,
            is_html_dda: false,
            extensible: true,
            immutable_prototype: false,
            is_constructor: false,
            kind: ObjectKind::IteratorWrap,
            payload: ObjectPayload::IteratorWrap(IteratorWrapData { source, next }),
        }
    }

    /// Construct the branded Promise adapter used by async iteration over a
    /// synchronous iterator.
    #[must_use]
    pub const fn async_from_sync_iterator(
        shape: ShapeId,
        slots: Vec<PropertySlot>,
        sync_iterator: ObjectId,
        next: RawValue,
    ) -> Self {
        Self {
            shape,
            slots,
            private_brand_home: None,
            is_html_dda: false,
            extensible: true,
            immutable_prototype: false,
            is_constructor: false,
            kind: ObjectKind::AsyncFromSyncIterator,
            payload: ObjectPayload::AsyncFromSyncIterator(AsyncFromSyncIteratorData {
                sync_iterator,
                next,
            }),
        }
    }

    /// Construct the lazy sequencing iterator returned by `Iterator.concat`.
    #[must_use]
    pub const fn iterator_concat(
        shape: ShapeId,
        slots: Vec<PropertySlot>,
        items: Vec<Option<IteratorConcatItem>>,
    ) -> Self {
        Self {
            shape,
            slots,
            private_brand_home: None,
            is_html_dda: false,
            extensible: true,
            immutable_prototype: false,
            is_constructor: false,
            kind: ObjectKind::IteratorConcat,
            payload: ObjectPayload::IteratorConcat(IteratorConcatData {
                items,
                index: 0,
                iterator: None,
                next: RawValue::Undefined,
                running: false,
            }),
        }
    }

    /// Construct one genuine Proxy with a null ordinary prototype.
    ///
    /// `is_constructor` is copied from the target at creation time, just as
    /// QuickJS sets the Proxy object's constructor bit independently from its
    /// callable class hook.
    #[must_use]
    pub const fn proxy(
        shape: ShapeId,
        slots: Vec<PropertySlot>,
        target: ObjectId,
        handler: ObjectId,
        is_callable: bool,
        is_constructor: bool,
    ) -> Self {
        Self {
            shape,
            slots,
            private_brand_home: None,
            is_html_dda: false,
            extensible: true,
            immutable_prototype: false,
            is_constructor,
            kind: ObjectKind::Proxy,
            payload: ObjectPayload::Proxy(ProxyData {
                target,
                handler,
                is_callable,
                is_revoked: false,
            }),
        }
    }

    /// Construct one attached ArrayBuffer by transferring an existing byte
    /// vector. The runtime validates the vector length and maximum before
    /// entering this allocation boundary.
    #[must_use]
    pub fn array_buffer_from_bytes(
        shape: ShapeId,
        slots: Vec<PropertySlot>,
        bytes: Vec<u8>,
        max_byte_length: Option<u32>,
    ) -> Self {
        Self {
            shape,
            slots,
            private_brand_home: None,
            is_html_dda: false,
            extensible: true,
            immutable_prototype: false,
            is_constructor: false,
            kind: ObjectKind::ArrayBuffer,
            payload: ObjectPayload::ArrayBuffer(ArrayBufferData {
                bytes,
                max_byte_length,
                detached: false,
            }),
        }
    }

    /// Construct one genuine SharedArrayBuffer wrapper around a safe shared
    /// backing handle. Cloned handles share bytes while retaining independent
    /// wrapper-local length metadata.
    #[must_use]
    pub fn shared_array_buffer(
        shape: ShapeId,
        slots: Vec<PropertySlot>,
        handle: SharedBufferHandle,
    ) -> Self {
        Self {
            shape,
            slots,
            private_brand_home: None,
            is_html_dda: false,
            extensible: true,
            immutable_prototype: false,
            is_constructor: false,
            kind: ObjectKind::SharedArrayBuffer,
            payload: ObjectPayload::SharedArrayBuffer(SharedArrayBufferData { handle }),
        }
    }

    /// Construct one genuine DataView over an ArrayBuffer.
    ///
    /// The heap validates only the durable structural layout here. Detached
    /// and currently out-of-bounds states remain valid so later resize/detach
    /// operations never corrupt the object graph.
    #[must_use]
    pub const fn data_view(
        shape: ShapeId,
        slots: Vec<PropertySlot>,
        data: ArrayBufferViewData,
    ) -> Self {
        Self {
            shape,
            slots,
            private_brand_home: None,
            is_html_dda: false,
            extensible: true,
            immutable_prototype: false,
            is_constructor: false,
            kind: ObjectKind::DataView,
            payload: ObjectPayload::DataView(data),
        }
    }

    /// Construct one genuine integer-indexed TypedArray over an ArrayBuffer.
    #[must_use]
    pub const fn typed_array(
        shape: ShapeId,
        slots: Vec<PropertySlot>,
        data: TypedArrayData,
    ) -> Self {
        Self {
            shape,
            slots,
            private_brand_home: None,
            is_html_dda: false,
            extensible: true,
            immutable_prototype: false,
            is_constructor: false,
            kind: ObjectKind::TypedArray,
            payload: ObjectPayload::TypedArray(data),
        }
    }

    /// Construct a non-constructable runtime-provided function object.
    #[must_use]
    pub(crate) const fn native_function(
        shape: ShapeId,
        slots: Vec<PropertySlot>,
        target: NativeFunctionId,
        min_readable_args: u8,
    ) -> Self {
        Self {
            shape,
            slots,
            private_brand_home: None,
            is_html_dda: false,
            extensible: true,
            immutable_prototype: false,
            is_constructor: target.descriptor().cproto.default_is_constructor(),
            kind: ObjectKind::NativeFunction,
            payload: ObjectPayload::NativeFunction {
                data: NativeFunctionData {
                    target,
                    realm: None,
                    min_readable_args,
                },
                internal: None,
            },
        }
    }

    /// Construct a native callable whose defining realm is already live.
    #[must_use]
    pub(crate) const fn bound_native_function(
        shape: ShapeId,
        slots: Vec<PropertySlot>,
        target: NativeFunctionId,
        realm: ContextId,
        min_readable_args: u8,
    ) -> Self {
        Self {
            shape,
            slots,
            private_brand_home: None,
            is_html_dda: false,
            extensible: true,
            immutable_prototype: false,
            is_constructor: target.descriptor().cproto.default_is_constructor(),
            kind: ObjectKind::NativeFunction,
            payload: ObjectPayload::NativeFunction {
                data: NativeFunctionData {
                    target,
                    realm: Some(realm),
                    min_readable_args,
                },
                internal: None,
            },
        }
    }

    /// Construct a realm-bound internal native callable with typed hidden
    /// capture data.  Allocation retains every raw edge in `internal`; the
    /// caller transfers no public runtime-owning wrapper into the heap.
    #[must_use]
    pub(crate) const fn bound_internal_native_function(
        shape: ShapeId,
        slots: Vec<PropertySlot>,
        target: NativeFunctionId,
        realm: ContextId,
        min_readable_args: u8,
        internal: InternalCallableData,
    ) -> Self {
        Self {
            shape,
            slots,
            private_brand_home: None,
            is_html_dda: false,
            extensible: true,
            immutable_prototype: false,
            is_constructor: false,
            kind: ObjectKind::NativeFunction,
            payload: ObjectPayload::NativeFunction {
                data: NativeFunctionData {
                    target,
                    realm: Some(realm),
                    min_readable_args,
                },
                internal: Some(internal),
            },
        }
    }

    /// Construct a QuickJS-style bound function. Its ordinary `length` and
    /// `name` properties are installed by the runtime after allocation; the
    /// class payload owns the target, bound receiver and argument vector.
    #[must_use]
    pub(crate) const fn bound_function(
        shape: ShapeId,
        slots: Vec<PropertySlot>,
        target: ObjectId,
        this_value: RawValue,
        arguments: Rc<[RawValue]>,
        is_constructor: bool,
    ) -> Self {
        Self {
            shape,
            slots,
            private_brand_home: None,
            is_html_dda: false,
            extensible: true,
            immutable_prototype: false,
            is_constructor,
            kind: ObjectKind::BoundFunction,
            payload: ObjectPayload::BoundFunction {
                target,
                this_value,
                arguments,
            },
        }
    }

    /// Construct an ordinary bytecode-function object.
    #[must_use]
    pub const fn bytecode_function(
        shape: ShapeId,
        slots: Vec<PropertySlot>,
        bytecode: FunctionBytecodeId,
        home_object: Option<ObjectId>,
        is_constructor: bool,
    ) -> Self {
        Self {
            shape,
            slots,
            private_brand_home: None,
            is_html_dda: false,
            extensible: true,
            immutable_prototype: false,
            is_constructor,
            kind: ObjectKind::BytecodeFunction,
            payload: ObjectPayload::BytecodeFunction {
                bytecode,
                home_object,
                class_instance_initializer: None,
                class_static_initializer_started: false,
                closure_slots: Vec::new(),
            },
        }
    }

    /// Construct a bytecode-function object whose closure slots own the given
    /// captured-variable cells. Repeated identities are intentional: each
    /// slot contributes one strong reference, as in QuickJS.
    #[must_use]
    pub const fn bytecode_function_with_closures(
        shape: ShapeId,
        slots: Vec<PropertySlot>,
        bytecode: FunctionBytecodeId,
        home_object: Option<ObjectId>,
        closure_slots: Vec<VarRefId>,
        is_constructor: bool,
    ) -> Self {
        Self {
            shape,
            slots,
            private_brand_home: None,
            is_html_dda: false,
            extensible: true,
            immutable_prototype: false,
            is_constructor,
            kind: ObjectKind::BytecodeFunction,
            payload: ObjectPayload::BytecodeFunction {
                bytecode,
                home_object,
                class_instance_initializer: None,
                class_static_initializer_started: false,
                closure_slots,
            },
        }
    }

    /// Construct a branded synchronous generator in its initial suspended
    /// state. The complete activation is retained as heap-visible raw edges.
    #[must_use]
    pub fn generator(
        shape: ShapeId,
        slots: Vec<PropertySlot>,
        activation: GeneratorActivationData,
    ) -> Self {
        Self {
            shape,
            slots,
            private_brand_home: None,
            is_html_dda: false,
            extensible: true,
            immutable_prototype: false,
            is_constructor: false,
            kind: ObjectKind::Generator,
            payload: ObjectPayload::Generator {
                state: GeneratorState::SuspendedStart,
                activation: Some(Box::new(activation)),
            },
        }
    }

    /// Construct a branded async generator in its initial suspended state.
    #[must_use]
    pub fn async_generator(
        shape: ShapeId,
        slots: Vec<PropertySlot>,
        activation: GeneratorActivationData,
    ) -> Self {
        Self {
            shape,
            slots,
            private_brand_home: None,
            is_html_dda: false,
            extensible: true,
            immutable_prototype: false,
            is_constructor: false,
            kind: ObjectKind::AsyncGenerator,
            payload: ObjectPayload::AsyncGenerator(AsyncGeneratorData {
                state: AsyncGeneratorState::SuspendedStart,
                activation: Some(Box::new(activation)),
                queue: VecDeque::new(),
                resume_realm: None,
            }),
        }
    }

    /// Construct one hidden async-function driver in its initial executing
    /// phase. The runtime roots the active frame until the first suspension;
    /// the state object owns the outer resolving functions immediately.
    #[must_use]
    pub const fn async_function_state(
        shape: ShapeId,
        slots: Vec<PropertySlot>,
        driver_realm: ContextId,
        outer_resolve: ObjectId,
        outer_reject: ObjectId,
    ) -> Self {
        Self {
            shape,
            slots,
            private_brand_home: None,
            is_html_dda: false,
            extensible: true,
            immutable_prototype: false,
            is_constructor: false,
            kind: ObjectKind::AsyncFunctionState,
            payload: ObjectPayload::AsyncFunctionState(AsyncFunctionStateData {
                driver_realm,
                outer_resolve,
                outer_reject,
                activation: None,
                phase: AsyncFunctionPhase::Executing,
            }),
        }
    }

    /// Construct one genuine pending Promise.  Its result slot starts at
    /// `undefined` and owns no reactions until `PerformPromiseThen` appends
    /// them through `Heap::promise_add_reactions`.
    #[must_use]
    pub const fn promise(shape: ShapeId, slots: Vec<PropertySlot>) -> Self {
        Self {
            shape,
            slots,
            private_brand_home: None,
            is_html_dda: false,
            extensible: true,
            immutable_prototype: false,
            is_constructor: false,
            kind: ObjectKind::Promise,
            payload: ObjectPayload::Promise(PromiseData {
                state: PromiseState::Pending,
                result: RawValue::Undefined,
                fulfill_reactions: Vec::new(),
                reject_reactions: Vec::new(),
                is_handled: false,
            }),
        }
    }
}

/// Allocation-complete plan for shortening one fast Array prefix.
///
/// Preparing the plan does not mutate the Array. Once prepared, committing it
/// moves the removed values into already-reserved storage before detaching
/// their heap and atom ownership, so the representation change itself cannot
/// fail because of a container allocation.
pub(crate) struct PreparedArrayDenseTruncation {
    object: ObjectId,
    original_len: usize,
    new_len: usize,
    removed_atom_count: usize,
    removed: Vec<RawValue>,
    cleanup: HeapCleanup,
}

/// Current arena population, split by lifecycle state and node kind.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HeapCounts {
    pub object_nodes: usize,
    pub shape_nodes: usize,
    pub var_ref_nodes: usize,
    pub context_nodes: usize,
    pub function_bytecode_nodes: usize,
    pub initializing: usize,
    pub live: usize,
    pub zero_queued: usize,
    pub finalizing: usize,
    pub zombies: usize,
    pub vacant: usize,
    pub retired: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum RawId {
    Object(ObjectId),
    Shape(ShapeId),
    VarRef(VarRefId),
    Context(ContextId),
    FunctionBytecode(FunctionBytecodeId),
}

impl RawId {
    const fn kind(self) -> HeapNodeKind {
        match self {
            Self::Object(_) => HeapNodeKind::Object,
            Self::Shape(_) => HeapNodeKind::Shape,
            Self::VarRef(_) => HeapNodeKind::VarRef,
            Self::Context(_) => HeapNodeKind::Context,
            Self::FunctionBytecode(_) => HeapNodeKind::FunctionBytecode,
        }
    }

    const fn index(self) -> u32 {
        match self {
            Self::Object(id) => id.index,
            Self::Shape(id) => id.index,
            Self::VarRef(id) => id.index,
            Self::Context(id) => id.index,
            Self::FunctionBytecode(id) => id.index,
        }
    }

    const fn generation(self) -> u32 {
        match self {
            Self::Object(id) => id.generation,
            Self::Shape(id) => id.generation,
            Self::VarRef(id) => id.generation,
            Self::Context(id) => id.generation,
            Self::FunctionBytecode(id) => id.generation,
        }
    }
}

// Context nodes intentionally stay inline in the generational arena: boxing
// only this variant would add a second allocator/failure boundary to realm
// publication and collection without shrinking any live Context graph.
#[allow(clippy::large_enum_variant)]
enum NodeData {
    Object(ObjectData),
    Shape(Shape),
    VarRef(VarRefData),
    Context(ContextData),
    FunctionBytecode(FunctionBytecodeData),
}

impl NodeData {
    const fn kind(&self) -> HeapNodeKind {
        match self {
            Self::Object(_) => HeapNodeKind::Object,
            Self::Shape(_) => HeapNodeKind::Shape,
            Self::VarRef(_) => HeapNodeKind::VarRef,
            Self::Context(_) => HeapNodeKind::Context,
            Self::FunctionBytecode(_) => HeapNodeKind::FunctionBytecode,
        }
    }

    fn edges(&self) -> Vec<RawId> {
        match self {
            Self::Object(object) => object_edges(object),
            Self::Shape(shape) => shape_edges(shape),
            Self::VarRef(var_ref) => var_ref_edges(var_ref),
            Self::Context(context) => context_edges(context),
            Self::FunctionBytecode(bytecode) => function_bytecode_edges(bytecode),
        }
    }
}

struct Node {
    strong: u32,
    data: NodeData,
}

enum SlotState {
    Initializing { kind: HeapNodeKind, strong: u32 },
    Live(Node),
    ZeroQueued(Node),
    Finalizing(Node),
    Zombie { kind: HeapNodeKind, strong: u32 },
    Vacant,
    Retired,
}

impl SlotState {
    const fn public_state(&self) -> HeapSlotState {
        match self {
            Self::Initializing { .. } => HeapSlotState::Initializing,
            Self::Live(_) => HeapSlotState::Live,
            Self::ZeroQueued(_) => HeapSlotState::ZeroQueued,
            Self::Finalizing(_) => HeapSlotState::Finalizing,
            Self::Zombie { .. } => HeapSlotState::Zombie,
            Self::Vacant => HeapSlotState::Vacant,
            Self::Retired => HeapSlotState::Retired,
        }
    }

    const fn kind(&self) -> Option<HeapNodeKind> {
        match self {
            Self::Initializing { kind, .. } | Self::Zombie { kind, .. } => Some(*kind),
            Self::Live(node) | Self::ZeroQueued(node) | Self::Finalizing(node) => {
                Some(node.data.kind())
            }
            Self::Vacant | Self::Retired => None,
        }
    }

    const fn strong(&self) -> Option<u32> {
        match self {
            Self::Initializing { strong, .. } | Self::Zombie { strong, .. } => Some(*strong),
            Self::Live(node) | Self::ZeroQueued(node) | Self::Finalizing(node) => Some(node.strong),
            Self::Vacant | Self::Retired => None,
        }
    }
}

struct ArenaSlot {
    generation: u32,
    state: SlotState,
    weak_prev: Option<ObjectId>,
    weak_next: Option<ObjectId>,
}

/// Runtime-local object and shape arena.
///
/// A `Heap` is deliberately not internally synchronized.  The enclosing
/// runtime chooses its single-threaded ownership boundary, as QuickJS does.
pub struct Heap {
    slots: Vec<ArenaSlot>,
    free: Vec<u32>,
    zero_queue: VecDeque<RawId>,
    weak_head: Option<ObjectId>,
    weak_tail: Option<ObjectId>,
}

impl Default for Heap {
    fn default() -> Self {
        Self::new()
    }
}

impl Heap {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            slots: Vec::new(),
            free: Vec::new(),
            zero_queue: VecDeque::new(),
            weak_head: None,
            weak_tail: None,
        }
    }

    /// Allocate and publish a shape, retaining its prototype edge.
    ///
    /// The caller owns one returned shape reference and must eventually call
    /// [`Heap::release_shape`].  Atom references are owned by the caller until
    /// this succeeds, then by the shape until returned as cleanup.
    pub fn allocate_shape(&mut self, shape: Shape) -> Result<ShapeId, HeapError> {
        let (index, generation) = self.reserve(HeapNodeKind::Shape)?;
        let id = ShapeId { index, generation };
        let edges = shape_edges(&shape);

        if let Err(error) = self.retain_edges_transactionally(&edges) {
            self.abort_initializing(index)?;
            return Err(error);
        }

        self.publish(index, NodeData::Shape(shape))?;
        Ok(id)
    }

    /// Allocate and publish an object, retaining its shape and property edges.
    ///
    /// The caller owns one returned object reference and must eventually call
    /// [`Heap::release_object`].
    pub fn allocate_object(&mut self, object: ObjectData) -> Result<ObjectId, HeapError> {
        if matches!(
            &object.payload,
            ObjectPayload::NativeFunction {
                data: NativeFunctionData { realm: None, .. },
                ..
            }
        ) {
            return Err(HeapError::Invariant(
                "an unbound native function may only be allocated during realm bootstrap",
            ));
        }
        self.allocate_object_inner(object)
    }

    /// Allocate a genuine WeakRef behind the runtime intrinsic surface. The
    /// target remains a non-owning generational identity.
    pub(crate) fn allocate_weak_ref_object(
        &mut self,
        shape: ShapeId,
        slots: Vec<PropertySlot>,
        target: WeakCollectionKey,
    ) -> Result<ObjectId, HeapError> {
        self.validate_live_weak_target(target)?;
        self.allocate_object_inner(ObjectData::weak_ref(shape, slots, target))
    }

    /// Allocate a genuine FinalizationRegistry behind its public runtime
    /// constructor. Callback and realm are traced by the payload itself.
    pub(crate) fn allocate_finalization_registry_object(
        &mut self,
        shape: ShapeId,
        slots: Vec<PropertySlot>,
        callback: ObjectId,
        realm: ContextId,
    ) -> Result<ObjectId, HeapError> {
        if !object_data_is_callable(self.object(callback)?) {
            return Err(HeapError::Invariant(
                "FinalizationRegistry callback is not callable",
            ));
        }
        self.context(realm)?;
        self.allocate_object_inner(ObjectData::finalization_registry(
            shape, slots, callback, realm,
        ))
    }

    /// Allocate the one provisional native callable needed to bootstrap a
    /// realm. The caller must synchronously finish it with
    /// [`Self::attach_native_function_realm`] before exposing it.
    pub(crate) fn allocate_bootstrap_native_function(
        &mut self,
        object: ObjectData,
    ) -> Result<ObjectId, HeapError> {
        if !matches!(
            &object.payload,
            ObjectPayload::NativeFunction {
                data: NativeFunctionData {
                    target: NativeFunctionId::FunctionPrototype,
                    realm: None,
                    ..
                },
                ..
            }
        ) {
            return Err(HeapError::Invariant(
                "bootstrap native-function allocation requires an unbound Function.prototype",
            ));
        }
        self.allocate_object_inner(object)
    }

    fn allocate_object_inner(&mut self, object: ObjectData) -> Result<ObjectId, HeapError> {
        self.validate_object_layout(&object)?;
        let is_weak_object = matches!(
            &object.payload,
            ObjectPayload::WeakMap { .. }
                | ObjectPayload::WeakSet { .. }
                | ObjectPayload::WeakRef { .. }
                | ObjectPayload::FinalizationRegistry(_)
        );
        let (index, generation) = self.reserve(HeapNodeKind::Object)?;
        let id = ObjectId { index, generation };
        let edges = object_edges(&object);

        if let Err(error) = self.retain_edges_transactionally(&edges) {
            self.abort_initializing(index)?;
            return Err(error);
        }

        self.publish(index, NodeData::Object(object))?;
        if is_weak_object {
            self.link_weak_object(id)?;
        }
        Ok(id)
    }

    /// Allocate and publish a realm/context node, retaining all realm roots.
    /// Symbol atoms in `intrinsics` transfer to the node on success.
    pub fn allocate_context(&mut self, context: ContextData) -> Result<ContextId, HeapError> {
        if context
            .intrinsics
            .iter()
            .any(|value| matches!(value, RawValue::Private(_)))
        {
            return Err(HeapError::Invariant(
                "private-name identity escaped into a realm intrinsic",
            ));
        }
        let (index, generation) = self.reserve(HeapNodeKind::Context)?;
        let id = ContextId { index, generation };
        let edges = context_edges(&context);

        if let Err(error) = self.retain_edges_transactionally(&edges) {
            self.abort_initializing(index)?;
            return Err(error);
        }

        self.publish(index, NodeData::Context(context))?;
        Ok(id)
    }

    /// Finish two-phase native-function bootstrap by installing its defining
    /// realm as an owned GC edge.
    ///
    /// Realm construction is necessarily cyclic: the Context owns
    /// `%Function.prototype%`, while that native callable owns its defining
    /// Context. The object is therefore allocated provisionally, the Context
    /// is published, and this operation closes the cycle transactionally.
    pub(crate) fn attach_native_function_realm(
        &mut self,
        object: ObjectId,
        realm: ContextId,
    ) -> Result<(), HeapError> {
        self.context(realm)?;
        match &self.object(object)?.payload {
            ObjectPayload::NativeFunction {
                data: NativeFunctionData { realm: None, .. },
                ..
            } => {}
            ObjectPayload::NativeFunction {
                data: NativeFunctionData { realm: Some(_), .. },
                ..
            } => {
                return Err(HeapError::Invariant(
                    "native function already has a defining realm",
                ));
            }
            ObjectPayload::Ordinary
            | ObjectPayload::RawJson
            | ObjectPayload::Array { .. }
            | ObjectPayload::Arguments { .. }
            | ObjectPayload::ArrayIterator { .. }
            | ObjectPayload::ForInIterator(_)
            | ObjectPayload::Primitive(_)
            | ObjectPayload::Date(_)
            | ObjectPayload::RegExp(_)
            | ObjectPayload::RegExpStringIterator { .. }
            | ObjectPayload::Map { .. }
            | ObjectPayload::MapIterator { .. }
            | ObjectPayload::Set { .. }
            | ObjectPayload::SetIterator { .. }
            | ObjectPayload::WeakMap { .. }
            | ObjectPayload::WeakSet { .. }
            | ObjectPayload::WeakRef { .. }
            | ObjectPayload::FinalizationRegistry(_)
            | ObjectPayload::GlobalObject { .. }
            | ObjectPayload::Error
            | ObjectPayload::StringIterator { .. }
            | ObjectPayload::IteratorHelper(_)
            | ObjectPayload::IteratorWrap(_)
            | ObjectPayload::AsyncFromSyncIterator(_)
            | ObjectPayload::IteratorConcat(_)
            | ObjectPayload::Proxy(_)
            | ObjectPayload::ArrayBuffer(_)
            | ObjectPayload::SharedArrayBuffer(_)
            | ObjectPayload::DataView(_)
            | ObjectPayload::TypedArray(_)
            | ObjectPayload::BoundFunction { .. }
            | ObjectPayload::BytecodeFunction { .. }
            | ObjectPayload::Generator { .. }
            | ObjectPayload::AsyncGenerator(_)
            | ObjectPayload::AsyncFunctionState(_)
            | ObjectPayload::Promise(_) => {
                return Err(HeapError::Invariant(
                    "attempted to attach a native realm to a non-native function",
                ));
            }
        }

        self.retain_raw(RawId::Context(realm), 1)?;
        let ObjectPayload::NativeFunction { data, .. } = &mut self.object_mut(object)?.payload
        else {
            unreachable!("native-function payload was validated before retaining its realm")
        };
        data.realm = Some(realm);
        Ok(())
    }

    /// Publish the realm's shared frozen `%ThrowTypeError%` root after the
    /// context exists. Its native callable already owns the context, so this
    /// deliberately closes the same collectable realm cycle as QuickJS.
    pub(crate) fn attach_throw_type_error(
        &mut self,
        realm: ContextId,
        thrower: ObjectId,
    ) -> Result<(), HeapError> {
        let context = self.context(realm)?;
        if context.throw_type_error.is_some() {
            return Err(HeapError::Invariant(
                "context already has a %ThrowTypeError% root",
            ));
        }
        if !matches!(
            self.object(thrower)?.payload,
            ObjectPayload::NativeFunction {
                data: NativeFunctionData {
                    target: NativeFunctionId::ThrowTypeError,
                    realm: Some(target_realm),
                    ..
                },
                ..
            } if target_realm == realm
        ) {
            return Err(HeapError::Invariant(
                "%ThrowTypeError% root is not the realm's poison native function",
            ));
        }

        self.retain_raw(RawId::Object(thrower), 1)?;
        let NodeData::Context(context) = &mut self.live_node_mut(RawId::Context(realm))?.data
        else {
            unreachable!("context identity was validated before retaining the thrower")
        };
        context.throw_type_error = Some(thrower);
        Ok(())
    }

    /// Cache the original realm-local `Array.prototype.values` callable.
    /// This is a distinct Context root because the public prototype property
    /// is writable and configurable while arguments creation must keep using
    /// the bootstrap identity.
    pub(crate) fn attach_array_prototype_values(
        &mut self,
        realm: ContextId,
        values: ObjectId,
    ) -> Result<(), HeapError> {
        let context = self.context(realm)?;
        if context.array_prototype_values.is_some() {
            return Err(HeapError::Invariant(
                "context already has an Array.prototype.values cache root",
            ));
        }
        if !matches!(
            self.object(values)?.payload,
            ObjectPayload::NativeFunction {
                data: NativeFunctionData {
                    target: NativeFunctionId::ArrayPrototypeIterator(ArrayIteratorKind::Value),
                    realm: Some(target_realm),
                    ..
                },
                ..
            } if target_realm == realm
        ) {
            return Err(HeapError::Invariant(
                "Array.prototype.values cache is not the realm's values native",
            ));
        }

        self.retain_raw(RawId::Object(values), 1)?;
        let NodeData::Context(context) = &mut self.live_node_mut(RawId::Context(realm))?.data
        else {
            unreachable!("context identity was validated before retaining Array values")
        };
        context.array_prototype_values = Some(values);
        Ok(())
    }

    /// Publish the realm's `%Function%` root after its native callable and
    /// constructor/prototype cycle have been fully initialized.
    pub(crate) fn attach_function_constructor(
        &mut self,
        realm: ContextId,
        constructor: ObjectId,
    ) -> Result<(), HeapError> {
        let context = self.context(realm)?;
        if context.function_constructor.is_some() {
            return Err(HeapError::Invariant(
                "context already has a Function constructor root",
            ));
        }
        let constructor_object = self.object(constructor)?;
        if !constructor_object.is_constructor
            || !matches!(
                constructor_object.payload,
                ObjectPayload::NativeFunction {
                    data: NativeFunctionData {
                        target: NativeFunctionId::FunctionConstructor(DynamicFunctionKind::Normal),
                        realm: Some(target_realm),
                        ..
                    },
                    ..
                } if target_realm == realm
            )
        {
            return Err(HeapError::Invariant(
                "Function constructor root is not the realm's Function native",
            ));
        }

        self.retain_raw(RawId::Object(constructor), 1)?;
        let NodeData::Context(context) = &mut self.live_node_mut(RawId::Context(realm))?.data
        else {
            unreachable!("context identity was validated before retaining Function")
        };
        context.function_constructor = Some(constructor);
        Ok(())
    }

    /// Cache the realm's original `%eval%` callable independently from its
    /// mutable global property, matching `JSContext.eval_obj`.
    pub(crate) fn attach_eval_function(
        &mut self,
        realm: ContextId,
        function: ObjectId,
    ) -> Result<(), HeapError> {
        let context = self.context(realm)?;
        if context.eval_function.is_some() {
            return Err(HeapError::Invariant(
                "context already has an eval function root",
            ));
        }
        let function_object = self.object(function)?;
        if function_object.is_constructor
            || !matches!(
                function_object.payload,
                ObjectPayload::NativeFunction {
                    data: NativeFunctionData {
                        target: NativeFunctionId::GlobalEval,
                        realm: Some(target_realm),
                        ..
                    },
                    ..
                } if target_realm == realm
            )
        {
            return Err(HeapError::Invariant(
                "eval function root is not the realm's non-constructor eval native",
            ));
        }

        self.retain_raw(RawId::Object(function), 1)?;
        let NodeData::Context(context) = &mut self.live_node_mut(RawId::Context(realm))?.data
        else {
            unreachable!("context identity was validated before retaining eval")
        };
        context.eval_function = Some(function);
        Ok(())
    }

    /// Publish the realm's `%Array%` root after its native callable and
    /// constructor/prototype cycle have been initialized.
    pub(crate) fn attach_array_constructor(
        &mut self,
        realm: ContextId,
        constructor: ObjectId,
    ) -> Result<(), HeapError> {
        let context = self.context(realm)?;
        if context.array_constructor.is_some() {
            return Err(HeapError::Invariant(
                "context already has an Array constructor root",
            ));
        }
        let constructor_object = self.object(constructor)?;
        if !constructor_object.is_constructor
            || !matches!(
                constructor_object.payload,
                ObjectPayload::NativeFunction {
                    data: NativeFunctionData {
                        target: NativeFunctionId::ArrayConstructor,
                        realm: Some(target_realm),
                        ..
                    },
                    ..
                } if target_realm == realm
            )
        {
            return Err(HeapError::Invariant(
                "Array constructor root is not the realm's Array native",
            ));
        }

        self.retain_raw(RawId::Object(constructor), 1)?;
        let NodeData::Context(context) = &mut self.live_node_mut(RawId::Context(realm))?.data
        else {
            unreachable!("context identity was validated before retaining Array")
        };
        context.array_constructor = Some(constructor);
        Ok(())
    }

    /// Atomically publish the realm's RegExp intrinsic roots after its native
    /// constructor, ordinary prototype, RegExp String Iterator prototype, and
    /// canonical instance shape exist.
    ///
    /// `last_index_atom` is validation-only: the shape already owns its atom
    /// edge. Passing it explicitly lets this heap layer prove that the sole
    /// instance slot really is `lastIndex` without depending on `AtomTable`
    /// string lookup. The four GC edges are retained as one transaction
    /// before the Context is mutated.
    pub(crate) fn attach_regexp_intrinsics(
        &mut self,
        realm: ContextId,
        regexp: RegExpRealmData,
        last_index_atom: Atom,
    ) -> Result<(), HeapError> {
        let context = self.context(realm)?;
        if context.regexp.is_some() {
            return Err(HeapError::Invariant(
                "context already has RegExp intrinsic roots",
            ));
        }
        let iterator_prototype = context.iterator_prototype;

        let constructor = self.object(regexp.constructor)?;
        if !constructor.is_constructor
            || !matches!(
                constructor.payload,
                ObjectPayload::NativeFunction {
                    data: NativeFunctionData {
                        target: NativeFunctionId::RegExp(RegExpNativeKind::Constructor),
                        realm: Some(target_realm),
                        ..
                    },
                    ..
                } if target_realm == realm
            )
        {
            return Err(HeapError::Invariant(
                "RegExp constructor root is not the realm's RegExp native",
            ));
        }

        let prototype = self.object(regexp.prototype)?;
        if prototype.kind != ObjectKind::Ordinary
            || !matches!(prototype.payload, ObjectPayload::Ordinary)
        {
            return Err(HeapError::Invariant(
                "RegExp prototype root is not an ordinary object",
            ));
        }

        let string_iterator_prototype = self.object(regexp.string_iterator_prototype)?;
        if string_iterator_prototype.kind != ObjectKind::Ordinary
            || !matches!(string_iterator_prototype.payload, ObjectPayload::Ordinary)
        {
            return Err(HeapError::Invariant(
                "RegExp String Iterator prototype root is not an ordinary object",
            ));
        }
        if self.shape(string_iterator_prototype.shape)?.prototype() != Some(iterator_prototype) {
            return Err(HeapError::Invariant(
                "RegExp String Iterator prototype does not inherit from the realm's Iterator prototype",
            ));
        }

        let object_shape = self.shape(regexp.object_shape)?;
        if object_shape.prototype() != Some(regexp.prototype) {
            return Err(HeapError::Invariant(
                "RegExp object shape does not inherit from the realm's RegExp prototype",
            ));
        }
        let [last_index] = object_shape.entries() else {
            return Err(HeapError::Invariant(
                "RegExp object shape does not contain exactly one lastIndex property",
            ));
        };
        if last_index.atom != last_index_atom
            || last_index.flags != PropertyFlags::data(true, false, false)
        {
            return Err(HeapError::Invariant(
                "RegExp object shape has an invalid lastIndex property",
            ));
        }

        let edges = [
            RawId::Object(regexp.prototype),
            RawId::Object(regexp.constructor),
            RawId::Object(regexp.string_iterator_prototype),
            RawId::Shape(regexp.object_shape),
        ];
        self.retain_edges_transactionally(&edges)?;

        let NodeData::Context(context) = &mut self.live_node_mut(RawId::Context(realm))?.data
        else {
            unreachable!("context identity was validated before retaining RegExp roots")
        };
        context.regexp = Some(regexp);
        Ok(())
    }

    /// Atomically publish the realm's Map constructor, ordinary prototype,
    /// and Map Iterator prototype roots after all three have been initialized.
    pub(crate) fn attach_map_intrinsics(
        &mut self,
        realm: ContextId,
        map: MapRealmData,
    ) -> Result<(), HeapError> {
        let context = self.context(realm)?;
        if context.map.is_some() {
            return Err(HeapError::Invariant(
                "context already has Map intrinsic roots",
            ));
        }
        let iterator_prototype = context.iterator_prototype;

        let prototype = self.object(map.prototype)?;
        if prototype.kind != ObjectKind::Ordinary
            || !matches!(prototype.payload, ObjectPayload::Ordinary)
        {
            return Err(HeapError::Invariant(
                "Map prototype root is not an ordinary object",
            ));
        }

        let map_iterator_prototype = self.object(map.iterator_prototype)?;
        if map_iterator_prototype.kind != ObjectKind::Ordinary
            || !matches!(map_iterator_prototype.payload, ObjectPayload::Ordinary)
        {
            return Err(HeapError::Invariant(
                "Map Iterator prototype root is not an ordinary object",
            ));
        }
        if self.shape(map_iterator_prototype.shape)?.prototype() != Some(iterator_prototype) {
            return Err(HeapError::Invariant(
                "Map Iterator prototype does not inherit from the realm's Iterator prototype",
            ));
        }

        let edges = [
            RawId::Object(map.prototype),
            RawId::Object(map.iterator_prototype),
        ];
        self.retain_edges_transactionally(&edges)?;

        let NodeData::Context(context) = &mut self.live_node_mut(RawId::Context(realm))?.data
        else {
            unreachable!("context identity was validated before retaining Map roots")
        };
        context.map = Some(map);
        Ok(())
    }

    /// Atomically publish the realm's ordinary Set prototype and Set Iterator
    /// prototype roots after both have been initialized.
    pub(crate) fn attach_set_intrinsics(
        &mut self,
        realm: ContextId,
        set: SetRealmData,
    ) -> Result<(), HeapError> {
        let context = self.context(realm)?;
        if context.set.is_some() {
            return Err(HeapError::Invariant(
                "context already has Set intrinsic roots",
            ));
        }
        let iterator_prototype = context.iterator_prototype;

        let prototype = self.object(set.prototype)?;
        if prototype.kind != ObjectKind::Ordinary
            || !matches!(prototype.payload, ObjectPayload::Ordinary)
        {
            return Err(HeapError::Invariant(
                "Set prototype root is not an ordinary object",
            ));
        }

        let set_iterator_prototype = self.object(set.iterator_prototype)?;
        if set_iterator_prototype.kind != ObjectKind::Ordinary
            || !matches!(set_iterator_prototype.payload, ObjectPayload::Ordinary)
        {
            return Err(HeapError::Invariant(
                "Set Iterator prototype root is not an ordinary object",
            ));
        }
        if self.shape(set_iterator_prototype.shape)?.prototype() != Some(iterator_prototype) {
            return Err(HeapError::Invariant(
                "Set Iterator prototype does not inherit from the realm's Iterator prototype",
            ));
        }

        let edges = [
            RawId::Object(set.prototype),
            RawId::Object(set.iterator_prototype),
        ];
        self.retain_edges_transactionally(&edges)?;

        let NodeData::Context(context) = &mut self.live_node_mut(RawId::Context(realm))?.data
        else {
            unreachable!("context identity was validated before retaining Set roots")
        };
        context.set = Some(set);
        Ok(())
    }

    /// Atomically publish the realm's ordinary WeakMap prototype root.
    pub(crate) fn attach_weak_map_intrinsics(
        &mut self,
        realm: ContextId,
        weak_map: WeakMapRealmData,
    ) -> Result<(), HeapError> {
        let context = self.context(realm)?;
        if context.weak_map.is_some() {
            return Err(HeapError::Invariant(
                "context already has WeakMap intrinsic roots",
            ));
        }
        let prototype = self.object(weak_map.prototype)?;
        if prototype.kind != ObjectKind::Ordinary
            || !matches!(prototype.payload, ObjectPayload::Ordinary)
        {
            return Err(HeapError::Invariant(
                "WeakMap prototype root is not an ordinary object",
            ));
        }

        self.retain_raw(RawId::Object(weak_map.prototype), 1)?;
        let NodeData::Context(context) = &mut self.live_node_mut(RawId::Context(realm))?.data
        else {
            unreachable!("context identity was validated before retaining WeakMap roots")
        };
        context.weak_map = Some(weak_map);
        Ok(())
    }

    /// Atomically publish the realm's ordinary WeakSet prototype root.
    pub(crate) fn attach_weak_set_intrinsics(
        &mut self,
        realm: ContextId,
        weak_set: WeakSetRealmData,
    ) -> Result<(), HeapError> {
        let context = self.context(realm)?;
        if context.weak_set.is_some() {
            return Err(HeapError::Invariant(
                "context already has WeakSet intrinsic roots",
            ));
        }
        let prototype = self.object(weak_set.prototype)?;
        if prototype.kind != ObjectKind::Ordinary
            || !matches!(prototype.payload, ObjectPayload::Ordinary)
        {
            return Err(HeapError::Invariant(
                "WeakSet prototype root is not an ordinary object",
            ));
        }

        self.retain_raw(RawId::Object(weak_set.prototype), 1)?;
        let NodeData::Context(context) = &mut self.live_node_mut(RawId::Context(realm))?.data
        else {
            unreachable!("context identity was validated before retaining WeakSet roots")
        };
        context.weak_set = Some(weak_set);
        Ok(())
    }

    /// Atomically publish the realm's WeakRef and FinalizationRegistry class
    /// prototype roots. Both are ordinary children of this realm's
    /// Object.prototype and are installed together by pinned QuickJS.
    pub(crate) fn attach_weak_ref_intrinsics(
        &mut self,
        realm: ContextId,
        weak_ref: WeakRefRealmData,
    ) -> Result<(), HeapError> {
        let context = self.context(realm)?;
        if context.weak_ref.is_some() {
            return Err(HeapError::Invariant(
                "context already has WeakRef intrinsic roots",
            ));
        }
        if weak_ref.weak_ref_prototype == weak_ref.finalization_registry_prototype {
            return Err(HeapError::Invariant(
                "WeakRef and FinalizationRegistry prototypes share one identity",
            ));
        }
        let object_prototype = context.object_prototype;
        for (prototype, message) in [
            (
                weak_ref.weak_ref_prototype,
                "WeakRef prototype is not an ordinary child of Object.prototype",
            ),
            (
                weak_ref.finalization_registry_prototype,
                "FinalizationRegistry prototype is not an ordinary child of Object.prototype",
            ),
        ] {
            let object = self.object(prototype)?;
            if object.kind != ObjectKind::Ordinary
                || !matches!(object.payload, ObjectPayload::Ordinary)
                || self.shape(object.shape)?.prototype() != Some(object_prototype)
            {
                return Err(HeapError::Invariant(message));
            }
        }

        let edges = [
            RawId::Object(weak_ref.weak_ref_prototype),
            RawId::Object(weak_ref.finalization_registry_prototype),
        ];
        self.retain_edges_transactionally(&edges)?;

        let NodeData::Context(context) = &mut self.live_node_mut(RawId::Context(realm))?.data
        else {
            unreachable!("context identity was validated before retaining WeakRef roots")
        };
        context.weak_ref = Some(weak_ref);
        Ok(())
    }

    /// Atomically publish the realm's `%ArrayBuffer.prototype%` class root
    /// after validating the public constructor relationship.
    pub(crate) fn attach_array_buffer_intrinsics(
        &mut self,
        realm: ContextId,
        constructor: ObjectId,
        array_buffer: ArrayBufferRealmData,
    ) -> Result<(), HeapError> {
        let context = self.context(realm)?;
        if context.array_buffer.is_some() {
            return Err(HeapError::Invariant(
                "context already has ArrayBuffer intrinsic roots",
            ));
        }
        let object_prototype = context.object_prototype;

        let constructor = self.object(constructor)?;
        if !constructor.is_constructor
            || !matches!(
                constructor.payload,
                ObjectPayload::NativeFunction {
                    data: NativeFunctionData {
                        target: NativeFunctionId::ArrayBuffer(
                            ArrayBufferNativeKind::Constructor
                        ),
                        realm: Some(target_realm),
                        ..
                    },
                    internal: None,
                } if target_realm == realm
            )
        {
            return Err(HeapError::Invariant(
                "ArrayBuffer constructor root is not the realm's ArrayBuffer native",
            ));
        }

        let prototype = self.object(array_buffer.prototype)?;
        if prototype.kind != ObjectKind::Ordinary
            || !matches!(prototype.payload, ObjectPayload::Ordinary)
            || self.shape(prototype.shape)?.prototype() != Some(object_prototype)
        {
            return Err(HeapError::Invariant(
                "ArrayBuffer prototype is not an ordinary child of Object.prototype",
            ));
        }

        let edges = [RawId::Object(array_buffer.prototype)];
        self.retain_edges_transactionally(&edges)?;

        let NodeData::Context(context) = &mut self.live_node_mut(RawId::Context(realm))?.data
        else {
            unreachable!("context identity was validated before retaining ArrayBuffer roots")
        };
        context.array_buffer = Some(array_buffer);
        Ok(())
    }

    /// Atomically publish the realm's `%SharedArrayBuffer.prototype%` class
    /// root after validating its independent public constructor relationship.
    pub(crate) fn attach_shared_array_buffer_intrinsics(
        &mut self,
        realm: ContextId,
        constructor: ObjectId,
        shared_array_buffer: SharedArrayBufferRealmData,
    ) -> Result<(), HeapError> {
        let context = self.context(realm)?;
        if context.shared_array_buffer.is_some() {
            return Err(HeapError::Invariant(
                "context already has SharedArrayBuffer intrinsic roots",
            ));
        }
        let object_prototype = context.object_prototype;

        let constructor = self.object(constructor)?;
        if !constructor.is_constructor
            || !matches!(
                constructor.payload,
                ObjectPayload::NativeFunction {
                    data: NativeFunctionData {
                        target: NativeFunctionId::SharedArrayBuffer(
                            SharedArrayBufferNativeKind::Constructor
                        ),
                        realm: Some(target_realm),
                        ..
                    },
                    internal: None,
                } if target_realm == realm
            )
        {
            return Err(HeapError::Invariant(
                "SharedArrayBuffer constructor root is not the realm's SharedArrayBuffer native",
            ));
        }

        let prototype = self.object(shared_array_buffer.prototype)?;
        if prototype.kind != ObjectKind::Ordinary
            || !matches!(prototype.payload, ObjectPayload::Ordinary)
            || self.shape(prototype.shape)?.prototype() != Some(object_prototype)
        {
            return Err(HeapError::Invariant(
                "SharedArrayBuffer prototype is not an ordinary child of Object.prototype",
            ));
        }

        let edges = [RawId::Object(shared_array_buffer.prototype)];
        self.retain_edges_transactionally(&edges)?;

        let NodeData::Context(context) = &mut self.live_node_mut(RawId::Context(realm))?.data
        else {
            unreachable!("context identity was validated before retaining SharedArrayBuffer roots")
        };
        context.shared_array_buffer = Some(shared_array_buffer);
        Ok(())
    }

    /// Atomically publish the twelve concrete TypedArray class prototypes.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn attach_typed_array_intrinsics(
        &mut self,
        realm: ContextId,
        base_constructor: ObjectId,
        base_prototype: ObjectId,
        constructors: [ObjectId; TypedArrayElementKind::COUNT],
        typed_array: TypedArrayRealmData,
    ) -> Result<(), HeapError> {
        let context = self.context(realm)?;
        if context.typed_array.is_some() {
            return Err(HeapError::Invariant(
                "context already has TypedArray intrinsic roots",
            ));
        }
        let object_prototype = context.object_prototype;
        let function_prototype = context.function_prototype;

        let base_constructor_data = self.object(base_constructor)?;
        if !base_constructor_data.is_constructor
            || self.shape(base_constructor_data.shape)?.prototype() != Some(function_prototype)
            || !matches!(
                base_constructor_data.payload,
                ObjectPayload::NativeFunction {
                    data: NativeFunctionData {
                        target: NativeFunctionId::TypedArray(
                            TypedArrayNativeKind::BaseConstructor
                        ),
                        realm: Some(target_realm),
                        ..
                    },
                    internal: None,
                } if target_realm == realm
            )
        {
            return Err(HeapError::Invariant(
                "TypedArray base constructor has an invalid realm or prototype",
            ));
        }

        let base_prototype_data = self.object(base_prototype)?;
        if base_prototype_data.kind != ObjectKind::Ordinary
            || !matches!(base_prototype_data.payload, ObjectPayload::Ordinary)
            || self.shape(base_prototype_data.shape)?.prototype() != Some(object_prototype)
        {
            return Err(HeapError::Invariant(
                "TypedArray base prototype is not an ordinary child of Object.prototype",
            ));
        }

        for (index, element) in TypedArrayElementKind::ALL.into_iter().enumerate() {
            let constructor = self.object(constructors[index])?;
            if !constructor.is_constructor
                || self.shape(constructor.shape)?.prototype() != Some(base_constructor)
                || !matches!(
                    constructor.payload,
                    ObjectPayload::NativeFunction {
                        data: NativeFunctionData {
                            target: NativeFunctionId::TypedArray(
                                TypedArrayNativeKind::Constructor(target_element)
                            ),
                            realm: Some(target_realm),
                            ..
                        },
                        internal: None,
                    } if target_realm == realm && target_element == element
                )
            {
                return Err(HeapError::Invariant(
                    "concrete TypedArray constructor has an invalid class graph",
                ));
            }
            let prototype = self.object(typed_array.prototypes[index])?;
            if prototype.kind != ObjectKind::Ordinary
                || !matches!(prototype.payload, ObjectPayload::Ordinary)
                || self.shape(prototype.shape)?.prototype() != Some(base_prototype)
            {
                return Err(HeapError::Invariant(
                    "concrete TypedArray prototype has an invalid class graph",
                ));
            }
        }

        let edges = typed_array
            .prototypes
            .into_iter()
            .map(RawId::Object)
            .collect::<Vec<_>>();
        self.retain_edges_transactionally(&edges)?;

        let NodeData::Context(context) = &mut self.live_node_mut(RawId::Context(realm))?.data
        else {
            unreachable!("context identity was validated before retaining TypedArray roots")
        };
        context.typed_array = Some(typed_array);
        Ok(())
    }

    /// Atomically publish the realm's `%DataView.prototype%` class root after
    /// validating the public constructor relationship.
    pub(crate) fn attach_data_view_intrinsics(
        &mut self,
        realm: ContextId,
        constructor: ObjectId,
        data_view: DataViewRealmData,
    ) -> Result<(), HeapError> {
        let context = self.context(realm)?;
        if context.data_view.is_some() {
            return Err(HeapError::Invariant(
                "context already has DataView intrinsic roots",
            ));
        }
        let object_prototype = context.object_prototype;

        let constructor = self.object(constructor)?;
        if !constructor.is_constructor
            || !matches!(
                constructor.payload,
                ObjectPayload::NativeFunction {
                    data: NativeFunctionData {
                        target: NativeFunctionId::DataView(DataViewNativeKind::Constructor),
                        realm: Some(target_realm),
                        ..
                    },
                    internal: None,
                } if target_realm == realm
            )
        {
            return Err(HeapError::Invariant(
                "DataView constructor root is not the realm's DataView native",
            ));
        }

        let prototype = self.object(data_view.prototype)?;
        if prototype.kind != ObjectKind::Ordinary
            || !matches!(prototype.payload, ObjectPayload::Ordinary)
            || self.shape(prototype.shape)?.prototype() != Some(object_prototype)
        {
            return Err(HeapError::Invariant(
                "DataView prototype is not an ordinary child of Object.prototype",
            ));
        }

        let edges = [RawId::Object(data_view.prototype)];
        self.retain_edges_transactionally(&edges)?;

        let NodeData::Context(context) = &mut self.live_node_mut(RawId::Context(realm))?.data
        else {
            unreachable!("context identity was validated before retaining DataView roots")
        };
        context.data_view = Some(data_view);
        Ok(())
    }

    /// Atomically publish the synchronous Iterator constructor and the hidden
    /// Iterator Concat/Helper/Wrap class prototypes.
    pub(crate) fn attach_iterator_intrinsics(
        &mut self,
        realm: ContextId,
        iterator: IteratorRealmData,
    ) -> Result<(), HeapError> {
        let context = self.context(realm)?;
        if context.iterator.is_some() {
            return Err(HeapError::Invariant(
                "context already has Iterator intrinsic roots",
            ));
        }
        let iterator_prototype = context.iterator_prototype;

        let constructor = self.object(iterator.constructor)?;
        if !constructor.is_constructor
            || !matches!(
                constructor.payload,
                ObjectPayload::NativeFunction {
                    data: NativeFunctionData {
                        target: NativeFunctionId::IteratorConstructor,
                        realm: Some(target_realm),
                        ..
                    },
                    internal: None,
                } if target_realm == realm
            )
        {
            return Err(HeapError::Invariant(
                "Iterator constructor root is not the realm's Iterator native",
            ));
        }

        if iterator.concat_prototype == iterator.helper_prototype
            || iterator.concat_prototype == iterator.wrap_prototype
            || iterator.helper_prototype == iterator.wrap_prototype
        {
            return Err(HeapError::Invariant(
                "Iterator hidden prototypes share an identity",
            ));
        }
        for (prototype, message) in [
            (
                iterator.concat_prototype,
                "Iterator Concat prototype is not an ordinary child of the realm's Iterator prototype",
            ),
            (
                iterator.helper_prototype,
                "Iterator Helper prototype is not an ordinary child of the realm's Iterator prototype",
            ),
            (
                iterator.wrap_prototype,
                "Iterator Wrap prototype is not an ordinary child of the realm's Iterator prototype",
            ),
        ] {
            let object = self.object(prototype)?;
            if object.kind != ObjectKind::Ordinary
                || !matches!(object.payload, ObjectPayload::Ordinary)
                || self.shape(object.shape)?.prototype() != Some(iterator_prototype)
            {
                return Err(HeapError::Invariant(message));
            }
        }

        let edges = [
            RawId::Object(iterator.constructor),
            RawId::Object(iterator.concat_prototype),
            RawId::Object(iterator.helper_prototype),
            RawId::Object(iterator.wrap_prototype),
        ];
        self.retain_edges_transactionally(&edges)?;

        let NodeData::Context(context) = &mut self.live_node_mut(RawId::Context(realm))?.data
        else {
            unreachable!("context identity was validated before retaining Iterator roots")
        };
        context.iterator = Some(iterator);
        Ok(())
    }

    /// Atomically publish the realm's Promise constructor and ordinary
    /// prototype after their public constructor/prototype links exist.
    pub(crate) fn attach_promise_intrinsics(
        &mut self,
        realm: ContextId,
        promise: PromiseRealmData,
    ) -> Result<(), HeapError> {
        let context = self.context(realm)?;
        if context.promise.is_some() {
            return Err(HeapError::Invariant(
                "context already has Promise intrinsic roots",
            ));
        }
        let object_prototype = context.object_prototype;

        let constructor = self.object(promise.constructor)?;
        if !constructor.is_constructor
            || !matches!(
                constructor.payload,
                ObjectPayload::NativeFunction {
                    data: NativeFunctionData {
                        target: NativeFunctionId::Promise(PromiseNativeKind::Constructor),
                        realm: Some(target_realm),
                        ..
                    },
                    internal: None,
                } if target_realm == realm
            )
        {
            return Err(HeapError::Invariant(
                "Promise constructor root is not the realm's Promise native",
            ));
        }

        let prototype = self.object(promise.prototype)?;
        if prototype.kind != ObjectKind::Ordinary
            || !matches!(prototype.payload, ObjectPayload::Ordinary)
            || self.shape(prototype.shape)?.prototype() != Some(object_prototype)
        {
            return Err(HeapError::Invariant(
                "Promise prototype is not an ordinary child of Object.prototype",
            ));
        }

        let edges = [
            RawId::Object(promise.prototype),
            RawId::Object(promise.constructor),
        ];
        self.retain_edges_transactionally(&edges)?;

        let NodeData::Context(context) = &mut self.live_node_mut(RawId::Context(realm))?.data
        else {
            unreachable!("context identity was validated before retaining Promise roots")
        };
        context.promise = Some(promise);
        Ok(())
    }

    /// Atomically publish the two realm-local synchronous-generator class
    /// prototypes after their property graph has been initialized.
    pub(crate) fn attach_generator_intrinsics(
        &mut self,
        realm: ContextId,
        generator: GeneratorRealmData,
    ) -> Result<(), HeapError> {
        let context = self.context(realm)?;
        if context.generator.is_some() {
            return Err(HeapError::Invariant(
                "context already has Generator intrinsic roots",
            ));
        }
        let iterator_prototype = context.iterator_prototype;
        let function_prototype = context.function_prototype;

        let prototype = self.object(generator.prototype)?;
        if prototype.kind != ObjectKind::Ordinary
            || !matches!(prototype.payload, ObjectPayload::Ordinary)
            || self.shape(prototype.shape)?.prototype() != Some(iterator_prototype)
        {
            return Err(HeapError::Invariant(
                "Generator prototype does not inherit from the realm's Iterator prototype",
            ));
        }

        let generator_function_prototype = self.object(generator.function_prototype)?;
        if generator_function_prototype.kind != ObjectKind::Ordinary
            || !matches!(
                generator_function_prototype.payload,
                ObjectPayload::Ordinary
            )
            || self.shape(generator_function_prototype.shape)?.prototype()
                != Some(function_prototype)
        {
            return Err(HeapError::Invariant(
                "GeneratorFunction prototype does not inherit from Function.prototype",
            ));
        }

        let edges = [
            RawId::Object(generator.prototype),
            RawId::Object(generator.function_prototype),
        ];
        self.retain_edges_transactionally(&edges)?;

        let NodeData::Context(context) = &mut self.live_node_mut(RawId::Context(realm))?.data
        else {
            unreachable!("context identity was validated before retaining Generator roots")
        };
        context.generator = Some(generator);
        Ok(())
    }

    /// Atomically publish the realm-local `%AsyncFunction.prototype%` root
    /// after its hidden constructor/prototype property graph exists.
    pub(crate) fn attach_async_function_intrinsics(
        &mut self,
        realm: ContextId,
        async_function: AsyncFunctionRealmData,
    ) -> Result<(), HeapError> {
        let context = self.context(realm)?;
        if context.async_function.is_some() {
            return Err(HeapError::Invariant(
                "context already has AsyncFunction intrinsic roots",
            ));
        }
        let function_prototype = context.function_prototype;

        let async_function_prototype = self.object(async_function.function_prototype)?;
        if async_function_prototype.kind != ObjectKind::Ordinary
            || !matches!(async_function_prototype.payload, ObjectPayload::Ordinary)
            || self.shape(async_function_prototype.shape)?.prototype() != Some(function_prototype)
        {
            return Err(HeapError::Invariant(
                "AsyncFunction prototype does not inherit from Function.prototype",
            ));
        }

        self.retain_raw(RawId::Object(async_function.function_prototype), 1)?;
        let NodeData::Context(context) = &mut self.live_node_mut(RawId::Context(realm))?.data
        else {
            unreachable!("context identity was validated before retaining AsyncFunction roots")
        };
        context.async_function = Some(async_function);
        Ok(())
    }

    /// Atomically publish the realm-local async-iterator/generator graph.
    pub(crate) fn attach_async_generator_intrinsics(
        &mut self,
        realm: ContextId,
        async_generator: AsyncGeneratorRealmData,
    ) -> Result<(), HeapError> {
        let context = self.context(realm)?;
        if context.async_generator.is_some() {
            return Err(HeapError::Invariant(
                "context already has AsyncGenerator intrinsic roots",
            ));
        }
        let object_prototype = context.object_prototype;
        let function_prototype = context.function_prototype;

        let async_iterator = self.object(async_generator.async_iterator_prototype)?;
        if async_iterator.kind != ObjectKind::Ordinary
            || !matches!(async_iterator.payload, ObjectPayload::Ordinary)
            || self.shape(async_iterator.shape)?.prototype() != Some(object_prototype)
        {
            return Err(HeapError::Invariant(
                "AsyncIterator prototype does not inherit from Object.prototype",
            ));
        }
        let async_from_sync = self.object(async_generator.async_from_sync_iterator_prototype)?;
        if async_from_sync.kind != ObjectKind::Ordinary
            || !matches!(async_from_sync.payload, ObjectPayload::Ordinary)
            || self.shape(async_from_sync.shape)?.prototype()
                != Some(async_generator.async_iterator_prototype)
        {
            return Err(HeapError::Invariant(
                "AsyncFromSyncIterator prototype does not inherit from AsyncIterator prototype",
            ));
        }
        let prototype = self.object(async_generator.prototype)?;
        if prototype.kind != ObjectKind::Ordinary
            || !matches!(prototype.payload, ObjectPayload::Ordinary)
            || self.shape(prototype.shape)?.prototype()
                != Some(async_generator.async_iterator_prototype)
        {
            return Err(HeapError::Invariant(
                "AsyncGenerator prototype does not inherit from AsyncIterator prototype",
            ));
        }
        let function = self.object(async_generator.function_prototype)?;
        if function.kind != ObjectKind::Ordinary
            || !matches!(function.payload, ObjectPayload::Ordinary)
            || self.shape(function.shape)?.prototype() != Some(function_prototype)
        {
            return Err(HeapError::Invariant(
                "AsyncGeneratorFunction prototype does not inherit from Function.prototype",
            ));
        }

        self.retain_edges_transactionally(&[
            RawId::Object(async_generator.async_iterator_prototype),
            RawId::Object(async_generator.async_from_sync_iterator_prototype),
            RawId::Object(async_generator.prototype),
            RawId::Object(async_generator.function_prototype),
        ])?;
        let NodeData::Context(context) = &mut self.live_node_mut(RawId::Context(realm))?.data
        else {
            unreachable!("context identity was validated before retaining AsyncGenerator roots")
        };
        context.async_generator = Some(async_generator);
        Ok(())
    }

    /// Allocate and publish immutable function bytecode, retaining its realm
    /// and every GC edge in its constant pool. `auxiliary_atoms` and symbol
    /// constants transfer to the node on success. No arena slot is reserved
    /// until both metadata authentication and shared bytecode verification
    /// succeed.
    pub fn allocate_function_bytecode(
        &mut self,
        bytecode: FunctionBytecodeData,
    ) -> Result<FunctionBytecodeId, HeapError> {
        if bytecode
            .constants
            .iter()
            .any(|constant| matches!(constant, BytecodeConstant::Value(RawValue::Private(_))))
        {
            return Err(HeapError::Invariant(
                "private-name identity escaped into a bytecode constant",
            ));
        }
        if bytecode.metadata.local_count > MAX_LOCAL_SLOTS {
            return Err(HeapError::Invariant(
                "bytecode local count exceeds QuickJS JS_MAX_LOCAL_VARS",
            ));
        }
        let parameter_initializer_locals = bytecode
            .local_definitions
            .iter()
            .map(|definition| definition.is_parameter_initializer)
            .collect::<Vec<_>>();
        let parameter_body_pc = validate_parameter_bytecode_layout(
            &bytecode.metadata,
            &bytecode.code,
            &parameter_initializer_locals,
            bytecode.parameter_environment.as_ref(),
        )
        .map_err(HeapError::Invariant)?;
        let initial_yields = bytecode
            .code
            .iter()
            .enumerate()
            .filter_map(|(pc, instruction)| {
                matches!(instruction, Instruction::InitialYield).then_some(pc)
            })
            .collect::<Vec<_>>();
        let has_generator_only_instruction = bytecode.code.iter().any(|instruction| {
            matches!(
                instruction,
                Instruction::Yield
                    | Instruction::YieldStar
                    | Instruction::AsyncYieldStar
                    | Instruction::IteratorStart
                    | Instruction::AsyncIteratorStart
                    | Instruction::IteratorNext
                    | Instruction::IteratorCall(_)
                    | Instruction::IteratorCheckObject
                    | Instruction::IteratorDetachPreserve
                    | Instruction::ThrowIteratorMissingThrow
            )
        });
        let has_await = bytecode
            .code
            .iter()
            .any(|instruction| matches!(instruction, Instruction::Await));
        let has_sync_delegation = bytecode.code.iter().any(|instruction| {
            matches!(
                instruction,
                Instruction::YieldStar | Instruction::IteratorStart
            )
        });
        let has_async_delegation = bytecode.code.iter().any(|instruction| {
            matches!(
                instruction,
                Instruction::AsyncYieldStar | Instruction::AsyncIteratorStart
            )
        });
        let has_async_iteration = bytecode.code.iter().any(|instruction| {
            matches!(
                instruction,
                Instruction::ForAwaitOfStart
                    | Instruction::ForAwaitOfNext
                    | Instruction::IteratorGetValueDone
            )
        });
        let has_async_generator_iterator_detach = bytecode
            .code
            .iter()
            .any(|instruction| matches!(instruction, Instruction::IteratorDetachPreserve));
        match bytecode.metadata.function_kind {
            FunctionKind::Generator => {
                if initial_yields.len() != 1
                    || !bytecode.metadata.has_prototype
                    || bytecode.metadata.constructor_kind != ConstructorKind::None
                    || bytecode.metadata.class_initializer_kind.is_some()
                    || parameter_body_pc.is_some_and(|body_pc| initial_yields[0] < body_pc)
                    || has_await
                    || has_async_delegation
                    || has_async_iteration
                    || has_async_generator_iterator_detach
                {
                    return Err(HeapError::Invariant(
                        "generator bytecode has invalid suspension metadata",
                    ));
                }
            }
            FunctionKind::Async => {
                if !initial_yields.is_empty()
                    || has_generator_only_instruction
                    || bytecode.metadata.has_prototype
                    || bytecode.metadata.constructor_kind != ConstructorKind::None
                    || bytecode.metadata.class_initializer_kind.is_some()
                    || has_async_generator_iterator_detach
                {
                    return Err(HeapError::Invariant(
                        "async bytecode has invalid execution metadata",
                    ));
                }
            }
            FunctionKind::AsyncGenerator => {
                if initial_yields.len() != 1
                    || !bytecode.metadata.has_prototype
                    || bytecode.metadata.constructor_kind != ConstructorKind::None
                    || bytecode.metadata.class_initializer_kind.is_some()
                    || parameter_body_pc.is_some_and(|body_pc| initial_yields[0] < body_pc)
                    || has_sync_delegation
                {
                    return Err(HeapError::Invariant(
                        "async-generator bytecode has invalid suspension metadata",
                    ));
                }
            }
            FunctionKind::Normal => {
                if !initial_yields.is_empty()
                    || has_generator_only_instruction
                    || has_await
                    || has_async_iteration
                {
                    return Err(HeapError::Invariant(
                        "non-async bytecode contains a suspension opcode",
                    ));
                }
            }
        }
        if bytecode.metadata.is_module
            && (!bytecode.metadata.strict
                || bytecode.metadata.function_kind != FunctionKind::Async
                || bytecode.metadata.eval_kind != EvalKind::None
                || bytecode.metadata.argument_count != 0
                || bytecode.metadata.defined_argument_count != 0
                || bytecode.metadata.rest_parameter.is_some()
                || bytecode.metadata.rest_pattern_start.is_some()
                || bytecode.metadata.parameter_environment_local_count != 0
                || bytecode.metadata.pattern_argument_count != 0
                || bytecode.metadata.parameter_pattern_end.is_some()
                || bytecode.parameter_environment.is_some()
                || bytecode.metadata.function_name_local.is_some()
                || bytecode.metadata.derived_this_local.is_some()
                || bytecode.metadata.active_function_local.is_some()
                || bytecode.metadata.eval_variable_object_local.is_some()
                || bytecode.metadata.super_call_allowed
                || bytecode.metadata.super_allowed
                || bytecode.metadata.arguments_forbidden
                || bytecode.metadata.needs_home_object
                || bytecode.metadata.has_prototype
                || bytecode.metadata.constructor_kind != ConstructorKind::None
                || bytecode.metadata.class_initializer_kind.is_some()
                || bytecode.metadata.class_private_brand)
        {
            return Err(HeapError::Invariant(
                "module bytecode has invalid root metadata",
            ));
        }
        if bytecode
            .metadata
            .function_name_local
            .is_some_and(|index| index >= bytecode.metadata.local_count)
        {
            return Err(HeapError::Invariant(
                "function-name local is outside bytecode local slots",
            ));
        }
        for (local, message) in [
            (
                bytecode.metadata.derived_this_local,
                "derived this local is outside bytecode local slots",
            ),
            (
                bytecode.metadata.active_function_local,
                "active-function local is outside bytecode local slots",
            ),
        ] {
            if local.is_some_and(|index| index >= bytecode.metadata.local_count) {
                return Err(HeapError::Invariant(message));
            }
        }
        if bytecode
            .metadata
            .eval_variable_object_local
            .is_some_and(|index| index >= bytecode.metadata.local_count)
        {
            return Err(HeapError::Invariant(
                "eval variable-object local is outside bytecode local slots",
            ));
        }
        if bytecode.metadata.eval_variable_object_local.is_some()
            && bytecode.metadata.eval_variable_object_local == bytecode.metadata.function_name_local
        {
            return Err(HeapError::Invariant(
                "eval variable-object and function-name locals overlap",
            ));
        }
        let arg_eval_variable_object_local = bytecode
            .parameter_environment
            .as_ref()
            .and_then(|layout| layout.arg_eval_variable_object_local);
        if arg_eval_variable_object_local
            .is_some_and(|index| index >= bytecode.metadata.local_count)
        {
            return Err(HeapError::Invariant(
                "parameter eval variable-object local is outside bytecode local slots",
            ));
        }
        if let Some(index) = arg_eval_variable_object_local
            && (bytecode.metadata.eval_variable_object_local == Some(index)
                || bytecode.metadata.function_name_local == Some(index))
        {
            return Err(HeapError::Invariant(
                "parameter eval variable-object local overlaps another private local",
            ));
        }
        let private_locals = [
            bytecode.metadata.function_name_local,
            bytecode.metadata.eval_variable_object_local,
            arg_eval_variable_object_local,
            bytecode.metadata.derived_this_local,
            bytecode.metadata.active_function_local,
        ];
        for (index, local) in private_locals.iter().enumerate() {
            if local.is_some()
                && private_locals[..index]
                    .iter()
                    .any(|earlier| earlier == local)
            {
                return Err(HeapError::Invariant("authenticated private locals overlap"));
            }
        }
        if bytecode.argument_definitions.len() != usize::from(bytecode.metadata.argument_count) {
            return Err(HeapError::Invariant(
                "argument definition count does not match bytecode metadata",
            ));
        }
        if bytecode.local_definitions.len() != usize::from(bytecode.metadata.local_count) {
            return Err(HeapError::Invariant(
                "local definition count does not match bytecode metadata",
            ));
        }
        validate_published_private_elements(self, &bytecode)?;
        let unnamed_arguments = bytecode
            .argument_definitions
            .iter()
            .map(|definition| definition.name.is_none())
            .collect::<Vec<_>>();
        let lexical_locals = bytecode
            .local_definitions
            .iter()
            .map(|definition| definition.is_lexical)
            .collect::<Vec<_>>();
        let const_locals = bytecode
            .local_definitions
            .iter()
            .map(|definition| definition.is_const)
            .collect::<Vec<_>>();
        validate_derived_constructor_bytecode_layout(
            &bytecode.metadata,
            &bytecode.code,
            &lexical_locals,
            &const_locals,
            &bytecode.closure_variables,
        )
        .map_err(HeapError::Invariant)?;
        validate_class_initializer_bytecode_layout(&bytecode.metadata, &bytecode.code)
            .map_err(HeapError::Invariant)?;
        let pattern_body_pc = bytecode
            .metadata
            .parameter_pattern_end
            .and_then(|marker| usize::try_from(marker).ok())
            .and_then(|marker| marker.checked_add(1));
        validate_parameter_initializer_scope_layout(
            &bytecode.metadata,
            &bytecode.code,
            parameter_body_pc.or(pattern_body_pc),
            &lexical_locals,
            &parameter_initializer_locals,
        )
        .map_err(HeapError::Invariant)?;
        validate_pattern_parameter_bytecode_layout(
            &bytecode.metadata,
            &bytecode.code,
            &unnamed_arguments,
            &lexical_locals,
            &parameter_initializer_locals,
            bytecode.parameter_environment.as_ref(),
        )
        .map_err(HeapError::Invariant)?;
        let parameter_initializer_capture_locals = parameter_initializer_visible_locals(
            &bytecode.metadata,
            &bytecode.code,
            parameter_body_pc,
            &parameter_initializer_locals,
            bytecode.parameter_environment.as_ref(),
        )
        .map_err(HeapError::Invariant)?;
        validate_eval_environment_phase_layout(
            &bytecode.eval_environments,
            EvalEnvironmentPhaseContext {
                metadata: &bytecode.metadata,
                code: &bytecode.code,
                parameter_body_pc,
                pattern_body_pc,
                lexical_locals: &lexical_locals,
                parameter_initializer_locals: &parameter_initializer_locals,
                parameter_initializer_visible_locals: parameter_initializer_capture_locals
                    .as_deref(),
                parameter_environment: bytecode.parameter_environment.as_ref(),
            },
        )
        .map_err(HeapError::Invariant)?;
        for definition in bytecode.argument_definitions.iter() {
            if definition.kind != ClosureVariableKind::Normal
                || definition.is_lexical
                || definition.is_const
                || definition.is_parameter_initializer
            {
                return Err(HeapError::Invariant(
                    "argument definition is not an ordinary mutable binding",
                ));
            }
        }
        if let Some(layout) = bytecode.parameter_environment.as_ref() {
            let parameter_definitions = bytecode
                .local_definitions
                .iter()
                .take(usize::from(
                    bytecode.metadata.parameter_environment_local_count,
                ))
                .collect::<Vec<_>>();
            for (index, local) in parameter_definitions.iter().enumerate() {
                if local.kind != ClosureVariableKind::Normal
                    || !local.is_lexical
                    || local.is_const
                    || local.is_parameter_initializer
                    || local.name.is_none()
                    || parameter_definitions[..index]
                        .iter()
                        .any(|earlier| earlier.name == local.name)
                {
                    return Err(HeapError::Invariant(
                        "parameter environment cell definition is not authenticated",
                    ));
                }
            }
            let mut mapped_arguments = vec![false; bytecode.argument_definitions.len()];
            for cell in layout.argument_cells.iter() {
                mapped_arguments[usize::from(cell.argument)] = true;
                let argument = &bytecode.argument_definitions[usize::from(cell.argument)];
                let local = &bytecode.local_definitions[usize::from(cell.parameter_local)];
                if argument.name.is_none() || argument.name != local.name {
                    return Err(HeapError::Invariant(
                        "parameter argument cell name disagrees with its physical argument",
                    ));
                }
            }
            if bytecode
                .argument_definitions
                .iter()
                .zip(mapped_arguments)
                .any(|(argument, mapped)| argument.name.is_some() != mapped)
            {
                return Err(HeapError::Invariant(
                    "parameter argument-cell map is not one-to-one with named arguments",
                ));
            }
            for copy in layout.pattern_copies.iter() {
                let source = &bytecode.local_definitions[usize::from(copy.parameter_local)];
                let target = &bytecode.local_definitions[usize::from(copy.body_local)];
                if target.kind != ClosureVariableKind::Normal
                    || target.is_lexical
                    || target.is_const
                    || source.is_parameter_initializer
                    || target.is_parameter_initializer
                    || source.name != target.name
                {
                    return Err(HeapError::Invariant(
                        "parameter pattern copy definitions are not same-name lexical-to-root storage",
                    ));
                }
            }
            if let Some(index) = layout.synthetic_arguments_local {
                let definition = &bytecode.local_definitions[usize::from(index)];
                if definition.kind != ClosureVariableKind::Normal
                    || !definition.is_lexical
                    || definition.is_const
                    || definition.is_parameter_initializer
                    || definition.name.is_none()
                {
                    return Err(HeapError::Invariant(
                        "synthetic parameter arguments definition is not authenticated",
                    ));
                }
            }
        }
        for (index, definition) in bytecode.local_definitions.iter().enumerate() {
            let is_function_name =
                bytecode.metadata.function_name_local == u16::try_from(index).ok();
            let is_derived_this = bytecode.metadata.derived_this_local == u16::try_from(index).ok();
            let is_active_function =
                bytecode.metadata.active_function_local == u16::try_from(index).ok();
            let is_eval_variable_object =
                bytecode.metadata.eval_variable_object_local == u16::try_from(index).ok();
            let is_arg_eval_variable_object =
                arg_eval_variable_object_local == u16::try_from(index).ok();
            if is_function_name {
                if definition.kind != ClosureVariableKind::FunctionName
                    || definition.is_lexical
                    || definition.is_const != bytecode.metadata.strict
                    || definition.name.is_none()
                {
                    return Err(HeapError::Invariant(
                        "function-name definition disagrees with bytecode metadata",
                    ));
                }
            } else if is_derived_this {
                if definition.kind != ClosureVariableKind::Normal
                    || !definition.is_lexical
                    || definition.is_const
                    || definition.is_parameter_initializer
                    || definition.name.is_none()
                {
                    return Err(HeapError::Invariant(
                        "derived this definition disagrees with bytecode metadata",
                    ));
                }
            } else if is_active_function {
                if definition.kind != ClosureVariableKind::Normal
                    || definition.is_lexical
                    || definition.is_const
                    || definition.is_parameter_initializer
                    || definition.name.is_none()
                {
                    return Err(HeapError::Invariant(
                        "active-function definition disagrees with bytecode metadata",
                    ));
                }
            } else if is_eval_variable_object {
                if definition.kind != ClosureVariableKind::EvalVariableObject
                    || definition.is_lexical
                    || definition.is_const
                    || definition.name.is_none()
                {
                    return Err(HeapError::Invariant(
                        "eval variable-object definition disagrees with bytecode metadata",
                    ));
                }
            } else if is_arg_eval_variable_object {
                if definition.kind != ClosureVariableKind::ArgEvalVariableObject
                    || definition.is_lexical
                    || definition.is_const
                    || definition.name.is_none()
                {
                    return Err(HeapError::Invariant(
                        "parameter eval variable-object definition disagrees with bytecode layout",
                    ));
                }
            } else if definition.kind == ClosureVariableKind::WithObject {
                if bytecode.metadata.strict
                    || definition.is_lexical
                    || definition.is_const
                    || definition.name.is_none()
                {
                    return Err(HeapError::Invariant(
                        "strict or malformed bytecode contains a with-object local",
                    ));
                }
            } else if definition.kind != ClosureVariableKind::Normal
                && !definition.kind.is_private()
            {
                return Err(HeapError::Invariant(
                    "ordinary local definition uses a non-local binding kind",
                ));
            } else if definition.is_const && !definition.is_lexical {
                return Err(HeapError::Invariant(
                    "a const local definition must also be lexical",
                ));
            }
        }
        if bytecode.closure_variables.len() != usize::from(bytecode.metadata.closure_count) {
            return Err(HeapError::Invariant(
                "function closure descriptor count does not match its bytecode metadata",
            ));
        }
        let mut owned_name_atoms = HashMap::<Atom, usize>::new();
        for atom in bytecode.auxiliary_atoms.iter().copied() {
            *owned_name_atoms.entry(atom).or_default() += 1;
        }
        if let Some(debug) = &bytecode.debug {
            if debug.filename.is_null() {
                return Err(HeapError::Invariant(
                    "bytecode debug filename is the null atom",
                ));
            }
            let Some(count) = owned_name_atoms.get_mut(&debug.filename) else {
                return Err(HeapError::Invariant(
                    "debug filename atom is not owned by bytecode metadata",
                ));
            };
            if *count == 0 {
                return Err(HeapError::Invariant(
                    "debug filename atom ownership multiplicity is too small",
                ));
            }
            *count -= 1;
            if let Some(table) = &debug.pc2line {
                if table.definition.line == u32::MAX || table.definition.column == u32::MAX {
                    return Err(HeapError::Invariant(
                        "bytecode debug definition cannot be represented one-based",
                    ));
                }
                let mut previous_pc = None;
                for entry in &table.entries {
                    if usize::try_from(entry.pc)
                        .ok()
                        .is_none_or(|pc| pc >= bytecode.code.len())
                    {
                        return Err(HeapError::Invariant(
                            "bytecode debug PC is outside the instruction stream",
                        ));
                    }
                    if previous_pc.is_some_and(|previous| entry.pc < previous) {
                        return Err(HeapError::Invariant("bytecode debug PCs are not ordered"));
                    }
                    if entry.position.line == u32::MAX || entry.position.column == u32::MAX {
                        return Err(HeapError::Invariant(
                            "bytecode debug position cannot be represented one-based",
                        ));
                    }
                    previous_pc = Some(entry.pc);
                }
            }
        }
        for definition in bytecode
            .argument_definitions
            .iter()
            .chain(bytecode.local_definitions.iter())
        {
            if definition.kind == ClosureVariableKind::ModuleImportView {
                return Err(HeapError::Invariant(
                    "module-import view escaped into a variable definition",
                ));
            }
            let Some(atom) = definition.name else {
                continue;
            };
            let Some(count) = owned_name_atoms.get_mut(&atom) else {
                return Err(HeapError::Invariant(
                    "variable-definition name atom is not owned by bytecode metadata",
                ));
            };
            if *count == 0 {
                return Err(HeapError::Invariant(
                    "variable-definition name atom ownership multiplicity is too small",
                ));
            }
            *count -= 1;
        }
        let mut global_declaration_names = HashMap::new();
        let mut import_meta_count = 0_u8;
        for descriptor in bytecode.closure_variables.iter().copied() {
            if bytecode.metadata.is_module {
                if !matches!(
                    descriptor.source,
                    ClosureSource::ModuleDeclaration
                        | ClosureSource::ModuleImport
                        | ClosureSource::ModuleImportCollision
                        | ClosureSource::ModuleImportMeta
                        | ClosureSource::Global
                ) {
                    return Err(HeapError::Invariant(
                        "module root closure descriptor used a non-module source",
                    ));
                }
            } else if matches!(
                descriptor.source,
                ClosureSource::ModuleDeclaration
                    | ClosureSource::ModuleImport
                    | ClosureSource::ModuleImportCollision
                    | ClosureSource::ModuleImportMeta
            ) {
                return Err(HeapError::Invariant(
                    "module closure descriptor escaped module bytecode",
                ));
            }
            match descriptor.source {
                ClosureSource::ModuleDeclaration
                    if descriptor.kind != ClosureVariableKind::Normal
                        || (descriptor.is_const && !descriptor.is_lexical) =>
                {
                    return Err(HeapError::Invariant(
                        "module declaration descriptor has invalid binding metadata",
                    ));
                }
                ClosureSource::ModuleImport
                    if descriptor.kind != ClosureVariableKind::ModuleImportView
                        || !descriptor.is_lexical
                        || !descriptor.is_const =>
                {
                    return Err(HeapError::Invariant(
                        "module import descriptor has invalid binding metadata",
                    ));
                }
                ClosureSource::ModuleImportCollision
                    if !descriptor.is_lexical
                        || !descriptor.is_const
                        || !matches!(
                            descriptor.kind,
                            ClosureVariableKind::Normal | ClosureVariableKind::ModuleImportView
                        ) =>
                {
                    return Err(HeapError::Invariant(
                        "module import collision descriptor has invalid binding metadata",
                    ));
                }
                ClosureSource::ModuleImportMeta
                    if descriptor.kind != ClosureVariableKind::Normal
                        || !descriptor.is_lexical
                        || !descriptor.is_const =>
                {
                    return Err(HeapError::Invariant(
                        "import.meta descriptor has invalid binding metadata",
                    ));
                }
                _ => {}
            }
            if descriptor.source == ClosureSource::ModuleImportMeta {
                import_meta_count = import_meta_count.saturating_add(1);
                if import_meta_count > 1 {
                    return Err(HeapError::Invariant(
                        "module bytecode contains more than one import.meta binding",
                    ));
                }
            }
            if descriptor.kind == ClosureVariableKind::ModuleImportView
                && (!descriptor.is_lexical
                    || !descriptor.is_const
                    || !matches!(
                        descriptor.source,
                        ClosureSource::ModuleImport
                            | ClosureSource::ModuleImportCollision
                            | ClosureSource::ParentClosure(_)
                            | ClosureSource::EvalEnvironment(_)
                    ))
            {
                return Err(HeapError::Invariant(
                    "module-import view descriptor has invalid provenance",
                ));
            }
            if matches!(descriptor.source, ClosureSource::EvalEnvironment(_))
                && bytecode.metadata.eval_kind != EvalKind::Direct
            {
                return Err(HeapError::Invariant(
                    "eval-environment closure escaped a direct-eval root",
                ));
            }
            if descriptor.kind == ClosureVariableKind::GlobalFunction
                && (descriptor.is_lexical || descriptor.is_const)
            {
                return Err(HeapError::Invariant(
                    "global function declaration descriptor has lexical metadata",
                ));
            }
            if descriptor.kind.is_eval_variable_object()
                && (descriptor.is_lexical
                    || descriptor.is_const
                    || !matches!(
                        descriptor.source,
                        ClosureSource::ParentLocal(_)
                            | ClosureSource::ParentClosure(_)
                            | ClosureSource::EvalEnvironment(_)
                    ))
            {
                return Err(HeapError::Invariant(
                    "eval variable-object descriptor has invalid binding metadata",
                ));
            }
            if descriptor.kind == ClosureVariableKind::WithObject
                && (descriptor.is_lexical
                    || descriptor.is_const
                    || !matches!(
                        descriptor.source,
                        ClosureSource::ParentLocal(_)
                            | ClosureSource::ParentClosure(_)
                            | ClosureSource::EvalEnvironment(_)
                    ))
            {
                return Err(HeapError::Invariant(
                    "with-object descriptor has invalid binding metadata",
                ));
            }
            if descriptor.is_const
                && !descriptor.is_lexical
                && descriptor.kind != ClosureVariableKind::FunctionName
            {
                return Err(HeapError::Invariant(
                    "a const closure descriptor must also be lexical",
                ));
            }
            if (descriptor.source == ClosureSource::GlobalDeclaration
                && !matches!(
                    descriptor.kind,
                    ClosureVariableKind::Normal | ClosureVariableKind::GlobalFunction
                ))
                || (descriptor.source == ClosureSource::Global
                    && descriptor.kind != ClosureVariableKind::Normal)
                || (matches!(descriptor.source, ClosureSource::ParentGlobal(_))
                    && !matches!(
                        descriptor.kind,
                        ClosureVariableKind::Normal | ClosureVariableKind::GlobalFunction
                    ))
            {
                return Err(HeapError::Invariant(
                    "global declaration descriptor has non-global binding metadata",
                ));
            }
            if descriptor.kind == ClosureVariableKind::GlobalFunction
                && !matches!(
                    descriptor.source,
                    ClosureSource::GlobalDeclaration | ClosureSource::ParentGlobal(_)
                )
            {
                return Err(HeapError::Invariant(
                    "global function binding kind escaped a declaration relay",
                ));
            }
            let requires_name = matches!(
                descriptor.source,
                ClosureSource::GlobalDeclaration
                    | ClosureSource::Global
                    | ClosureSource::ParentGlobal(_)
                    | ClosureSource::EvalEnvironment(_)
                    | ClosureSource::ModuleDeclaration
                    | ClosureSource::ModuleImport
                    | ClosureSource::ModuleImportCollision
                    | ClosureSource::ModuleImportMeta
            ) || matches!(
                descriptor.kind,
                ClosureVariableKind::FunctionName
                    | ClosureVariableKind::EvalVariableObject
                    | ClosureVariableKind::ArgEvalVariableObject
                    | ClosureVariableKind::WithObject
            ) || descriptor.kind.is_private();
            let allows_name = requires_name
                || descriptor.is_lexical
                || matches!(
                    descriptor.source,
                    ClosureSource::ParentLocal(_)
                        | ClosureSource::ParentArgument(_)
                        | ClosureSource::ParentClosure(_)
                );
            if descriptor.source == ClosureSource::GlobalDeclaration
                && let ClosureVariableName::Atom(atom) = descriptor.name
            {
                match global_declaration_names.entry(atom) {
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        entry.insert((descriptor.is_lexical, descriptor.is_lexical));
                    }
                    std::collections::hash_map::Entry::Occupied(mut entry) => {
                        let (first_is_lexical, seen_lexical) = *entry.get();
                        if first_is_lexical
                            && seen_lexical
                            && (descriptor.is_lexical
                                || descriptor.kind != ClosureVariableKind::GlobalFunction)
                        {
                            return Err(HeapError::Invariant(
                                "duplicate lexical global declaration descriptor name",
                            ));
                        }
                        // A first sloppy Annex B normal record masks every
                        // later same-name declaration in QuickJS's conflict
                        // lookup, including repeated lexical and var records.
                        // A first lexical remains stricter.
                        if descriptor.is_lexical {
                            entry.get_mut().1 = true;
                        }
                    }
                }
            }
            if matches!(
                descriptor.source,
                ClosureSource::GlobalDeclaration
                    | ClosureSource::Global
                    | ClosureSource::ParentGlobal(_)
            ) && !matches!(
                descriptor.kind,
                ClosureVariableKind::Normal | ClosureVariableKind::GlobalFunction
            ) {
                return Err(HeapError::Invariant(
                    "published global closure descriptor has a non-global binding kind",
                ));
            }
            match descriptor.name {
                ClosureVariableName::Atom(atom) if allows_name => {
                    let Some(count) = owned_name_atoms.get_mut(&atom) else {
                        return Err(HeapError::Invariant(
                            "closure name atom is not owned by bytecode metadata",
                        ));
                    };
                    if *count == 0 {
                        return Err(HeapError::Invariant(
                            "closure name atom ownership multiplicity is too small",
                        ));
                    }
                    *count -= 1;
                }
                ClosureVariableName::None if !requires_name => {}
                ClosureVariableName::Constant(_) => {
                    return Err(HeapError::Invariant(
                        "published closure descriptor retained an unlinked name constant",
                    ));
                }
                ClosureVariableName::None | ClosureVariableName::Atom(_) => {
                    return Err(HeapError::Invariant(
                        "published closure descriptor name does not match its binding kind",
                    ));
                }
            }
        }
        for environment in bytecode.eval_environments.iter() {
            let first_function_anchor = environment
                .scopes
                .iter()
                .position(|scope| {
                    matches!(
                        scope.kind,
                        EvalScopeKind::FunctionRoot | EvalScopeKind::Parameter
                    )
                })
                .and_then(|scope| u16::try_from(scope).ok())
                .ok_or(HeapError::Invariant(
                    "eval environment contains no representable current function anchor",
                ))?;
            match environment.variable_environment {
                EvalVariableEnvironment::Global => {
                    let current_body_is_program = first_function_anchor
                        .checked_sub(1)
                        .and_then(|scope| environment.scopes.get(usize::from(scope)))
                        .is_some_and(|scope| scope.kind == EvalScopeKind::ProgramBody);
                    if bytecode.metadata.is_module
                        || !current_body_is_program
                        || (environment.caller_strict
                            && bytecode.metadata.eval_kind != EvalKind::None)
                    {
                        return Err(HeapError::Invariant(
                            "global eval variable environment escaped an authored Script root",
                        ));
                    }
                }
                EvalVariableEnvironment::StrictLocal(scope) if environment.caller_strict => {
                    if scope != first_function_anchor {
                        return Err(HeapError::Invariant(
                            "strict eval variable environment selected the wrong current function segment",
                        ));
                    }
                    let current_body_is_program = first_function_anchor
                        .checked_sub(1)
                        .and_then(|scope| environment.scopes.get(usize::from(scope)))
                        .is_some_and(|scope| scope.kind == EvalScopeKind::ProgramBody);
                    if current_body_is_program
                        && bytecode.metadata.eval_kind == EvalKind::None
                        && !bytecode.metadata.is_module
                    {
                        return Err(HeapError::Invariant(
                            "authored Script eval environment used a non-canonical strict-local target",
                        ));
                    }
                    let Some(scope) = environment.scopes.get(usize::from(scope)) else {
                        return Err(HeapError::Invariant(
                            "strict eval variable-environment scope is out of bounds",
                        ));
                    };
                    if !matches!(
                        scope.kind,
                        EvalScopeKind::FunctionRoot | EvalScopeKind::Parameter
                    ) {
                        return Err(HeapError::Invariant(
                            "strict eval variable environment has the wrong function segment anchor",
                        ));
                    }
                }
                EvalVariableEnvironment::VariableObject { scope, source }
                    if !environment.caller_strict =>
                {
                    let target_matches_function_segment =
                        if bytecode.metadata.eval_kind == EvalKind::None {
                            scope == first_function_anchor
                                && matches!(source, EvalBindingSource::Local(_))
                        } else {
                            bytecode.metadata.eval_kind == EvalKind::Direct
                                && scope > first_function_anchor
                                && matches!(source, EvalBindingSource::Closure(_))
                        };
                    if !target_matches_function_segment {
                        return Err(HeapError::Invariant(
                            "eval variable object selected the wrong current function segment",
                        ));
                    }
                    let Some(scope) = environment.scopes.get(usize::from(scope)) else {
                        return Err(HeapError::Invariant(
                            "eval variable-object scope is out of bounds",
                        ));
                    };
                    let expected_kind = match scope.kind {
                        EvalScopeKind::FunctionRoot => ClosureVariableKind::EvalVariableObject,
                        EvalScopeKind::Parameter => ClosureVariableKind::ArgEvalVariableObject,
                        _ => {
                            return Err(HeapError::Invariant(
                                "eval variable object has the wrong function segment scope",
                            ));
                        }
                    };
                    if matches!(source, EvalBindingSource::Argument(_))
                        || scope
                            .bindings
                            .iter()
                            .filter(|binding| {
                                binding.source == source && binding.kind == expected_kind
                            })
                            .count()
                            != 1
                    {
                        return Err(HeapError::Invariant(
                            "eval variable-object target is not exact",
                        ));
                    }
                }
                EvalVariableEnvironment::StrictLocal(_)
                | EvalVariableEnvironment::VariableObject { .. } => {
                    return Err(HeapError::Invariant(
                        "eval variable environment disagrees with caller strictness",
                    ));
                }
            }
            for scope in environment.scopes.iter() {
                if scope.kind == EvalScopeKind::With && scope.bindings.len() != 1 {
                    return Err(HeapError::Invariant(
                        "eval with scope does not contain exactly one object binding",
                    ));
                }
                for binding in &scope.bindings {
                    if binding.name.is_null() {
                        return Err(HeapError::Invariant("eval binding name is the null atom"));
                    }
                    let source_name_matches = match binding.source {
                        EvalBindingSource::Local(index) => bytecode
                            .local_definitions
                            .get(usize::from(index))
                            .is_some_and(|definition| definition.name == Some(binding.name)),
                        EvalBindingSource::Argument(index) => bytecode
                            .argument_definitions
                            .get(usize::from(index))
                            .is_some_and(|definition| definition.name == Some(binding.name)),
                        EvalBindingSource::Closure(index) => bytecode
                            .closure_variables
                            .get(usize::from(index))
                            .is_some_and(|descriptor| {
                                descriptor.name == ClosureVariableName::Atom(binding.name)
                            }),
                    };
                    if !source_name_matches {
                        return Err(HeapError::Invariant(
                            "eval binding name atom disagrees with its source metadata",
                        ));
                    }
                    if binding.is_catch_parameter
                        && (scope.kind != EvalScopeKind::Catch
                            || !binding.is_lexical
                            || binding.is_const
                            || binding.kind != ClosureVariableKind::Normal)
                    {
                        return Err(HeapError::Invariant(
                            "eval catch binding metadata disagrees with its scope",
                        ));
                    }
                    if (binding.kind == ClosureVariableKind::WithObject)
                        != (scope.kind == EvalScopeKind::With)
                        || (binding.kind == ClosureVariableKind::WithObject
                            && (binding.is_lexical
                                || binding.is_const
                                || binding.is_catch_parameter
                                || matches!(binding.source, EvalBindingSource::Argument(_))))
                    {
                        return Err(HeapError::Invariant(
                            "eval with-object binding metadata disagrees with its scope",
                        ));
                    }
                    if binding.kind.is_eval_variable_object() {
                        let role_allowed = match scope.kind {
                            EvalScopeKind::FunctionRoot => true,
                            EvalScopeKind::Parameter => {
                                binding.kind == ClosureVariableKind::ArgEvalVariableObject
                            }
                            _ => false,
                        };
                        if !role_allowed
                            || binding.is_lexical
                            || binding.is_const
                            || binding.is_catch_parameter
                        {
                            return Err(HeapError::Invariant(
                                "eval variable-object binding has invalid metadata",
                            ));
                        }
                        let authenticated = match binding.source {
                            EvalBindingSource::Local(index) => {
                                let expected = match binding.kind {
                                    ClosureVariableKind::EvalVariableObject => {
                                        bytecode.metadata.eval_variable_object_local
                                    }
                                    ClosureVariableKind::ArgEvalVariableObject => {
                                        arg_eval_variable_object_local
                                    }
                                    _ => {
                                        unreachable!("eval variable-object role was checked above")
                                    }
                                };
                                expected == Some(index)
                                    && bytecode
                                        .local_definitions
                                        .get(usize::from(index))
                                        .is_some_and(|definition| definition.kind == binding.kind)
                            }
                            EvalBindingSource::Closure(index) => bytecode
                                .closure_variables
                                .get(usize::from(index))
                                .is_some_and(|descriptor| descriptor.kind == binding.kind),
                            EvalBindingSource::Argument(_) => false,
                        };
                        if !authenticated {
                            return Err(HeapError::Invariant(
                                "eval variable-object binding source is not authenticated",
                            ));
                        }
                    }
                    if binding.kind == ClosureVariableKind::WithObject {
                        let authenticated = match binding.source {
                            EvalBindingSource::Local(index) => bytecode
                                .local_definitions
                                .get(usize::from(index))
                                .is_some_and(|definition| {
                                    definition.kind == ClosureVariableKind::WithObject
                                        && !definition.is_lexical
                                        && !definition.is_const
                                }),
                            EvalBindingSource::Closure(index) => bytecode
                                .closure_variables
                                .get(usize::from(index))
                                .is_some_and(|descriptor| {
                                    descriptor.kind == ClosureVariableKind::WithObject
                                        && !descriptor.is_lexical
                                        && !descriptor.is_const
                                }),
                            EvalBindingSource::Argument(_) => false,
                        };
                        if !authenticated {
                            return Err(HeapError::Invariant(
                                "eval with-object binding source is not authenticated",
                            ));
                        }
                    }
                    let Some(count) = owned_name_atoms.get_mut(&binding.name) else {
                        return Err(HeapError::Invariant(
                            "eval binding name atom is not owned by bytecode metadata",
                        ));
                    };
                    if *count == 0 {
                        return Err(HeapError::Invariant(
                            "eval binding name atom ownership multiplicity is too small",
                        ));
                    }
                    *count -= 1;
                }
            }
        }
        crate::bytecode::verify_parts(
            &bytecode.code,
            bytecode.constants.len(),
            bytecode.metadata.max_stack,
        )
        .map_err(|_| HeapError::Invariant("function bytecode failed generic verification"))?;
        let (index, generation) = self.reserve(HeapNodeKind::FunctionBytecode)?;
        let id = FunctionBytecodeId { index, generation };
        let edges = function_bytecode_edges(&bytecode);

        if let Err(error) = self.retain_edges_transactionally(&edges) {
            self.abort_initializing(index)?;
            return Err(error);
        }

        self.publish(index, NodeData::FunctionBytecode(bytecode))?;
        Ok(id)
    }

    /// Allocate a captured-variable cell and transfer ownership of its value
    /// to the heap. The returned reference is normally owned by the active
    /// frame; closure objects retain the same `VarRefId` when published.
    pub fn allocate_var_ref(&mut self, var_ref: VarRefData) -> Result<VarRefId, HeapError> {
        validate_var_ref_payload(&var_ref)?;
        let (index, generation) = self.reserve(HeapNodeKind::VarRef)?;
        let id = VarRefId { index, generation };
        let edges = var_ref_edges(&var_ref);

        if let Err(error) = self.retain_edges_transactionally(&edges) {
            self.abort_initializing(index)?;
            return Err(error);
        }

        self.publish(index, NodeData::VarRef(var_ref))?;
        Ok(id)
    }
    /// Read one live object record.
    pub fn object(&self, id: ObjectId) -> Result<&ObjectData, HeapError> {
        match self.live_node(RawId::Object(id))?.data {
            NodeData::Object(ref object) => Ok(object),
            NodeData::Shape(_)
            | NodeData::VarRef(_)
            | NodeData::Context(_)
            | NodeData::FunctionBytecode(_) => Err(HeapError::Invariant(
                "typed object lookup reached another node payload",
            )),
        }
    }

    /// Set QuickJS's identity-local Annex B `is_HTMLDDA` bit.
    #[cfg(feature = "test262-host")]
    pub(crate) fn set_object_is_html_dda(&mut self, id: ObjectId) -> Result<(), HeapError> {
        self.object_mut(id)?.is_html_dda = true;
        Ok(())
    }

    /// Read the private-method brand owned by an object's HomeObject slot.
    ///
    /// The atom is an internal identity rather than an ECMAScript property.
    /// Its ownership remains with the object until finalization.
    pub fn object_private_brand_home(&self, id: ObjectId) -> Result<Option<Atom>, HeapError> {
        Ok(self.object(id)?.private_brand_home)
    }

    /// Attach the freshly allocated private-method brand for one class side.
    ///
    /// The caller transfers one owned atom reference on success. A HomeObject
    /// has exactly one brand even when the class declares several methods.
    pub fn attach_object_private_brand_home(
        &mut self,
        id: ObjectId,
        brand: Atom,
    ) -> Result<(), HeapError> {
        let object = self.object_mut(id)?;
        if object.private_brand_home.is_some() {
            return Err(HeapError::Invariant(
                "private-method HomeObject already has a brand",
            ));
        }
        object.private_brand_home = Some(brand);
        Ok(())
    }

    /// Read the optional HomeObject edge of one bytecode function.
    ///
    /// Native, bound, and ordinary objects are rejected rather than silently
    /// impersonating bytecode functions at the super-resolution boundary.
    pub fn bytecode_function_home_object(
        &self,
        id: ObjectId,
    ) -> Result<Option<ObjectId>, HeapError> {
        let ObjectPayload::BytecodeFunction { home_object, .. } = &self.object(id)?.payload else {
            return Err(HeapError::Invariant(
                "HomeObject lookup reached a non-bytecode function",
            ));
        };
        Ok(*home_object)
    }

    /// Read the hidden public-instance-field initializer attached to one class
    /// constructor bytecode function.
    pub fn bytecode_class_instance_initializer(
        &self,
        id: ObjectId,
    ) -> Result<Option<ObjectId>, HeapError> {
        let ObjectPayload::BytecodeFunction {
            class_instance_initializer,
            ..
        } = &self.object(id)?.payload
        else {
            return Err(HeapError::Invariant(
                "class initializer lookup reached a non-bytecode function",
            ));
        };
        Ok(*class_instance_initializer)
    }

    /// Read one live shape record.
    pub fn shape(&self, id: ShapeId) -> Result<&Shape, HeapError> {
        match self.live_node(RawId::Shape(id))?.data {
            NodeData::Shape(ref shape) => Ok(shape),
            NodeData::Object(_)
            | NodeData::VarRef(_)
            | NodeData::Context(_)
            | NodeData::FunctionBytecode(_) => Err(HeapError::Invariant(
                "typed shape lookup reached another node payload",
            )),
        }
    }

    fn shape_mut(&mut self, id: ShapeId) -> Result<&mut Shape, HeapError> {
        match self.live_node_mut(RawId::Shape(id))?.data {
            NodeData::Shape(ref mut shape) => Ok(shape),
            NodeData::Object(_)
            | NodeData::VarRef(_)
            | NodeData::Context(_)
            | NodeData::FunctionBytecode(_) => Err(HeapError::Invariant(
                "typed mutable shape lookup reached another node payload",
            )),
        }
    }

    /// Read one captured-variable cell. All functions holding the same
    /// `VarRefId` observe this shared value.
    pub fn var_ref(&self, id: VarRefId) -> Result<&VarRefData, HeapError> {
        match self.live_node(RawId::VarRef(id))?.data {
            NodeData::VarRef(ref var_ref) => Ok(var_ref),
            NodeData::Object(_)
            | NodeData::Shape(_)
            | NodeData::Context(_)
            | NodeData::FunctionBytecode(_) => Err(HeapError::Invariant(
                "typed var-ref lookup reached another node payload",
            )),
        }
    }

    /// Read one live context record.
    pub fn context(&self, id: ContextId) -> Result<&ContextData, HeapError> {
        match self.live_node(RawId::Context(id))?.data {
            NodeData::Context(ref context) => Ok(context),
            NodeData::Object(_)
            | NodeData::Shape(_)
            | NodeData::VarRef(_)
            | NodeData::FunctionBytecode(_) => Err(HeapError::Invariant(
                "typed context lookup reached another node payload",
            )),
        }
    }

    /// Return the oldest live loaded module with `name` in this Context.
    pub(crate) fn first_loaded_module(
        &self,
        cache: ContextId,
        name: &JsString,
    ) -> Result<Option<RawModuleRef>, HeapError> {
        let context = self.context(cache)?;
        let Some(module) = context.loaded_modules.first_by_name.get(name).copied() else {
            return Ok(None);
        };
        let record = context
            .loaded_modules
            .records
            .get(module.0)
            .and_then(Option::as_ref)
            .ok_or(HeapError::Invariant(
                "loaded-module name index references a tombstone",
            ))?;
        if matches!(&record.body, RawModuleRecordBody::Aborted) {
            return Err(HeapError::Invariant(
                "loaded-module name index references an aborted record",
            ));
        }
        Ok(Some(RawModuleRef { cache, module }))
    }

    /// Clone a borrowed snapshot of one Context-owned module record.
    /// Raw identities in the result are not independently retained.
    pub(crate) fn loaded_module(&self, module: RawModuleRef) -> Result<RawModuleRecord, HeapError> {
        self.context(module.cache)?
            .loaded_modules
            .records
            .get(module.module.0)
            .and_then(Option::as_ref)
            .cloned()
            .ok_or(HeapError::Invariant(
                "loaded-module identity is out of bounds or tombstoned",
            ))
    }

    /// Return whether an append-only module identity still names a live
    /// record. A rollback tombstone or retained `Aborted` identity is a stable
    /// non-live state; out-of-range identities and stale Contexts remain
    /// checked heap errors.
    pub(crate) fn loaded_module_is_live(&self, module: RawModuleRef) -> Result<bool, HeapError> {
        let record = self
            .context(module.cache)?
            .loaded_modules
            .records
            .get(module.module.0)
            .ok_or(HeapError::Invariant(
                "loaded-module identity is out of bounds",
            ))?;
        Ok(record
            .as_ref()
            .is_some_and(|record| !matches!(&record.body, RawModuleRecordBody::Aborted)))
    }

    /// Borrowed snapshots of every live record in construction order.
    /// Stable `Aborted` identities are deliberately omitted.
    pub(crate) fn loaded_modules(
        &self,
        cache: ContextId,
    ) -> Result<Vec<(ModuleId, RawModuleRecord)>, HeapError> {
        Ok(self
            .context(cache)?
            .loaded_modules
            .records
            .iter()
            .enumerate()
            .filter_map(|(index, record)| {
                record.as_ref().and_then(|record| {
                    (!matches!(&record.body, RawModuleRecordBody::Aborted))
                        .then(|| (ModuleId(index), record.clone()))
                })
            })
            .collect())
    }

    #[cfg(test)]
    pub(crate) fn loaded_module_slot_count(&self, cache: ContextId) -> Result<usize, HeapError> {
        Ok(self.context(cache)?.loaded_modules.records.len())
    }

    /// Apply one sealed metadata-only transition without allocating or
    /// perturbing any heap/atom ownership.
    pub(crate) fn transition_loaded_module(
        &mut self,
        module: RawModuleRef,
        transition: RawModuleTransition,
    ) -> Result<(), HeapError> {
        let current = self.loaded_module(module)?;
        match &current.body {
            RawModuleRecordBody::Aborted => {
                return Err(HeapError::Invariant(
                    "loaded-module transition targeted an aborted identity",
                ));
            }
            RawModuleRecordBody::Parsing
                if !matches!(
                    &transition,
                    RawModuleTransition::BeginResolution
                        | RawModuleTransition::FinishResolution(_)
                        | RawModuleTransition::FailResolution
                        | RawModuleTransition::ResetResolution
                ) =>
            {
                return Err(HeapError::Invariant(
                    "parse-in-progress module received an executable-state transition",
                ));
            }
            RawModuleRecordBody::Parsing
            | RawModuleRecordBody::SourceText { .. }
            | RawModuleRecordBody::Json { .. } => {}
        }
        if matches!(&transition, RawModuleTransition::FailResolution)
            && !matches!(&current.body, RawModuleRecordBody::Parsing)
        {
            return Err(HeapError::Invariant(
                "loaded-module resolution failure did not target Parsing state",
            ));
        }
        if let RawModuleTransition::FinishResolution(dependencies) = &transition {
            let context = self.context(module.cache)?;
            for dependency in dependencies.iter().copied() {
                if context
                    .loaded_modules
                    .records
                    .get(dependency.0)
                    .and_then(Option::as_ref)
                    .is_none()
                {
                    return Err(HeapError::Invariant(
                        "resolved loaded-module transition references a missing cache record",
                    ));
                }
            }
            if matches!(&current.body, RawModuleRecordBody::Parsing)
                && dependencies.len() > current.requested_modules.len()
            {
                return Err(HeapError::Invariant(
                    "parse-in-progress module resolved beyond its request prefix",
                ));
            }
        }
        if let RawModuleTransition::FinishNamespace(namespace) = &transition {
            if self.object(*namespace)?.kind != ObjectKind::ModuleNamespace {
                return Err(HeapError::Invariant(
                    "loaded-module namespace transition has the wrong object class",
                ));
            }
        }
        if let RawModuleTransition::FinishEvaluation { cycle_root } = &transition {
            if self
                .context(module.cache)?
                .loaded_modules
                .records
                .get(cycle_root.0)
                .and_then(Option::as_ref)
                .is_none()
            {
                return Err(HeapError::Invariant(
                    "loaded-module evaluation transition has a missing cycle root",
                ));
            }
        }

        if matches!(
            &transition,
            RawModuleTransition::BeginLink | RawModuleTransition::FinishLink
        ) {
            let record = current;
            self.validate_loaded_module_record(module.cache, Some(module.module), &record)?;
            let instance = record.instance.as_ref().ok_or(HeapError::Invariant(
                "loaded-module link transition has no instance",
            ))?;
            if matches!(&transition, RawModuleTransition::FinishLink) {
                if instance.slots.iter().any(Option::is_none) {
                    return Err(HeapError::Invariant(
                        "loaded-module link transition has unresolved closure slots",
                    ));
                }
                if matches!(record.body, RawModuleRecordBody::SourceText { .. })
                    && instance.callable.is_none()
                {
                    return Err(HeapError::Invariant(
                        "loaded-module link transition has no source callable",
                    ));
                }
            }
        }

        let record = self.loaded_module_record_mut(module)?;
        match transition {
            RawModuleTransition::BeginResolution => match record.resolution {
                RawModuleResolutionState::Unresolved => {
                    record.resolution = RawModuleResolutionState::Resolving;
                }
                _ => {
                    return Err(HeapError::Invariant(
                        "loaded-module resolution did not begin from Unresolved",
                    ));
                }
            },
            RawModuleTransition::FinishResolution(dependencies) => match record.resolution {
                RawModuleResolutionState::Resolving => {
                    record.resolution = RawModuleResolutionState::Resolved(dependencies);
                }
                _ => {
                    return Err(HeapError::Invariant(
                        "loaded-module resolution did not finish from Resolving",
                    ));
                }
            },
            RawModuleTransition::FailResolution => match record.resolution {
                RawModuleResolutionState::Resolving | RawModuleResolutionState::Resolved(_) => {
                    record.resolution = RawModuleResolutionState::Failed;
                }
                _ => {
                    return Err(HeapError::Invariant(
                        "loaded-module resolution failure did not target an active latch",
                    ));
                }
            },
            RawModuleTransition::ResetResolution => match record.resolution {
                RawModuleResolutionState::Resolving => {
                    record.resolution = RawModuleResolutionState::Unresolved;
                }
                _ => {
                    return Err(HeapError::Invariant(
                        "loaded-module resolution reset did not target Resolving",
                    ));
                }
            },
            RawModuleTransition::BeginLink => match record.link_status {
                RawModuleLinkStatus::Unlinked
                    if matches!(record.resolution, RawModuleResolutionState::Resolved(_))
                        && record.instance.is_some()
                        && record.link_realm.is_some() =>
                {
                    record.link_status = RawModuleLinkStatus::Linking;
                }
                _ => {
                    return Err(HeapError::Invariant(
                        "loaded-module link did not begin from resolved instantiated Unlinked state",
                    ));
                }
            },
            RawModuleTransition::FinishLink => match record.link_status {
                RawModuleLinkStatus::Linking if record.instance.is_some() => {
                    record.link_status = RawModuleLinkStatus::Linked;
                }
                _ => {
                    return Err(HeapError::Invariant(
                        "loaded-module link did not finish from instantiated Linking state",
                    ));
                }
            },
            RawModuleTransition::ResetLink => match record.link_status {
                RawModuleLinkStatus::Linking => record.link_status = RawModuleLinkStatus::Unlinked,
                _ => {
                    return Err(HeapError::Invariant(
                        "loaded-module link reset did not target Linking",
                    ));
                }
            },
            RawModuleTransition::PoisonLink => match record.link_status {
                RawModuleLinkStatus::Linking => record.link_status = RawModuleLinkStatus::Poisoned,
                _ => {
                    return Err(HeapError::Invariant(
                        "loaded-module link poison did not target Linking",
                    ));
                }
            },
            RawModuleTransition::BeginEvaluation => match record.evaluation {
                RawModuleEvaluationState::Unevaluated
                    if matches!(record.link_status, RawModuleLinkStatus::Linked)
                        && record.evaluation_cycle_root.is_none()
                        && record.async_evaluation_order.is_none()
                        && record.pending_async_dependencies == 0
                        && record.async_parent_modules.is_empty() =>
                {
                    record.evaluation = RawModuleEvaluationState::Evaluating;
                }
                _ => {
                    return Err(HeapError::Invariant(
                        "loaded-module evaluation did not begin from linked Unevaluated state",
                    ));
                }
            },
            RawModuleTransition::FinishEvaluation { cycle_root } => match record.evaluation {
                RawModuleEvaluationState::Evaluating
                    if matches!(record.link_status, RawModuleLinkStatus::Linked) =>
                {
                    if record.async_evaluation_order.is_some() {
                        record.evaluation = RawModuleEvaluationState::EvaluatingAsync;
                    } else {
                        if record.has_top_level_await || record.pending_async_dependencies != 0 {
                            return Err(HeapError::Invariant(
                                "synchronous module SCC finish retained async work",
                            ));
                        }
                        record.evaluation = RawModuleEvaluationState::Evaluated;
                    }
                    record.evaluation_cycle_root = Some(cycle_root);
                }
                _ => {
                    return Err(HeapError::Invariant(
                        "loaded-module evaluation did not finish from linked Evaluating state",
                    ));
                }
            },
            RawModuleTransition::BeginAsyncEvaluation { order } => match record.evaluation {
                RawModuleEvaluationState::Evaluating
                    if matches!(record.link_status, RawModuleLinkStatus::Linked)
                        && record.evaluation_cycle_root.is_none()
                        && record.async_evaluation_order.is_none()
                        && (record.has_top_level_await
                            || record.pending_async_dependencies != 0) =>
                {
                    record.async_evaluation_order = Some(order);
                }
                _ => {
                    return Err(HeapError::Invariant(
                        "loaded-module async evaluation did not begin from active async work",
                    ));
                }
            },
            RawModuleTransition::FinishAsyncEvaluation => match record.evaluation {
                RawModuleEvaluationState::EvaluatingAsync
                    if matches!(record.link_status, RawModuleLinkStatus::Linked)
                        && record.evaluation_cycle_root.is_some()
                        && record.async_evaluation_order.is_some()
                        && record.pending_async_dependencies == 0 =>
                {
                    record.evaluation = RawModuleEvaluationState::Evaluated;
                    record.async_evaluation_order = None;
                }
                _ => {
                    return Err(HeapError::Invariant(
                        "loaded-module async evaluation finished from an invalid state",
                    ));
                }
            },
            RawModuleTransition::PoisonEvaluation => match record.evaluation {
                RawModuleEvaluationState::Evaluating
                | RawModuleEvaluationState::EvaluatingAsync => {
                    record.evaluation = RawModuleEvaluationState::Poisoned;
                    if record.evaluation_promise.is_some() && record.evaluation_cycle_root.is_none()
                    {
                        record.evaluation_cycle_root = Some(module.module);
                    }
                    record.async_evaluation_order = None;
                    record.pending_async_dependencies = 0;
                }
                _ => {
                    return Err(HeapError::Invariant(
                        "loaded-module evaluation poison did not target Evaluating",
                    ));
                }
            },
            RawModuleTransition::FinishNamespace(namespace) => match record.namespace {
                RawModuleNamespaceState::Building(current) if current == namespace => {
                    record.namespace = RawModuleNamespaceState::Ready(namespace);
                }
                _ => {
                    return Err(HeapError::Invariant(
                        "loaded-module namespace did not finish from matching Building state",
                    ));
                }
            },
        }
        Ok(())
    }

    /// Append one parser-discovered request without cloning the complete
    /// source-order prefix on every publication. Module requests own no arena
    /// edges or atoms, so the only fallible step is reserving vector capacity
    /// before the new request becomes visible.
    pub(crate) fn append_parsing_module_request(
        &mut self,
        module: RawModuleRef,
        request: ModuleRequest,
    ) -> Result<(), HeapError> {
        let record = self.loaded_module_record_mut(module)?;
        if !matches!(&record.body, RawModuleRecordBody::Parsing) {
            return Err(HeapError::Invariant(
                "module request publication did not target Parsing state",
            ));
        }
        if let Some(requests) = Rc::get_mut(&mut record.requested_modules) {
            requests.try_reserve(1).map_err(|_| HeapError::Allocation {
                operation: "growing a parse-in-progress module request prefix",
            })?;
            requests.push(request);
            return Ok(());
        }

        let mut requests = Vec::new();
        requests
            .try_reserve(record.requested_modules.len() + 1)
            .map_err(|_| HeapError::Allocation {
                operation: "copying a shared parse-in-progress module request prefix",
            })?;
        requests.extend(record.requested_modules.iter().cloned());
        requests.push(request);
        record.requested_modules = Rc::new(requests);
        Ok(())
    }

    fn loaded_module_record_mut(
        &mut self,
        module: RawModuleRef,
    ) -> Result<&mut RawModuleRecord, HeapError> {
        let NodeData::Context(context) =
            &mut self.live_node_mut(RawId::Context(module.cache))?.data
        else {
            return Err(HeapError::Invariant(
                "loaded-module mutation reached another node payload",
            ));
        };
        context
            .loaded_modules
            .records
            .get_mut(module.module.0)
            .and_then(Option::as_mut)
            .ok_or(HeapError::Invariant(
                "loaded-module mutation reached a missing cache record",
            ))
    }

    fn validate_loaded_module_record(
        &self,
        cache: ContextId,
        module: Option<ModuleId>,
        record: &RawModuleRecord,
    ) -> Result<(), HeapError> {
        self.context(cache)?;
        if record.compile_realm != cache {
            return Err(HeapError::Invariant(
                "loaded-module compilation realm disagrees with its Context cache",
            ));
        }
        match &record.body {
            RawModuleRecordBody::Parsing => {
                if !module_record_has_pristine_construction_metadata(record) {
                    return Err(HeapError::Invariant(
                        "parse-in-progress module retains completed metadata",
                    ));
                }
                if let RawModuleResolutionState::Resolved(dependencies) = &record.resolution {
                    if dependencies.len() > record.requested_modules.len() {
                        return Err(HeapError::Invariant(
                            "parse-in-progress module resolved beyond its request prefix",
                        ));
                    }
                    let context = self.context(cache)?;
                    for dependency in dependencies.iter().copied() {
                        if context
                            .loaded_modules
                            .records
                            .get(dependency.0)
                            .and_then(Option::as_ref)
                            .is_none()
                        {
                            return Err(HeapError::Invariant(
                                "parse-in-progress module dependency references a missing identity",
                            ));
                        }
                    }
                }
                return Ok(());
            }
            RawModuleRecordBody::Aborted => {
                if module.is_none()
                    || !record.requested_modules.is_empty()
                    || !matches!(record.resolution, RawModuleResolutionState::Unresolved)
                    || !module_record_has_pristine_construction_metadata(record)
                {
                    return Err(HeapError::Invariant(
                        "aborted module retained state beyond its name and realm",
                    ));
                }
                return Ok(());
            }
            RawModuleRecordBody::SourceText { .. } | RawModuleRecordBody::Json { .. } => {}
        }
        if record.instance.is_some() != record.link_realm.is_some() {
            return Err(HeapError::Invariant(
                "loaded-module instance and link realm ownership disagree",
            ));
        }
        if let Some(link_realm) = record.link_realm {
            match link_realm {
                RawModuleLinkRealm::Cache => {}
                RawModuleLinkRealm::Other(realm) => {
                    if realm == cache {
                        return Err(HeapError::Invariant(
                            "loaded-module cache realm escaped through an Other link edge",
                        ));
                    }
                    self.context(realm)?;
                }
            }
        }
        let source_function = match &record.body {
            RawModuleRecordBody::SourceText { function } => {
                if self.function_bytecode(*function)?.realm != cache {
                    return Err(HeapError::Invariant(
                        "loaded-module source bytecode belongs to another realm",
                    ));
                }
                Some(*function)
            }
            RawModuleRecordBody::Json { default_value } => {
                if record.has_top_level_await {
                    return Err(HeapError::Invariant(
                        "JSON module claims authored top-level await",
                    ));
                }
                validate_module_storable_value(default_value)?;
                None
            }
            RawModuleRecordBody::Parsing | RawModuleRecordBody::Aborted => {
                return Err(HeapError::Invariant(
                    "module construction state escaped ready-record validation",
                ));
            }
        };
        if let Some(import_meta) = record.import_meta
            && self.object(import_meta)?.kind != ObjectKind::Ordinary
        {
            return Err(HeapError::Invariant(
                "loaded-module import.meta has the wrong object class",
            ));
        }
        if let RawModuleEvaluationState::Errored(exception) = &record.evaluation {
            validate_module_storable_value(exception)?;
        }
        match &record.evaluation {
            RawModuleEvaluationState::Unevaluated => {
                if record.evaluation_cycle_root.is_some()
                    || record.async_evaluation_order.is_some()
                    || record.pending_async_dependencies != 0
                    || !record.async_parent_modules.is_empty()
                {
                    return Err(HeapError::Invariant(
                        "unevaluated loaded module retains async evaluation state",
                    ));
                }
            }
            RawModuleEvaluationState::Evaluating => {
                if record.evaluation_cycle_root.is_some() {
                    return Err(HeapError::Invariant(
                        "active module received a cycle root before SCC publication",
                    ));
                }
            }
            RawModuleEvaluationState::EvaluatingAsync => {
                if record.evaluation_cycle_root.is_none() || record.async_evaluation_order.is_none()
                {
                    return Err(HeapError::Invariant(
                        "async-evaluating module has incomplete SCC metadata",
                    ));
                }
            }
            RawModuleEvaluationState::Evaluated | RawModuleEvaluationState::Errored(_) => {
                if record.evaluation_cycle_root.is_none()
                    || record.async_evaluation_order.is_some()
                    || record.pending_async_dependencies != 0
                {
                    return Err(HeapError::Invariant(
                        "completed loaded-module evaluation retains active async metadata",
                    ));
                }
            }
            RawModuleEvaluationState::Poisoned => {
                if record.async_evaluation_order.is_some() || record.pending_async_dependencies != 0
                {
                    return Err(HeapError::Invariant(
                        "poisoned loaded module retains active async metadata",
                    ));
                }
            }
        }
        if let Some(cycle_root) = record.evaluation_cycle_root {
            if self
                .context(cache)?
                .loaded_modules
                .records
                .get(cycle_root.0)
                .and_then(Option::as_ref)
                .is_none()
            {
                return Err(HeapError::Invariant(
                    "loaded-module evaluation cycle root is missing",
                ));
            }
        }
        match (
            record.evaluation_promise,
            record.evaluation_resolve,
            record.evaluation_reject,
        ) {
            (None, None, None) => {}
            (Some(promise), Some(resolve), Some(reject)) => {
                self.validate_module_evaluation_capability(promise, resolve, reject)?;
                let Some(module) = module else {
                    return Err(HeapError::Invariant(
                        "new loaded module already retains an evaluation capability",
                    ));
                };
                if record
                    .evaluation_cycle_root
                    .is_some_and(|cycle_root| cycle_root != module)
                {
                    return Err(HeapError::Invariant(
                        "non-cycle-root module retains an evaluation capability",
                    ));
                }
            }
            _ => {
                return Err(HeapError::Invariant(
                    "loaded-module evaluation capability is partially published",
                ));
            }
        }
        let context = self.context(cache)?;
        for parent in record.async_parent_modules.iter().copied() {
            if context
                .loaded_modules
                .records
                .get(parent.0)
                .and_then(Option::as_ref)
                .is_none()
            {
                return Err(HeapError::Invariant(
                    "loaded-module async parent references a missing cache record",
                ));
            }
        }
        if let Some(instance) = &record.instance {
            for slot in instance.slots.iter().flatten().copied() {
                self.var_ref(slot)?;
            }
            match source_function {
                Some(function) => {
                    if instance.slots.len()
                        != self.function_bytecode(function)?.closure_variables.len()
                    {
                        return Err(HeapError::Invariant(
                            "loaded-module source instance has the wrong closure slot count",
                        ));
                    }
                    if let Some(callable) = instance.callable {
                        let ObjectPayload::BytecodeFunction {
                            bytecode,
                            closure_slots,
                            ..
                        } = &self.object(callable)?.payload
                        else {
                            return Err(HeapError::Invariant(
                                "loaded-module source callable is not a bytecode function",
                            ));
                        };
                        if *bytecode != function
                            || instance.slots.iter().copied().collect::<Option<Vec<_>>>()
                                != Some(closure_slots.clone())
                        {
                            return Err(HeapError::Invariant(
                                "loaded-module callable does not match its source instance",
                            ));
                        }
                    }
                }
                None => {
                    if instance.slots.len() != 1 || instance.callable.is_some() {
                        return Err(HeapError::Invariant(
                            "loaded-module JSON instance has an invalid environment",
                        ));
                    }
                }
            }
        }
        match record.namespace {
            RawModuleNamespaceState::Empty => {}
            RawModuleNamespaceState::Building(namespace)
            | RawModuleNamespaceState::Ready(namespace) => {
                if self.object(namespace)?.kind != ObjectKind::ModuleNamespace {
                    return Err(HeapError::Invariant(
                        "loaded-module namespace has the wrong object class",
                    ));
                }
                if record.instance.is_none() {
                    return Err(HeapError::Invariant(
                        "loaded-module namespace exists without an instance",
                    ));
                }
            }
        }
        if (record.instance.is_some()
            || !matches!(record.namespace, RawModuleNamespaceState::Empty)
            || !matches!(record.link_status, RawModuleLinkStatus::Unlinked))
            && !matches!(record.resolution, RawModuleResolutionState::Resolved(_))
        {
            return Err(HeapError::Invariant(
                "loaded-module active state is not resolved",
            ));
        }
        if matches!(record.link_status, RawModuleLinkStatus::Linked) {
            let Some(instance) = &record.instance else {
                return Err(HeapError::Invariant(
                    "linked loaded-module has no instantiated environment",
                ));
            };
            if instance.slots.iter().any(Option::is_none)
                || source_function.is_some() && instance.callable.is_none()
            {
                return Err(HeapError::Invariant(
                    "linked loaded-module has an incomplete instance",
                ));
            }
        }
        if !matches!(record.evaluation, RawModuleEvaluationState::Unevaluated)
            && !matches!(record.link_status, RawModuleLinkStatus::Linked)
        {
            return Err(HeapError::Invariant(
                "active loaded-module evaluation is not linked",
            ));
        }
        if record.evaluation_promise.is_some()
            && !matches!(record.link_status, RawModuleLinkStatus::Linked)
        {
            return Err(HeapError::Invariant(
                "loaded-module evaluation Promise exists before linking",
            ));
        }
        if let RawModuleResolutionState::Resolved(dependencies) = &record.resolution {
            let context = self.context(cache)?;
            for dependency in dependencies.iter().copied() {
                if context
                    .loaded_modules
                    .records
                    .get(dependency.0)
                    .and_then(Option::as_ref)
                    .is_none()
                {
                    return Err(HeapError::Invariant(
                        "loaded-module dependency references a missing cache record",
                    ));
                }
            }
        }
        Ok(())
    }

    /// Publish a module at the tail of this Context's construction-ordered
    /// cache. All arena edges are retained before the record becomes visible.
    pub(crate) fn publish_loaded_module(
        &mut self,
        cache: ContextId,
        record: RawModuleRecord,
    ) -> Result<RawModuleRef, HeapError> {
        self.validate_loaded_module_record(cache, None, &record)?;
        let cache_index = self.live_index(RawId::Context(cache))?;
        {
            let NodeData::Context(context) = &mut self.live_node_mut(RawId::Context(cache))?.data
            else {
                return Err(HeapError::Invariant(
                    "loaded-module publication reached another node payload",
                ));
            };
            context
                .loaded_modules
                .records
                .try_reserve(1)
                .map_err(|_| HeapError::Allocation {
                    operation: "growing a loaded-module cache",
                })?;
            if !context
                .loaded_modules
                .first_by_name
                .contains_key(&record.name)
            {
                context
                    .loaded_modules
                    .first_by_name
                    .try_reserve(1)
                    .map_err(|_| HeapError::Allocation {
                        operation: "growing a loaded-module name index",
                    })?;
            }
        }
        let edges = raw_module_record_edges(&record);
        let name = record.name.clone();
        self.retain_edges_transactionally(&edges)?;

        let SlotState::Live(node) = &mut self.slots[cache_index].state else {
            unreachable!("authenticated loaded-module cache disappeared before publication")
        };
        let NodeData::Context(context) = &mut node.data else {
            unreachable!("authenticated loaded-module cache changed node kind before publication")
        };
        let module = ModuleId(context.loaded_modules.records.len());
        context.loaded_modules.records.push(Some(record));
        context
            .loaded_modules
            .first_by_name
            .entry(name)
            .or_insert(module);
        Ok(RawModuleRef { cache, module })
    }

    /// Atomically replace one module record. Replacement edges are retained
    /// before publication; detached edges are released only after the swap.
    pub(crate) fn replace_loaded_module(
        &mut self,
        module: RawModuleRef,
        replacement: RawModuleRecord,
    ) -> Result<HeapCleanup, HeapError> {
        let current = self.loaded_module(module)?;
        self.validate_loaded_module_record(module.cache, Some(module.module), &replacement)?;
        if replacement.name != current.name {
            return Err(HeapError::Invariant(
                "loaded-module replacement changed its cache name",
            ));
        }
        if replacement.compile_realm != module.cache
            || replacement.compile_realm != current.compile_realm
        {
            return Err(HeapError::Invariant(
                "loaded-module replacement changed its compilation cache",
            ));
        }
        validate_module_body_replacement(&current, &replacement)?;
        let cache_index = self.live_index(RawId::Context(module.cache))?;

        let old_edges = raw_module_record_edges(&current);
        let new_edges = raw_module_record_edges(&replacement);
        let added_edges = multiset_difference(
            &new_edges,
            &old_edges,
            "computing added loaded-module edges",
        )?;
        let removed_edges = multiset_difference(
            &old_edges,
            &new_edges,
            "computing removed loaded-module edges",
        )?;
        let old_atoms = raw_module_record_atoms(&current).collect::<Vec<_>>();
        let new_atoms = raw_module_record_atoms(&replacement).collect::<Vec<_>>();
        let removed_atoms = multiset_difference(
            &old_atoms,
            &new_atoms,
            "computing removed loaded-module atoms",
        )?;
        self.preflight_module_edge_releases(&removed_edges)?;
        let mut cleanup = HeapCleanup {
            atoms: removed_atoms,
            ..HeapCleanup::default()
        };
        self.retain_edges_transactionally(&added_edges)?;

        let SlotState::Live(node) = &mut self.slots[cache_index].state else {
            unreachable!("authenticated loaded-module cache disappeared before replacement")
        };
        let NodeData::Context(context) = &mut node.data else {
            unreachable!("authenticated loaded-module cache changed node kind before replacement")
        };
        let slot = context
            .loaded_modules
            .records
            .get_mut(module.module.0)
            .and_then(Option::as_mut)
            .expect("authenticated loaded-module record disappeared before replacement");
        let previous = std::mem::replace(slot, replacement);
        drop(previous);
        cleanup.merge(self.release_preflighted_module_edges(&removed_edges));
        Ok(cleanup)
    }

    /// Atomically abort one parse-in-progress module definition.
    ///
    /// An unreferenced identity becomes an ordinary tombstone. If another live
    /// record already refers to it, the slot instead retains an edge-free
    /// `Aborted` sentinel so those append-only references remain structurally
    /// valid while every public module operation observes a non-live handle.
    /// In both cases the record is removed from oldest-name lookup.
    pub(crate) fn abort_parsing_loaded_module(
        &mut self,
        module: RawModuleRef,
    ) -> Result<HeapCleanup, HeapError> {
        let current = self.loaded_module(module)?;
        if !matches!(&current.body, RawModuleRecordBody::Parsing) {
            return Err(HeapError::Invariant(
                "module construction abort did not target Parsing state",
            ));
        }
        let aborted = aborted_module_record(&current);
        self.validate_loaded_module_record(module.cache, Some(module.module), &aborted)?;

        let context = self.context(module.cache)?;
        context.loaded_modules.validate_first_by_name()?;
        let referenced =
            context
                .loaded_modules
                .records
                .iter()
                .enumerate()
                .any(|(index, record)| {
                    index != module.module.0
                        && record.as_ref().is_some_and(|record| {
                            !matches!(&record.body, RawModuleRecordBody::Aborted)
                                && module_record_references_identity(record, module.module)
                        })
                });
        let indexed_first = context
            .loaded_modules
            .first_by_name
            .get(&current.name)
            .copied();
        let fallback = (indexed_first == Some(module.module)).then(|| {
            context
                .loaded_modules
                .records
                .iter()
                .enumerate()
                .skip(module.module.0 + 1)
                .find_map(|(index, record)| {
                    record.as_ref().and_then(|record| {
                        (record.name == current.name
                            && !matches!(&record.body, RawModuleRecordBody::Aborted))
                        .then_some(ModuleId(index))
                    })
                })
        });

        let removed_edges = raw_module_record_edges(&current);
        let removed_atoms = raw_module_record_atoms(&current).collect::<Vec<_>>();
        debug_assert!(removed_atoms.is_empty());
        self.preflight_module_edge_releases(&removed_edges)?;
        let cache_index = self.live_index(RawId::Context(module.cache))?;
        let mut cleanup = HeapCleanup {
            atoms: removed_atoms,
            ..HeapCleanup::default()
        };

        let SlotState::Live(node) = &mut self.slots[cache_index].state else {
            unreachable!("authenticated loaded-module cache disappeared before abort")
        };
        let NodeData::Context(context) = &mut node.data else {
            unreachable!("authenticated loaded-module cache changed kind before abort")
        };
        let slot = context
            .loaded_modules
            .records
            .get_mut(module.module.0)
            .expect("authenticated parse-in-progress module disappeared before abort");
        let previous = if referenced {
            Some(std::mem::replace(
                slot.as_mut()
                    .expect("authenticated parse-in-progress module became a tombstone"),
                aborted,
            ))
        } else {
            slot.take()
        };
        drop(previous);
        if indexed_first == Some(module.module) {
            match fallback.flatten() {
                Some(fallback) => {
                    *context
                        .loaded_modules
                        .first_by_name
                        .get_mut(&current.name)
                        .expect("authenticated module name index disappeared before abort") =
                        fallback;
                }
                None => {
                    let removed = context.loaded_modules.first_by_name.remove(&current.name);
                    debug_assert_eq!(removed, Some(module.module));
                }
            }
        }

        cleanup.merge(self.release_preflighted_module_edges(&removed_edges));
        Ok(cleanup)
    }

    /// Atomically tombstone a validated set of records in one Context cache.
    /// The caller supplies the already-computed rollback set; its membership
    /// and ordering policy remain outside this ownership primitive.
    pub(crate) fn unpublish_loaded_modules(
        &mut self,
        cache: ContextId,
        modules: &[ModuleId],
    ) -> Result<HeapCleanup, HeapError> {
        let mut unique = HashSet::new();
        unique
            .try_reserve(modules.len())
            .map_err(|_| HeapError::Allocation {
                operation: "validating loaded-module removal batch",
            })?;
        let mut records = Vec::new();
        records
            .try_reserve(modules.len())
            .map_err(|_| HeapError::Allocation {
                operation: "preparing loaded-module removal batch",
            })?;
        for &module in modules {
            if !unique.insert(module) {
                return Err(HeapError::Invariant(
                    "loaded-module removal batch contains a duplicate identity",
                ));
            }
            let record = self.loaded_module(RawModuleRef { cache, module })?;
            if matches!(&record.body, RawModuleRecordBody::Aborted) {
                return Err(HeapError::Invariant(
                    "loaded-module removal batch contains an aborted identity",
                ));
            }
            records.push((module, record));
        }
        let context = self.context(cache)?;
        context.loaded_modules.validate_first_by_name()?;

        for (index, record) in context.loaded_modules.records.iter().enumerate() {
            if unique.contains(&ModuleId(index)) {
                continue;
            }
            if let Some(record) = record
                && !matches!(&record.body, RawModuleRecordBody::Aborted)
                && let RawModuleResolutionState::Resolved(dependencies) = &record.resolution
                && dependencies
                    .iter()
                    .any(|dependency| unique.contains(dependency))
            {
                return Err(HeapError::Invariant(
                    "loaded-module removal leaves a live dependency on a tombstone",
                ));
            }
            if let Some(record) = record
                && !matches!(&record.body, RawModuleRecordBody::Aborted)
                && record
                    .async_parent_modules
                    .iter()
                    .any(|parent| unique.contains(parent))
            {
                return Err(HeapError::Invariant(
                    "loaded-module removal leaves a live async-parent edge on a tombstone",
                ));
            }
            if let Some(record) = record
                && !matches!(&record.body, RawModuleRecordBody::Aborted)
                && record
                    .evaluation_cycle_root
                    .is_some_and(|cycle_root| unique.contains(&cycle_root))
            {
                return Err(HeapError::Invariant(
                    "loaded-module removal leaves a live evaluation-root edge on a tombstone",
                ));
            }
        }

        #[allow(clippy::mutable_key_type)]
        let rebuilt_first_by_name = context
            .loaded_modules
            .rebuild_first_by_name_excluding(|candidate| unique.contains(&candidate))?;

        let mut removed_edges = Vec::new();
        let mut removed_atoms = Vec::new();
        for (_, record) in &records {
            removed_edges.extend(raw_module_record_edges(record));
            removed_atoms.extend(raw_module_record_atoms(record));
        }
        self.preflight_module_edge_releases(&removed_edges)?;
        let cache_index = self.live_index(RawId::Context(cache))?;
        let mut cleanup = HeapCleanup {
            atoms: removed_atoms,
            ..HeapCleanup::default()
        };

        let SlotState::Live(node) = &mut self.slots[cache_index].state else {
            unreachable!("authenticated loaded-module cache disappeared before batch removal")
        };
        let NodeData::Context(context) = &mut node.data else {
            unreachable!("authenticated loaded-module cache changed kind before batch removal")
        };
        for &(module, _) in &records {
            context.loaded_modules.records[module.0]
                .take()
                .expect("authenticated loaded-module disappeared before batch removal");
        }
        context.loaded_modules.first_by_name = rebuilt_first_by_name;

        cleanup.merge(self.release_preflighted_module_edges(&removed_edges));
        Ok(cleanup)
    }

    /// Atomically clear namespace objects created by one namespace-building
    /// transaction. Both Building and Ready are accepted because a recursive
    /// member may finish before a later member makes the transaction fail.
    pub(crate) fn rollback_loaded_module_namespaces(
        &mut self,
        modules: &[RawModuleRef],
    ) -> Result<HeapCleanup, HeapError> {
        if modules.is_empty() {
            return Ok(HeapCleanup::default());
        }
        let cache = modules[0].cache;
        let mut unique = HashSet::new();
        unique
            .try_reserve(modules.len())
            .map_err(|_| HeapError::Allocation {
                operation: "validating module namespace rollback batch",
            })?;
        let mut namespaces = Vec::new();
        namespaces
            .try_reserve(modules.len())
            .map_err(|_| HeapError::Allocation {
                operation: "preparing module namespace rollback batch",
            })?;
        for &module in modules {
            if module.cache != cache || !unique.insert(module.module) {
                return Err(HeapError::Invariant(
                    "module namespace rollback batch has mixed or duplicate identities",
                ));
            }
            let namespace = match self.loaded_module(module)?.namespace {
                RawModuleNamespaceState::Building(namespace)
                | RawModuleNamespaceState::Ready(namespace) => namespace,
                RawModuleNamespaceState::Empty => {
                    return Err(HeapError::Invariant(
                        "module namespace rollback reached an empty record",
                    ));
                }
            };
            namespaces.push((module, namespace));
        }
        let removed_edges = namespaces
            .iter()
            .map(|(_, namespace)| RawId::Object(*namespace))
            .collect::<Vec<_>>();
        self.preflight_module_edge_releases(&removed_edges)?;
        for &(module, namespace) in &namespaces {
            let record = self
                .loaded_module_record_mut(module)
                .expect("authenticated namespace rollback record disappeared before commit");
            debug_assert!(matches!(
                record.namespace,
                RawModuleNamespaceState::Building(current)
                    | RawModuleNamespaceState::Ready(current) if current == namespace
            ));
            record.namespace = RawModuleNamespaceState::Empty;
        }
        Ok(self.release_preflighted_module_edges(&removed_edges))
    }

    /// Atomically install the cached evaluation Promise and its intrinsic
    /// resolving pair on a cycle root. All three object edges become owned by
    /// the Context record together, so an evaluator never needs to publish a
    /// partially rooted capability through a general record mutation.
    pub(crate) fn publish_loaded_module_evaluation_capability(
        &mut self,
        module: RawModuleRef,
        promise: ObjectId,
        resolve: ObjectId,
        reject: ObjectId,
    ) -> Result<(), HeapError> {
        self.validate_module_evaluation_capability(promise, resolve, reject)?;
        let record = self.loaded_module(module)?;
        if record.evaluation_promise.is_some()
            || record.evaluation_resolve.is_some()
            || record.evaluation_reject.is_some()
            || record.link_status != RawModuleLinkStatus::Linked
            || matches!(
                record.evaluation,
                RawModuleEvaluationState::Evaluating | RawModuleEvaluationState::Poisoned
            )
            || record
                .evaluation_cycle_root
                .is_some_and(|cycle_root| cycle_root != module.module)
        {
            return Err(HeapError::Invariant(
                "loaded-module evaluation capability has an invalid publication target",
            ));
        }
        let edges = [
            RawId::Object(promise),
            RawId::Object(resolve),
            RawId::Object(reject),
        ];
        self.retain_edges_transactionally(&edges)?;
        let record = self
            .loaded_module_record_mut(module)
            .expect("authenticated evaluation capability target disappeared before commit");
        record.evaluation_promise = Some(promise);
        record.evaluation_resolve = Some(resolve);
        record.evaluation_reject = Some(reject);
        Ok(())
    }

    /// Append one reverse async-dependency edge and increment its parent's
    /// pending count as one allocation-safe metadata transaction. Duplicate
    /// module identities are deliberately retained and counted separately.
    pub(crate) fn add_loaded_module_async_dependency(
        &mut self,
        dependency: RawModuleRef,
        parent: RawModuleRef,
    ) -> Result<u32, HeapError> {
        if dependency.cache != parent.cache || dependency.module == parent.module {
            return Err(HeapError::Invariant(
                "async module dependency has mixed caches or a self parent",
            ));
        }
        let dependency_record = self.loaded_module(dependency)?;
        let parent_record = self.loaded_module(parent)?;
        if dependency_record.async_evaluation_order.is_none()
            || !matches!(
                dependency_record.evaluation,
                RawModuleEvaluationState::Evaluating | RawModuleEvaluationState::EvaluatingAsync
            )
            || !matches!(
                parent_record.evaluation,
                RawModuleEvaluationState::Evaluating
            )
            || parent_record.evaluation_cycle_root.is_some()
        {
            return Err(HeapError::Invariant(
                "async module dependency edge has invalid evaluation states",
            ));
        }
        let pending = parent_record
            .pending_async_dependencies
            .checked_add(1)
            .ok_or(HeapError::Overflow {
                operation: "counting pending async module dependencies",
            })?;
        self.loaded_module_record_mut(dependency)?
            .async_parent_modules
            .try_reserve(1)
            .map_err(|_| HeapError::Allocation {
                operation: "growing async module parent edges",
            })?;
        self.loaded_module_record_mut(parent)?
            .pending_async_dependencies = pending;
        self.loaded_module_record_mut(dependency)?
            .async_parent_modules
            .push(parent.module);
        Ok(pending)
    }

    /// Consume one reverse dependency edge's pending-count contribution.
    /// The caller uses the returned zero to decide when the parent is ready.
    pub(crate) fn complete_loaded_module_async_dependency(
        &mut self,
        parent: RawModuleRef,
    ) -> Result<u32, HeapError> {
        let record = self.loaded_module_record_mut(parent)?;
        if !matches!(record.evaluation, RawModuleEvaluationState::EvaluatingAsync)
            || record.evaluation_cycle_root.is_none()
            || record.async_evaluation_order.is_none()
        {
            return Err(HeapError::Invariant(
                "async module dependency completed for an inactive parent",
            ));
        }
        record.pending_async_dependencies = record
            .pending_async_dependencies
            .checked_sub(1)
            .ok_or(HeapError::Invariant(
                "async module dependency count underflow",
            ))?;
        Ok(record.pending_async_dependencies)
    }

    fn validate_module_evaluation_capability(
        &self,
        promise: ObjectId,
        resolve: ObjectId,
        reject: ObjectId,
    ) -> Result<(), HeapError> {
        if !matches!(self.object(promise)?.payload, ObjectPayload::Promise(_)) {
            return Err(HeapError::Invariant(
                "module evaluation capability has a non-Promise target",
            ));
        }
        let ObjectPayload::NativeFunction {
            data: resolve_data,
            internal:
                Some(InternalCallableData::PromiseResolving {
                    promise: resolve_promise,
                    already_resolved: resolve_cell,
                    kind: PromiseResolvingKind::Resolve,
                }),
        } = &self.object(resolve)?.payload
        else {
            return Err(HeapError::Invariant(
                "module evaluation capability has an invalid resolve function",
            ));
        };
        let ObjectPayload::NativeFunction {
            data: reject_data,
            internal:
                Some(InternalCallableData::PromiseResolving {
                    promise: reject_promise,
                    already_resolved: reject_cell,
                    kind: PromiseResolvingKind::Reject,
                }),
        } = &self.object(reject)?.payload
        else {
            return Err(HeapError::Invariant(
                "module evaluation capability has an invalid reject function",
            ));
        };
        if resolve_data.target != NativeFunctionId::PromiseResolving(PromiseResolvingKind::Resolve)
            || reject_data.target
                != NativeFunctionId::PromiseResolving(PromiseResolvingKind::Reject)
            || *resolve_promise != promise
            || *reject_promise != promise
            || !Rc::ptr_eq(resolve_cell, reject_cell)
        {
            return Err(HeapError::Invariant(
                "module evaluation capability resolving pair targets another Promise",
            ));
        }
        Ok(())
    }

    /// Atomically publish the same cached abrupt completion across one active
    /// evaluation SCC. Object edges are retained transactionally before any
    /// record changes; Symbol atom ownership is prepared by Runtime because it
    /// belongs to the separate atom table.
    pub(crate) fn publish_loaded_module_errors(
        &mut self,
        cache: ContextId,
        modules: &[ModuleId],
        cycle_root: ModuleId,
        exception: RawValue,
    ) -> Result<(), HeapError> {
        validate_module_storable_value(&exception)?;
        let mut unique = HashSet::new();
        unique
            .try_reserve(modules.len())
            .map_err(|_| HeapError::Allocation {
                operation: "validating module evaluation error batch",
            })?;
        for &module in modules {
            if !unique.insert(module) {
                return Err(HeapError::Invariant(
                    "module evaluation error batch contains a duplicate identity",
                ));
            }
            let record = self.loaded_module(RawModuleRef { cache, module })?;
            if !matches!(record.evaluation, RawModuleEvaluationState::Evaluating)
                || !matches!(record.link_status, RawModuleLinkStatus::Linked)
            {
                return Err(HeapError::Invariant(
                    "module evaluation error batch reached a non-evaluating linked record",
                ));
            }
        }
        if !unique.contains(&cycle_root) {
            return Err(HeapError::Invariant(
                "module evaluation error cycle root was not active",
            ));
        }
        let edge = raw_value_edges(&exception);
        let mut added_edges = Vec::new();
        added_edges
            .try_reserve(edge.len().saturating_mul(modules.len()))
            .map_err(|_| HeapError::Allocation {
                operation: "preparing module evaluation error edges",
            })?;
        for _ in modules {
            added_edges.extend(edge.iter().copied());
        }
        self.retain_edges_transactionally(&added_edges)?;
        for &module in modules {
            let record = self
                .loaded_module_record_mut(RawModuleRef { cache, module })
                .expect("authenticated evaluation error record disappeared before commit");
            record.evaluation = RawModuleEvaluationState::Errored(exception.clone());
            record.evaluation_cycle_root = Some(cycle_root);
            record.async_evaluation_order = None;
            record.pending_async_dependencies = 0;
        }
        Ok(())
    }

    /// Cache one rejection reason on an already-published async module. The
    /// record preserves its SCC cycle root and reverse parent list so Runtime
    /// can reproduce QuickJS's observable per-node reject-then-recurse order.
    ///
    /// As with `publish_loaded_module_errors`, Symbol atom ownership is
    /// prepared by Runtime; this operation retains all arena edges.
    pub(crate) fn publish_loaded_module_async_error(
        &mut self,
        module: RawModuleRef,
        exception: RawValue,
    ) -> Result<(), HeapError> {
        validate_module_storable_value(&exception)?;
        let record = self.loaded_module(module)?;
        if !matches!(record.evaluation, RawModuleEvaluationState::EvaluatingAsync)
            || record.evaluation_cycle_root.is_none()
            || record.async_evaluation_order.is_none()
            || record.link_status != RawModuleLinkStatus::Linked
        {
            return Err(HeapError::Invariant(
                "async module rejection reached a non-evaluating record",
            ));
        }
        let value_edges = raw_value_edges(&exception);
        self.retain_edges_transactionally(&value_edges)?;
        let record = self
            .loaded_module_record_mut(module)
            .expect("authenticated async module rejection target disappeared before commit");
        record.evaluation = RawModuleEvaluationState::Errored(exception);
        record.async_evaluation_order = None;
        record.pending_async_dependencies = 0;
        Ok(())
    }

    fn preflight_module_edge_releases(&mut self, edges: &[RawId]) -> Result<(), HeapError> {
        let mut counts = HashMap::<RawId, u32>::new();
        counts
            .try_reserve(edges.len())
            .map_err(|_| HeapError::Allocation {
                operation: "preflighting loaded-module edge releases",
            })?;
        for &edge in edges {
            let count = counts.entry(edge).or_default();
            *count = count.checked_add(1).ok_or(HeapError::Overflow {
                operation: "counting removed loaded-module edges",
            })?;
        }
        let mut newly_zero = 0usize;
        for (&edge, &removed) in &counts {
            let strong = self.live_node(edge)?.strong;
            let remaining = strong.checked_sub(removed).ok_or(HeapError::Underflow {
                kind: edge.kind(),
                index: edge.index(),
                generation: edge.generation(),
            })?;
            newly_zero = newly_zero.saturating_add(usize::from(remaining == 0));
        }
        self.zero_queue
            .try_reserve(newly_zero)
            .map_err(|_| HeapError::Allocation {
                operation: "reserving loaded-module release queue space",
            })?;
        Ok(())
    }

    fn release_preflighted_module_edges(&mut self, edges: &[RawId]) -> HeapCleanup {
        for &edge in edges {
            self.release_raw_no_drain(edge)
                .expect("preflighted loaded-module edge release failed after publication");
        }
        self.drain_zero_queue()
            .expect("preflighted loaded-module edge drain failed after publication")
    }

    /// Seed the realm-local xorshift64* stream used by `Math.random`.
    /// QuickJS replaces an all-zero time seed with one because zero is the
    /// generator's absorbing state.
    pub(crate) fn initialize_math_random_state(
        &mut self,
        id: ContextId,
        seed: u64,
    ) -> Result<(), HeapError> {
        let NodeData::Context(context) = &mut self.live_node_mut(RawId::Context(id))?.data else {
            return Err(HeapError::Invariant(
                "typed context lookup reached another node payload",
            ));
        };
        if context.math_random_state != 0 {
            return Err(HeapError::Invariant(
                "Math.random state was initialized more than once",
            ));
        }
        context.math_random_state = if seed == 0 { 1 } else { seed };
        Ok(())
    }

    /// Advance the pinned QuickJS xorshift64* stream for one realm.
    pub(crate) fn next_math_random_u64(&mut self, id: ContextId) -> Result<u64, HeapError> {
        let NodeData::Context(context) = &mut self.live_node_mut(RawId::Context(id))?.data else {
            return Err(HeapError::Invariant(
                "typed context lookup reached another node payload",
            ));
        };
        if context.math_random_state == 0 {
            return Err(HeapError::Invariant(
                "Math.random state was used before initialization",
            ));
        }
        let mut state = context.math_random_state;
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        context.math_random_state = state;
        Ok(state.wrapping_mul(0x2545_f491_4f6c_dd1d))
    }

    /// Clone one native function's typed hidden capture.  Returned raw values
    /// and identities are borrowed snapshots; callers that keep them across a
    /// heap mutation must first promote or otherwise retain their edges.
    pub(crate) fn native_internal_callable(
        &self,
        id: ObjectId,
    ) -> Result<Option<InternalCallableData>, HeapError> {
        let ObjectPayload::NativeFunction { internal, .. } = &self.object(id)?.payload else {
            return Err(HeapError::Invariant(
                "internal callable lookup reached a non-native function",
            ));
        };
        Ok(internal.clone())
    }

    /// Borrow a complete snapshot of one genuine Proxy's hidden state.
    ///
    /// The target and handler identities in the result are borrowed: callers
    /// that keep them across a heap mutation must promote them to owned roots.
    pub(crate) fn proxy_snapshot(&self, id: ObjectId) -> Result<ProxyData, HeapError> {
        let ObjectPayload::Proxy(data) = &self.object(id)?.payload else {
            return Err(HeapError::Invariant(
                "Proxy snapshot reached an object with the wrong class",
            ));
        };
        Ok(*data)
    }

    /// Consume a `Proxy.revocable` closure's one owned Proxy capture and revoke
    /// that Proxy as one heap mutation.
    ///
    /// A second call is a no-op and returns `false`. On the first call this
    /// clears only the closure edge, matching QuickJS's one-shot
    /// `func_data[0] = JS_NULL`; the Proxy continues to retain both its target
    /// and handler after `is_revoked` is set. Keeping the mutation atomic also
    /// prevents the captured Proxy from being finalized between clearing the
    /// closure and marking its payload revoked.
    pub(crate) fn revoke_proxy_from_callable(
        &mut self,
        callable: ObjectId,
    ) -> Result<(bool, HeapCleanup), HeapError> {
        let proxy = match &self.object(callable)?.payload {
            ObjectPayload::NativeFunction {
                data:
                    NativeFunctionData {
                        target: NativeFunctionId::ProxyRevoke,
                        ..
                    },
                internal: Some(InternalCallableData::ProxyRevoke { proxy }),
            } => *proxy,
            _ => {
                return Err(HeapError::Invariant(
                    "Proxy revocation reached the wrong native function",
                ));
            }
        };
        let Some(proxy) = proxy else {
            return Ok((false, HeapCleanup::default()));
        };
        if !matches!(&self.object(proxy)?.payload, ObjectPayload::Proxy(_)) {
            return Err(HeapError::Invariant(
                "Proxy revocation closure retained an object with the wrong class",
            ));
        }

        let ObjectPayload::Proxy(data) = &mut self.object_mut(proxy)?.payload else {
            unreachable!("Proxy revocation capture was validated before mutation")
        };
        data.is_revoked = true;

        let ObjectPayload::NativeFunction {
            internal: Some(InternalCallableData::ProxyRevoke { proxy: capture }),
            ..
        } = &mut self.object_mut(callable)?.payload
        else {
            unreachable!("Proxy revocation callable was validated before mutation")
        };
        *capture = None;

        self.release_raw_no_drain(RawId::Object(proxy))?;
        Ok((true, self.drain_zero_queue()?))
    }

    /// Store the two arbitrary arguments supplied to a NewPromiseCapability
    /// executor.  Callability is deliberately checked later by the runtime,
    /// after the custom constructor returns, as required by the specification.
    ///
    /// Object edges are retained before publication.  Symbol atoms must be
    /// pre-owned by the caller and transfer to the capture only when this
    /// returns `true`. `false` reports the spec-visible repeated invocation;
    /// the runtime must throw a TypeError and retain caller ownership.
    pub(crate) fn set_promise_capability_capture(
        &mut self,
        id: ObjectId,
        resolve: RawValue,
        reject: RawValue,
    ) -> Result<bool, HeapError> {
        if !is_promise_storable_value(&resolve) || !is_promise_storable_value(&reject) {
            return Err(HeapError::Invariant(
                "Promise capability capture contains an internal value sentinel",
            ));
        }
        match &self.object(id)?.payload {
            ObjectPayload::NativeFunction {
                data:
                    NativeFunctionData {
                        target: NativeFunctionId::PromiseCapabilityExecutor,
                        ..
                    },
                internal: Some(InternalCallableData::PromiseCapabilityExecutor(capture)),
            } if capture
                .resolve
                .as_ref()
                .is_none_or(|value| matches!(value, RawValue::Undefined))
                && capture
                    .reject
                    .as_ref()
                    .is_none_or(|value| matches!(value, RawValue::Undefined)) => {}
            ObjectPayload::NativeFunction {
                data:
                    NativeFunctionData {
                        target: NativeFunctionId::PromiseCapabilityExecutor,
                        ..
                    },
                internal: Some(InternalCallableData::PromiseCapabilityExecutor(_)),
            } => {
                return Ok(false);
            }
            _ => {
                return Err(HeapError::Invariant(
                    "Promise capability capture reached the wrong native function",
                ));
            }
        }

        let mut edges = raw_value_edges(&resolve);
        edges.extend(raw_value_edges(&reject));
        self.retain_edges_transactionally(&edges)?;
        let ObjectPayload::NativeFunction {
            internal: Some(InternalCallableData::PromiseCapabilityExecutor(capture)),
            ..
        } = &mut self.object_mut(id)?.payload
        else {
            unreachable!("Promise capability executor was validated before retaining arguments")
        };
        capture.resolve = Some(resolve);
        capture.reject = Some(reject);
        Ok(true)
    }

    /// Borrow a copy of the current NewPromiseCapability capture.
    /// Raw values in the result do not own additional heap or atom references.
    pub(crate) fn promise_capability_capture(
        &self,
        id: ObjectId,
    ) -> Result<PromiseCapabilityExecutorData, HeapError> {
        match &self.object(id)?.payload {
            ObjectPayload::NativeFunction {
                data:
                    NativeFunctionData {
                        target: NativeFunctionId::PromiseCapabilityExecutor,
                        ..
                    },
                internal: Some(InternalCallableData::PromiseCapabilityExecutor(capture)),
            } => Ok(capture.clone()),
            _ => Err(HeapError::Invariant(
                "Promise capability lookup reached the wrong native function",
            )),
        }
    }

    /// Borrow a complete snapshot of one genuine Promise's hidden state.
    /// Raw edges in the clone are not independently retained.
    pub(crate) fn promise_snapshot(&self, id: ObjectId) -> Result<PromiseData, HeapError> {
        let ObjectPayload::Promise(data) = &self.object(id)?.payload else {
            return Err(HeapError::Invariant(
                "Promise snapshot reached an object with the wrong class",
            ));
        };
        Ok(data.clone())
    }

    /// Append the paired reactions created by one `PerformPromiseThen` call.
    /// Every handler and present capability identity is retained
    /// transactionally before either vector becomes observable to the
    /// collector.
    pub(crate) fn promise_add_reactions(
        &mut self,
        id: ObjectId,
        fulfill: PromiseReaction,
        reject: PromiseReaction,
    ) -> Result<(), HeapError> {
        if fulfill.kind != PromiseReactionKind::Fulfill
            || reject.kind != PromiseReactionKind::Reject
        {
            return Err(HeapError::Invariant(
                "Promise reactions were appended to the wrong settlement lists",
            ));
        }
        match &self.object(id)?.payload {
            ObjectPayload::Promise(PromiseData {
                state: PromiseState::Pending,
                ..
            }) => {}
            ObjectPayload::Promise(_) => {
                return Err(HeapError::Invariant(
                    "cannot append reactions to a settled Promise",
                ));
            }
            _ => {
                return Err(HeapError::Invariant(
                    "Promise reaction append reached an object with the wrong class",
                ));
            }
        }

        let mut edges = promise_reaction_edges(&fulfill);
        edges.extend(promise_reaction_edges(&reject));
        self.retain_edges_transactionally(&edges)?;
        let ObjectPayload::Promise(data) = &mut self.object_mut(id)?.payload else {
            unreachable!("Promise payload was validated before retaining reactions")
        };
        data.fulfill_reactions.push(fulfill);
        data.reject_reactions.push(reject);
        Ok(())
    }

    /// Settle one pending Promise and detach all pending reaction ownership.
    ///
    /// The runtime must first snapshot and enqueue the selected reaction list;
    /// job enqueue retains its own edges.  This method then publishes the
    /// settled result, releases both obsolete reaction lists, and returns any
    /// detached Symbol atom ownership through the usual cleanup channel.
    pub(crate) fn promise_settle(
        &mut self,
        id: ObjectId,
        state: PromiseState,
        result: RawValue,
    ) -> Result<HeapCleanup, HeapError> {
        if state == PromiseState::Pending || !is_promise_storable_value(&result) {
            return Err(HeapError::Invariant(
                "Promise settlement requires a final state and ordinary value",
            ));
        }
        match &self.object(id)?.payload {
            ObjectPayload::Promise(PromiseData {
                state: PromiseState::Pending,
                ..
            }) => {}
            ObjectPayload::Promise(_) => {
                return Err(HeapError::Invariant("Promise was settled more than once"));
            }
            _ => {
                return Err(HeapError::Invariant(
                    "Promise settlement reached an object with the wrong class",
                ));
            }
        }

        let new_edges = raw_value_edges(&result);
        self.retain_edges_transactionally(&new_edges)?;
        let (previous, fulfill_reactions, reject_reactions) = {
            let ObjectPayload::Promise(data) = &mut self.object_mut(id)?.payload else {
                unreachable!("Promise payload was validated before retaining its result")
            };
            let previous = std::mem::replace(&mut data.result, result);
            data.state = state;
            (
                previous,
                std::mem::take(&mut data.fulfill_reactions),
                std::mem::take(&mut data.reject_reactions),
            )
        };

        let mut cleanup = HeapCleanup::default();
        cleanup.atoms.extend(raw_value_atom(&previous));
        for edge in raw_value_edges(&previous) {
            self.release_raw_no_drain(edge)?;
        }
        for reaction in fulfill_reactions.iter().chain(&reject_reactions) {
            for edge in promise_reaction_edges(reaction) {
                self.release_raw_no_drain(edge)?;
            }
        }
        cleanup.merge(self.drain_zero_queue()?);
        Ok(cleanup)
    }

    /// Mark one genuine Promise handled and report whether it was already
    /// handled, allowing the runtime to mirror QuickJS rejection tracking.
    pub(crate) fn promise_mark_handled(&mut self, id: ObjectId) -> Result<bool, HeapError> {
        let ObjectPayload::Promise(data) = &mut self.object_mut(id)?.payload else {
            return Err(HeapError::Invariant(
                "Promise handled update reached an object with the wrong class",
            ));
        };
        Ok(std::mem::replace(&mut data.is_handled, true))
    }

    /// Read immutable executable data without promoting any raw cpool edges.
    pub fn function_bytecode(
        &self,
        id: FunctionBytecodeId,
    ) -> Result<&FunctionBytecodeData, HeapError> {
        match self.live_node(RawId::FunctionBytecode(id))?.data {
            NodeData::FunctionBytecode(ref bytecode) => Ok(bytecode),
            NodeData::Object(_)
            | NodeData::Shape(_)
            | NodeData::VarRef(_)
            | NodeData::Context(_) => Err(HeapError::Invariant(
                "typed bytecode lookup reached another node payload",
            )),
        }
    }

    /// Replace the value stored in a captured-variable cell transactionally.
    ///
    /// The new value's GC edge is retained before the old edge is detached.
    /// A symbol atom transfers to the heap on success; any atom owned by the
    /// previous value is returned in the cleanup.
    pub fn replace_var_ref_value(
        &mut self,
        id: VarRefId,
        replacement: RawValue,
    ) -> Result<HeapCleanup, HeapError> {
        let current = self.var_ref(id)?;
        validate_var_ref_value(
            current.kind,
            current.is_lexical,
            current.is_const,
            &replacement,
        )?;
        let new_edges = raw_value_edges(&replacement);
        self.retain_edges_transactionally(&new_edges)?;

        let previous = {
            let var_ref = self.var_ref_mut(id)?;
            std::mem::replace(&mut var_ref.value, replacement)
        };

        let mut cleanup = HeapCleanup::default();
        cleanup.atoms.extend(raw_value_atom(&previous));
        for edge in raw_value_edges(&previous) {
            self.release_raw_no_drain(edge)?;
        }
        cleanup.merge(self.drain_zero_queue()?);
        Ok(cleanup)
    }

    /// Update binding-mode metadata without disturbing the shared value or
    /// any of its retained GC edges.
    pub fn set_var_ref_metadata(
        &mut self,
        id: VarRefId,
        is_lexical: bool,
        is_const: bool,
        kind: ClosureVariableKind,
    ) -> Result<(), HeapError> {
        let var_ref = self.var_ref_mut(id)?;
        validate_var_ref_value(kind, is_lexical, is_const, &var_ref.value)?;
        var_ref.is_lexical = is_lexical;
        var_ref.is_const = is_const;
        var_ref.kind = kind;
        Ok(())
    }

    /// Advance one branded String Iterator by one Unicode code point.
    ///
    /// The stored cursor is a UTF-16 code-unit index. A valid lead/trail pair
    /// advances by two and is returned unchanged as a two-unit string; every
    /// lone surrogate advances by one and is preserved verbatim. At end the
    /// backing string is released eagerly, matching QuickJS's transition to
    /// an undefined iterator target.
    pub fn string_iterator_next(&mut self, id: ObjectId) -> Result<Option<JsString>, HeapError> {
        let object = self.object_mut(id)?;
        let ObjectPayload::StringIterator { string, next_index } = &mut object.payload else {
            return Err(HeapError::Invariant(
                "String Iterator next reached an object with the wrong class",
            ));
        };
        let Some(value) = string.as_ref() else {
            return Ok(None);
        };
        if *next_index >= value.len() {
            *string = None;
            return Ok(None);
        }

        let first = value
            .code_unit_at(*next_index)
            .expect("validated String Iterator index must name a code unit");
        let pair = (0xd800..=0xdbff).contains(&first)
            && next_index
                .checked_add(1)
                .and_then(|index| value.code_unit_at(index))
                .is_some_and(|unit| (0xdc00..=0xdfff).contains(&unit));
        let width = if pair { 2 } else { 1 };
        let result = JsString::try_from_utf16((0..width).map(|offset| {
            value
                .code_unit_at(*next_index + offset)
                .expect("validated String Iterator code-point width must remain in bounds")
        }))
        .map_err(|_| HeapError::Invariant("String Iterator produced an oversized code point"))?;
        *next_index += width;
        Ok(Some(result))
    }

    /// Read the internal millisecond time value of one genuine Date object.
    pub fn date_value(&self, id: ObjectId) -> Result<f64, HeapError> {
        match &self.object(id)?.payload {
            ObjectPayload::Date(value) => Ok(*value),
            _ => Err(HeapError::Invariant(
                "Date value requested for an object with the wrong class",
            )),
        }
    }

    /// Replace the internal millisecond time value of one genuine Date.
    /// This payload owns no arena or atom edges, so mutation is infallible
    /// after the branded object identity has been validated.
    pub fn set_date_value(&mut self, id: ObjectId, value: f64) -> Result<(), HeapError> {
        let ObjectPayload::Date(current) = &mut self.object_mut(id)?.payload else {
            return Err(HeapError::Invariant(
                "Date value update reached an object with the wrong class",
            ));
        };
        *current = value;
        Ok(())
    }

    /// Read the typed internal state of one genuine RegExp object.
    pub fn regexp_data(&self, id: ObjectId) -> Result<&RegExpObjectData, HeapError> {
        let ObjectPayload::RegExp(data) = &self.object(id)?.payload else {
            return Err(HeapError::Invariant(
                "RegExp data requested for an object with the wrong class",
            ));
        };
        Ok(data)
    }

    /// Replace one genuine RegExp object's source/program state.
    ///
    /// Both variants are reference-counted leaves without arena or atom
    /// edges, so mutation needs no retain/release transaction in this heap.
    pub fn replace_regexp_data(
        &mut self,
        id: ObjectId,
        replacement: RegExpObjectData,
    ) -> Result<RegExpObjectData, HeapError> {
        let ObjectPayload::RegExp(current) = &mut self.object_mut(id)?.payload else {
            return Err(HeapError::Invariant(
                "RegExp data update reached an object with the wrong class",
            ));
        };
        Ok(std::mem::replace(current, replacement))
    }

    /// Snapshot one branded RegExp String Iterator's retained matcher, input
    /// string, cached flag modes, and completion state.
    pub fn regexp_string_iterator_state(
        &self,
        id: ObjectId,
    ) -> Result<(ObjectId, JsString, bool, bool, bool), HeapError> {
        let ObjectPayload::RegExpStringIterator {
            regexp,
            string,
            global,
            full_unicode,
            done,
        } = &self.object(id)?.payload
        else {
            return Err(HeapError::Invariant(
                "RegExp String Iterator state reached an object with the wrong class",
            ));
        };
        Ok((*regexp, string.clone(), *global, *full_unicode, *done))
    }

    /// Mark one branded RegExp String Iterator complete without releasing its
    /// matcher or input string. Pinned QuickJS retains both payload values until
    /// the iterator object itself is finalized.
    pub fn finish_regexp_string_iterator(&mut self, id: ObjectId) -> Result<(), HeapError> {
        let ObjectPayload::RegExpStringIterator { done, .. } = &mut self.object_mut(id)?.payload
        else {
            return Err(HeapError::Invariant(
                "RegExp String Iterator completion reached an object with the wrong class",
            ));
        };
        *done = true;
        Ok(())
    }

    /// Snapshot one branded Array Iterator's live target, cursor, and mode.
    pub fn array_iterator_state(
        &self,
        id: ObjectId,
    ) -> Result<(Option<ObjectId>, u32, ArrayIteratorKind), HeapError> {
        let ObjectPayload::ArrayIterator {
            object,
            next_index,
            kind,
        } = &self.object(id)?.payload
        else {
            return Err(HeapError::Invariant(
                "Array Iterator state reached an object with the wrong class",
            ));
        };
        Ok((*object, *next_index, *kind))
    }

    /// Advance a branded Array Iterator after its current element has been
    /// selected. Property lookup may still throw after this update, matching
    /// QuickJS's cursor order.
    pub fn set_array_iterator_index(
        &mut self,
        id: ObjectId,
        next_index: u32,
    ) -> Result<(), HeapError> {
        let ObjectPayload::ArrayIterator {
            object,
            next_index: stored,
            ..
        } = &mut self.object_mut(id)?.payload
        else {
            return Err(HeapError::Invariant(
                "Array Iterator advance reached an object with the wrong class",
            ));
        };
        if object.is_none() {
            return Err(HeapError::Invariant(
                "completed Array Iterator was advanced",
            ));
        }
        *stored = next_index;
        Ok(())
    }

    /// Permanently detach a completed Array Iterator target and release its
    /// owned object edge.
    pub fn finish_array_iterator(&mut self, id: ObjectId) -> Result<HeapCleanup, HeapError> {
        let source = {
            let ObjectPayload::ArrayIterator { object, .. } = &mut self.object_mut(id)?.payload
            else {
                return Err(HeapError::Invariant(
                    "Array Iterator completion reached an object with the wrong class",
                ));
            };
            object.take()
        };
        let Some(source) = source else {
            return Ok(HeapCleanup::default());
        };
        self.release_raw_no_drain(RawId::Object(source))?;
        self.drain_zero_queue()
    }
    /// Snapshot one genuine Iterator Helper payload. Raw values in the clone
    /// do not own additional arena or atom references.
    pub(crate) fn iterator_helper_state(
        &self,
        id: ObjectId,
    ) -> Result<IteratorHelperData, HeapError> {
        let ObjectPayload::IteratorHelper(data) = &self.object(id)?.payload else {
            return Err(HeapError::Invariant(
                "Iterator Helper snapshot reached an object with the wrong class",
            ));
        };
        Ok(data.clone())
    }

    /// Snapshot one `Iterator.from` forwarding wrapper. Raw values in the
    /// clone do not own additional arena or atom references.
    pub(crate) fn iterator_wrap_state(
        &self,
        id: ObjectId,
    ) -> Result<(RawValue, RawValue), HeapError> {
        let ObjectPayload::IteratorWrap(data) = &self.object(id)?.payload else {
            return Err(HeapError::Invariant(
                "Iterator Wrap snapshot reached an object with the wrong class",
            ));
        };
        Ok((data.source.clone(), data.next.clone()))
    }

    /// Snapshot one branded Async-from-Sync iterator. The cached raw method
    /// does not gain an additional arena or atom occurrence in the clone.
    pub(crate) fn async_from_sync_iterator_state(
        &self,
        id: ObjectId,
    ) -> Result<(ObjectId, RawValue), HeapError> {
        let ObjectPayload::AsyncFromSyncIterator(data) = &self.object(id)?.payload else {
            return Err(HeapError::Invariant(
                "Async-from-Sync Iterator snapshot reached an object with the wrong class",
            ));
        };
        Ok((data.sync_iterator, data.next.clone()))
    }

    /// Snapshot one `Iterator.concat` state machine. Raw values in the clone
    /// do not own additional arena or atom references.
    pub(crate) fn iterator_concat_state(
        &self,
        id: ObjectId,
    ) -> Result<IteratorConcatData, HeapError> {
        let ObjectPayload::IteratorConcat(data) = &self.object(id)?.payload else {
            return Err(HeapError::Invariant(
                "Iterator Concat snapshot reached an object with the wrong class",
            ));
        };
        Ok(data.clone())
    }

    /// Set or clear the `Iterator.concat` reentrancy guard.
    pub(crate) fn set_iterator_concat_running(
        &mut self,
        id: ObjectId,
        running: bool,
    ) -> Result<(), HeapError> {
        let ObjectPayload::IteratorConcat(data) = &mut self.object_mut(id)?.payload else {
            return Err(HeapError::Invariant(
                "Iterator Concat execution update reached an object with the wrong class",
            ));
        };
        data.running = running;
        Ok(())
    }

    /// Replace the lazily created current iterator.
    pub(crate) fn set_iterator_concat_iterator(
        &mut self,
        id: ObjectId,
        replacement: Option<ObjectId>,
    ) -> Result<HeapCleanup, HeapError> {
        self.iterator_concat_state(id)?;
        if let Some(replacement) = replacement {
            self.object(replacement)?;
        }
        let new_edges: Vec<_> = replacement.into_iter().map(RawId::Object).collect();
        self.retain_edges_transactionally(&new_edges)?;
        let previous = {
            let ObjectPayload::IteratorConcat(data) = &mut self.object_mut(id)?.payload else {
                return Err(HeapError::Invariant(
                    "Iterator Concat iterator update reached an object with the wrong class",
                ));
            };
            std::mem::replace(&mut data.iterator, replacement)
        };
        if let Some(previous) = previous {
            self.release_raw_no_drain(RawId::Object(previous))?;
        }
        self.drain_zero_queue()
    }

    /// Cache the current iterator's `next` property. Symbol atom ownership is
    /// transferred separately by the runtime before this mutation.
    pub(crate) fn set_iterator_concat_next(
        &mut self,
        id: ObjectId,
        replacement: RawValue,
    ) -> Result<HeapCleanup, HeapError> {
        self.iterator_concat_state(id)?;
        if !is_map_storable_value(&replacement) {
            return Err(HeapError::Invariant(
                "Iterator Concat next cache contains an internal value sentinel",
            ));
        }
        let new_edges = raw_value_edges(&replacement);
        self.retain_edges_transactionally(&new_edges)?;
        let previous = {
            let ObjectPayload::IteratorConcat(data) = &mut self.object_mut(id)?.payload else {
                return Err(HeapError::Invariant(
                    "Iterator Concat next update reached an object with the wrong class",
                ));
            };
            std::mem::replace(&mut data.next, replacement)
        };
        self.release_replaced_raw_value(previous)
    }

    /// Finish the current iterable and advance to the next retained pair.
    pub(crate) fn advance_iterator_concat(
        &mut self,
        id: ObjectId,
    ) -> Result<HeapCleanup, HeapError> {
        let (item, iterator, next) = {
            let ObjectPayload::IteratorConcat(data) = &mut self.object_mut(id)?.payload else {
                return Err(HeapError::Invariant(
                    "Iterator Concat advance reached an object with the wrong class",
                ));
            };
            if data.index >= data.items.len() {
                return Err(HeapError::Invariant(
                    "Iterator Concat advanced past its retained inputs",
                ));
            }
            let item = data.items[data.index].take().ok_or(HeapError::Invariant(
                "Iterator Concat current input was already released",
            ))?;
            let iterator = data.iterator.take().ok_or(HeapError::Invariant(
                "Iterator Concat advanced without a current iterator",
            ))?;
            let next = std::mem::replace(&mut data.next, RawValue::Undefined);
            data.index += 1;
            (item, iterator, next)
        };
        // Pinned QuickJS releases a normally exhausted input in this order:
        // active iterator, cached next, captured open method, iterable.
        let mut cleanup = HeapCleanup::default();
        self.release_raw_no_drain(RawId::Object(iterator))?;
        cleanup.atoms.extend(raw_value_atom(&next));
        for edge in raw_value_edges(&next) {
            self.release_raw_no_drain(edge)?;
        }
        cleanup.atoms.extend(raw_value_atom(&item.method));
        for edge in raw_value_edges(&item.method) {
            self.release_raw_no_drain(edge)?;
        }
        self.release_raw_no_drain(RawId::Object(item.iterable))?;
        cleanup.merge(self.drain_zero_queue()?);
        Ok(cleanup)
    }

    /// Release the current iterator and every unvisited input after
    /// `Iterator Concat.prototype.return` completes.
    pub(crate) fn clear_iterator_concat(&mut self, id: ObjectId) -> Result<HeapCleanup, HeapError> {
        let (items, iterator, next) = {
            let ObjectPayload::IteratorConcat(data) = &mut self.object_mut(id)?.payload else {
                return Err(HeapError::Invariant(
                    "Iterator Concat clear reached an object with the wrong class",
                ));
            };
            let items = data.items[data.index..]
                .iter_mut()
                .filter_map(Option::take)
                .collect::<Vec<_>>();
            data.index = data.items.len();
            let iterator = data.iterator.take();
            let next = std::mem::replace(&mut data.next, RawValue::Undefined);
            (items, iterator, next)
        };

        let mut cleanup = HeapCleanup::default();
        for item in items {
            self.release_raw_no_drain(RawId::Object(item.iterable))?;
            cleanup.atoms.extend(raw_value_atom(&item.method));
            for edge in raw_value_edges(&item.method) {
                self.release_raw_no_drain(edge)?;
            }
            cleanup.merge(self.drain_zero_queue()?);
        }
        if let Some(iterator) = iterator {
            self.release_raw_no_drain(RawId::Object(iterator))?;
        }
        cleanup.atoms.extend(raw_value_atom(&next));
        for edge in raw_value_edges(&next) {
            self.release_raw_no_drain(edge)?;
        }
        cleanup.merge(self.drain_zero_queue()?);
        Ok(cleanup)
    }

    /// Replace the source iterator edge retained by a helper. The new edge is
    /// retained before the previous edge is detached.
    #[cfg(test)]
    pub(crate) fn set_iterator_helper_source(
        &mut self,
        id: ObjectId,
        replacement: ObjectId,
    ) -> Result<HeapCleanup, HeapError> {
        self.object(replacement)?;
        let previous = self.iterator_helper_state(id)?.source;
        self.retain_raw(RawId::Object(replacement), 1)?;
        let ObjectPayload::IteratorHelper(data) = &mut self.object_mut(id)?.payload else {
            unreachable!("Iterator Helper was validated before retaining its source")
        };
        data.source = replacement;
        self.release_raw_no_drain(RawId::Object(previous))?;
        self.drain_zero_queue()
    }

    /// Replace the cached `next` value transactionally. A replacement Symbol
    /// atom transfers on success; the previous Symbol atom is returned in the
    /// cleanup.
    #[cfg(test)]
    pub(crate) fn set_iterator_helper_next(
        &mut self,
        id: ObjectId,
        replacement: RawValue,
    ) -> Result<HeapCleanup, HeapError> {
        self.replace_iterator_helper_raw_value(id, IteratorHelperRawValueField::Next, replacement)
    }

    /// Replace the helper callback transactionally, preserving its callable
    /// invariant and retaining the replacement edge before detaching the old
    /// callback.
    #[cfg(test)]
    pub(crate) fn set_iterator_helper_callback(
        &mut self,
        id: ObjectId,
        replacement: RawValue,
    ) -> Result<HeapCleanup, HeapError> {
        self.replace_iterator_helper_raw_value(
            id,
            IteratorHelperRawValueField::Callback,
            replacement,
        )
    }

    /// Replace the optional inner iterator used by `flatMap`.
    pub(crate) fn set_iterator_helper_inner(
        &mut self,
        id: ObjectId,
        replacement: Option<ObjectId>,
    ) -> Result<HeapCleanup, HeapError> {
        if let Some(replacement) = replacement {
            self.object(replacement)?;
        }
        let current = self.iterator_helper_state(id)?;
        if current.kind != IteratorHelperKind::FlatMap && replacement.is_some() {
            return Err(HeapError::Invariant(
                "only a flatMap Iterator Helper may retain an inner iterator",
            ));
        }
        let new_edges: Vec<_> = replacement.into_iter().map(RawId::Object).collect();
        self.retain_edges_transactionally(&new_edges)?;
        let previous = {
            let ObjectPayload::IteratorHelper(data) = &mut self.object_mut(id)?.payload else {
                unreachable!("Iterator Helper was validated before retaining its inner iterator")
            };
            std::mem::replace(&mut data.inner, replacement)
        };
        if let Some(previous) = previous {
            self.release_raw_no_drain(RawId::Object(previous))?;
        }
        self.drain_zero_queue()
    }

    #[cfg(test)]
    fn replace_iterator_helper_raw_value(
        &mut self,
        id: ObjectId,
        field: IteratorHelperRawValueField,
        replacement: RawValue,
    ) -> Result<HeapCleanup, HeapError> {
        let mut candidate = self.iterator_helper_state(id)?;
        *field.get_mut(&mut candidate) = replacement.clone();
        validate_iterator_helper_data(self, &candidate)?;

        let new_edges = raw_value_edges(&replacement);
        self.retain_edges_transactionally(&new_edges)?;
        let previous = {
            let ObjectPayload::IteratorHelper(data) = &mut self.object_mut(id)?.payload else {
                unreachable!("Iterator Helper was validated before retaining replacement edges")
            };
            std::mem::replace(field.get_mut(data), replacement)
        };
        self.release_replaced_raw_value(previous)
    }

    /// Replace the source iterator retained by an Iterator Wrap.
    #[cfg(test)]
    pub(crate) fn set_iterator_wrap_source(
        &mut self,
        id: ObjectId,
        replacement: RawValue,
    ) -> Result<HeapCleanup, HeapError> {
        let (_, next) = self.iterator_wrap_state(id)?;
        let candidate = IteratorWrapData {
            source: replacement.clone(),
            next,
        };
        validate_iterator_wrap_data(self, &candidate)?;

        let new_edges = raw_value_edges(&replacement);
        self.retain_edges_transactionally(&new_edges)?;
        let previous = {
            let ObjectPayload::IteratorWrap(data) = &mut self.object_mut(id)?.payload else {
                unreachable!("Iterator Wrap was validated before retaining replacement edges")
            };
            std::mem::replace(&mut data.source, replacement)
        };
        self.release_replaced_raw_value(previous)
    }

    /// Replace the cached `next` value retained by an Iterator Wrap.
    #[cfg(test)]
    pub(crate) fn set_iterator_wrap_next(
        &mut self,
        id: ObjectId,
        replacement: RawValue,
    ) -> Result<HeapCleanup, HeapError> {
        let (source, _) = self.iterator_wrap_state(id)?;
        let candidate = IteratorWrapData {
            source,
            next: replacement.clone(),
        };
        validate_iterator_wrap_data(self, &candidate)?;

        let new_edges = raw_value_edges(&replacement);
        self.retain_edges_transactionally(&new_edges)?;
        let previous = {
            let ObjectPayload::IteratorWrap(data) = &mut self.object_mut(id)?.payload else {
                unreachable!("Iterator Wrap was validated before retaining replacement edges")
            };
            std::mem::replace(&mut data.next, replacement)
        };
        self.release_replaced_raw_value(previous)
    }
    /// Update the helper's signed 64-bit limit/callback index.
    pub(crate) fn set_iterator_helper_count(
        &mut self,
        id: ObjectId,
        count: i64,
    ) -> Result<(), HeapError> {
        let mut candidate = self.iterator_helper_state(id)?;
        candidate.count = count;
        validate_iterator_helper_data(self, &candidate)?;
        let ObjectPayload::IteratorHelper(data) = &mut self.object_mut(id)?.payload else {
            unreachable!("Iterator Helper was validated before updating its count")
        };
        data.count = count;
        Ok(())
    }

    /// Set or clear QuickJS's reentrancy guard.
    pub(crate) fn set_iterator_helper_running(
        &mut self,
        id: ObjectId,
        running: bool,
    ) -> Result<(), HeapError> {
        let ObjectPayload::IteratorHelper(data) = &mut self.object_mut(id)?.payload else {
            return Err(HeapError::Invariant(
                "Iterator Helper execution update reached an object with the wrong class",
            ));
        };
        data.executing = running;
        Ok(())
    }

    /// Update completion and reentrancy flags together. Marking a helper done
    /// preserves every payload-owned value until object finalization.
    pub(crate) fn set_iterator_helper_done_and_running(
        &mut self,
        id: ObjectId,
        done: bool,
        running: bool,
    ) -> Result<(), HeapError> {
        let mut candidate = self.iterator_helper_state(id)?;
        candidate.done = done;
        candidate.executing = running;
        validate_iterator_helper_data(self, &candidate)?;
        let ObjectPayload::IteratorHelper(data) = &mut self.object_mut(id)?.payload else {
            unreachable!("Iterator Helper was validated before completing its resume")
        };
        data.done = done;
        data.executing = running;
        Ok(())
    }

    /// Read QuickJS's representation-sensitive dense count for a genuine
    /// Array. `None` means the Array has converted to slow properties.
    pub fn array_dense_len(&self, id: ObjectId) -> Result<Option<u32>, HeapError> {
        match &self.object(id)?.payload {
            ObjectPayload::Array { dense: Some(dense) } => {
                Ok(Some(u32::try_from(dense.len()).map_err(|_| {
                    HeapError::Invariant("fast Array count exceeded Uint32")
                })?))
            }
            ObjectPayload::Array { dense: None } => Ok(None),
            _ => Err(HeapError::Invariant(
                "Array dense state requested for an object with the wrong class",
            )),
        }
    }

    /// Borrow QuickJS's physical dense prefix. The returned values own no
    /// additional arena or AtomTable references.
    pub fn array_dense_values(&self, id: ObjectId) -> Result<Option<&[RawValue]>, HeapError> {
        match &self.object(id)?.payload {
            ObjectPayload::Array { dense } => Ok(dense.as_deref()),
            _ => Err(HeapError::Invariant(
                "Array dense values requested for an object with the wrong class",
            )),
        }
    }

    /// Append one consecutive C/W/E element to a fast Array. Object edges are
    /// retained before publication. A Symbol atom must already be owned by the
    /// caller and transfers to the Array only when this operation succeeds.
    pub fn append_array_dense_value(
        &mut self,
        id: ObjectId,
        value: RawValue,
    ) -> Result<(), HeapError> {
        if !is_map_storable_value(&value) {
            return Err(HeapError::Invariant(
                "fast Array contains an internal value sentinel",
            ));
        }
        {
            let ObjectPayload::Array { dense: Some(dense) } = &mut self.object_mut(id)?.payload
            else {
                return Err(HeapError::Invariant(
                    "dense append reached a slow Array or an object with the wrong class",
                ));
            };
            if dense.len() >= u32::MAX as usize {
                return Err(HeapError::Overflow {
                    operation: "growing fast Array count",
                });
            }
            dense.try_reserve(1).map_err(|_| HeapError::Allocation {
                operation: "growing fast Array storage",
            })?;
        }
        self.retain_edges_transactionally(&raw_value_edges(&value))?;
        let ObjectPayload::Array { dense: Some(dense) } = &mut self.object_mut(id)?.payload else {
            unreachable!("fast Array changed representation while retaining its new value")
        };
        dense.push(value);
        Ok(())
    }

    /// Append to a newly constructed Array whose logical length still equals
    /// its dense count. This is QuickJS's `add_fast_array_element` substrate
    /// for literals, builtin result arrays, and JSON parsing: allocation and
    /// edge retention complete before the infallible length-slot publication.
    pub fn append_fresh_array_dense_value(
        &mut self,
        id: ObjectId,
        value: RawValue,
    ) -> Result<(), HeapError> {
        let next_len = {
            let object = self.object(id)?;
            let ObjectPayload::Array { dense: Some(dense) } = &object.payload else {
                return Err(HeapError::Invariant(
                    "fresh dense append reached a slow Array or wrong object class",
                ));
            };
            let dense_len = u32::try_from(dense.len())
                .map_err(|_| HeapError::Invariant("fast Array count exceeded Uint32"))?;
            let shape = self.shape(object.shape)?;
            let length = shape.entries().first().ok_or(HeapError::Invariant(
                "fresh Array has no physical length entry",
            ))?;
            let stored_len = match object.slots.first() {
                Some(PropertySlot::Data(RawValue::Int(length))) if *length >= 0 => *length as u32,
                Some(PropertySlot::Data(RawValue::Float(length)))
                    if length.is_finite()
                        && *length >= 0.0
                        && *length <= f64::from(u32::MAX)
                        && length.fract() == 0.0 =>
                {
                    *length as u32
                }
                _ => {
                    return Err(HeapError::Invariant(
                        "fresh Array length is not an exact Uint32 data value",
                    ));
                }
            };
            if !length.flags.writable || stored_len != dense_len {
                return Err(HeapError::Invariant(
                    "fresh Array length diverged from its dense count",
                ));
            }
            dense_len.checked_add(1).ok_or(HeapError::Overflow {
                operation: "growing fresh Array length",
            })?
        };

        self.append_array_dense_value(id, value)?;
        let replacement = if let Ok(length) = i32::try_from(next_len) {
            RawValue::Int(length)
        } else {
            RawValue::Float(f64::from(next_len))
        };
        let object = self
            .object_mut(id)
            .expect("fresh Array disappeared after retaining its appended value");
        let Some(PropertySlot::Data(length)) = object.slots.first_mut() else {
            unreachable!("fresh Array length slot changed after preflight")
        };
        *length = replacement;
        Ok(())
    }

    /// Replace one existing fast element transactionally. New edges are
    /// retained before the previous value and its Symbol atom are detached.
    pub fn replace_array_dense_value(
        &mut self,
        id: ObjectId,
        index: u32,
        replacement: RawValue,
    ) -> Result<HeapCleanup, HeapError> {
        if !is_map_storable_value(&replacement) {
            return Err(HeapError::Invariant(
                "fast Array contains an internal value sentinel",
            ));
        }
        let index = index as usize;
        match &self.object(id)?.payload {
            ObjectPayload::Array { dense: Some(dense) } if index < dense.len() => {}
            ObjectPayload::Array { dense: Some(_) } => {
                return Err(HeapError::Invariant(
                    "fast Array replacement index is outside its dense prefix",
                ));
            }
            ObjectPayload::Array { dense: None } => {
                return Err(HeapError::Invariant(
                    "fast Array replacement reached a slow Array",
                ));
            }
            _ => {
                return Err(HeapError::Invariant(
                    "fast Array replacement reached an object with the wrong class",
                ));
            }
        }
        self.retain_edges_transactionally(&raw_value_edges(&replacement))?;
        let previous = {
            let ObjectPayload::Array { dense: Some(dense) } = &mut self.object_mut(id)?.payload
            else {
                unreachable!("fast Array changed representation during value replacement")
            };
            std::mem::replace(&mut dense[index], replacement)
        };
        self.release_replaced_raw_value(previous)
    }

    /// Reserve every container needed to shorten a fast Array prefix without
    /// changing either its dense storage or logical `length` slot.
    pub(crate) fn prepare_array_dense_truncation(
        &self,
        id: ObjectId,
        new_len: u32,
    ) -> Result<PreparedArrayDenseTruncation, HeapError> {
        let ObjectPayload::Array { dense: Some(dense) } = &self.object(id)?.payload else {
            return Err(HeapError::Invariant(
                "fast Array truncation reached a slow Array or wrong object class",
            ));
        };
        let new_len = new_len as usize;
        let removal_count = dense
            .len()
            .checked_sub(new_len)
            .ok_or(HeapError::Invariant(
                "fast Array truncation attempted to grow its dense prefix",
            ))?;
        let removed_atom_count = dense[new_len..]
            .iter()
            .filter(|value| raw_value_atom(value).is_some())
            .count();
        let mut removed = Vec::new();
        removed
            .try_reserve_exact(removal_count)
            .map_err(|_| HeapError::Allocation {
                operation: "detaching fast Array tail",
            })?;
        let mut cleanup = HeapCleanup::default();
        cleanup
            .atoms
            .try_reserve(removed_atom_count)
            .map_err(|_| HeapError::Allocation {
                operation: "recording detached fast Array atoms",
            })?;
        Ok(PreparedArrayDenseTruncation {
            object: id,
            original_len: dense.len(),
            new_len,
            removed_atom_count,
            removed,
            cleanup,
        })
    }

    /// Publish an allocation-complete fast Array truncation and detach every
    /// removed edge and Symbol atom.
    pub(crate) fn commit_array_dense_truncation(
        &mut self,
        mut prepared: PreparedArrayDenseTruncation,
    ) -> Result<HeapCleanup, HeapError> {
        {
            let ObjectPayload::Array { dense: Some(dense) } =
                &mut self.object_mut(prepared.object)?.payload
            else {
                unreachable!("fast Array changed representation before tail truncation")
            };
            if dense.len() != prepared.original_len
                || dense[prepared.new_len..]
                    .iter()
                    .filter(|value| raw_value_atom(value).is_some())
                    .count()
                    != prepared.removed_atom_count
            {
                return Err(HeapError::Invariant(
                    "fast Array changed after truncation preparation",
                ));
            }
            prepared.removed.extend(dense.drain(prepared.new_len..));
        }
        self.release_raw_values_into(prepared.removed, prepared.cleanup)
    }

    /// Truncate the contiguous fast prefix without changing the Array's
    /// logical `length` slot. Every removed edge and Symbol atom is detached.
    pub fn truncate_array_dense(
        &mut self,
        id: ObjectId,
        new_len: u32,
    ) -> Result<HeapCleanup, HeapError> {
        let prepared = self.prepare_array_dense_truncation(id, new_len)?;
        self.commit_array_dense_truncation(prepared)
    }

    /// Read one Arguments object's representation-sensitive indexed prefix.
    pub fn arguments_state(&self, id: ObjectId) -> Result<(bool, Option<u32>), HeapError> {
        match &self.object(id)?.payload {
            ObjectPayload::Arguments { mapped, fast_len } => Ok((*mapped, *fast_len)),
            _ => Err(HeapError::Invariant(
                "Arguments state requested for an object with the wrong class",
            )),
        }
    }

    /// Update one Arguments object's fast indexed representation. Conversion
    /// to `None` is irreversible at the runtime semantic boundary.
    pub fn set_arguments_fast_len(
        &mut self,
        id: ObjectId,
        fast_len: Option<u32>,
    ) -> Result<(), HeapError> {
        let ObjectPayload::Arguments {
            fast_len: current, ..
        } = &mut self.object_mut(id)?.payload
        else {
            return Err(HeapError::Invariant(
                "Arguments fast state update reached an object with the wrong class",
            ));
        };
        *current = fast_len;
        Ok(())
    }

    /// Clone one generator's raw dormant activation while its object still
    /// owns every referenced edge. The runtime immediately maps this snapshot
    /// to rooted handles before beginning the destructive resume transition.
    pub fn generator_snapshot(
        &self,
        id: ObjectId,
    ) -> Result<(GeneratorState, Option<GeneratorActivationData>), HeapError> {
        let ObjectPayload::Generator { state, activation } = &self.object(id)?.payload else {
            return Err(HeapError::Invariant(
                "Generator state requested for an object with the wrong class",
            ));
        };
        Ok((*state, activation.as_deref().cloned()))
    }

    /// Move a suspended generator to `Executing` and detach the heap-owned
    /// activation edges. The caller must already have rooted a snapshot of the
    /// returned activation so releasing these occurrences cannot invalidate
    /// the active Rust representation.
    pub fn begin_generator_resume(
        &mut self,
        id: ObjectId,
    ) -> Result<(GeneratorState, GeneratorActivationData, HeapCleanup), HeapError> {
        let (state, activation) = {
            let ObjectPayload::Generator { state, activation } = &mut self.object_mut(id)?.payload
            else {
                return Err(HeapError::Invariant(
                    "Generator resume reached an object with the wrong class",
                ));
            };
            if !matches!(
                state,
                GeneratorState::SuspendedStart
                    | GeneratorState::SuspendedYield
                    | GeneratorState::SuspendedYieldStar
            ) {
                return Err(HeapError::Invariant(
                    "Generator resume began outside a suspended state",
                ));
            }
            let previous = *state;
            let activation = activation.take().ok_or(HeapError::Invariant(
                "suspended Generator has no activation",
            ))?;
            *state = GeneratorState::Executing;
            (previous, *activation)
        };
        let edges = generator_activation_edges(&activation);
        let atoms = generator_activation_atoms(&activation);
        for edge in edges {
            self.release_raw_no_drain(edge)?;
        }
        let mut cleanup = self.drain_zero_queue()?;
        cleanup.atoms.extend(atoms);
        Ok((state, activation, cleanup))
    }

    /// Reattach a fully encoded dormant frame after an executing generator
    /// reaches its next suspension point. Atom occurrences are retained by the
    /// runtime before this call; arena edges are retained transactionally here.
    pub fn suspend_generator(
        &mut self,
        id: ObjectId,
        state: GeneratorState,
        activation: GeneratorActivationData,
    ) -> Result<(), HeapError> {
        if !matches!(
            state,
            GeneratorState::SuspendedStart
                | GeneratorState::SuspendedYield
                | GeneratorState::SuspendedYieldStar
        ) {
            return Err(HeapError::Invariant(
                "Generator suspended with a non-suspended state",
            ));
        }
        match &self.object(id)?.payload {
            ObjectPayload::Generator {
                state: GeneratorState::Executing,
                activation: None,
            } => {}
            ObjectPayload::Generator { .. } => {
                return Err(HeapError::Invariant(
                    "Generator suspension did not follow an executing state",
                ));
            }
            _ => {
                return Err(HeapError::Invariant(
                    "Generator suspension reached an object with the wrong class",
                ));
            }
        }
        let mut candidate = self.object(id)?.clone();
        candidate.payload = ObjectPayload::Generator {
            state,
            activation: Some(Box::new(activation.clone())),
        };
        self.validate_object_layout(&candidate)?;
        let edges = generator_activation_edges(&activation);
        self.retain_edges_transactionally(&edges)?;
        let ObjectPayload::Generator {
            state: current,
            activation: current_activation,
        } = &mut self.object_mut(id)?.payload
        else {
            unreachable!("Generator payload was validated before suspension")
        };
        *current = state;
        *current_activation = Some(Box::new(activation));
        Ok(())
    }

    /// Permanently finish an executing generator after return, throw, or an
    /// abrupt resume failure. Its dormant edges were already detached by
    /// [`Self::begin_generator_resume`].
    pub fn complete_generator(&mut self, id: ObjectId) -> Result<(), HeapError> {
        let ObjectPayload::Generator { state, activation } = &mut self.object_mut(id)?.payload
        else {
            return Err(HeapError::Invariant(
                "Generator completion reached an object with the wrong class",
            ));
        };
        if *state != GeneratorState::Executing || activation.is_some() {
            return Err(HeapError::Invariant(
                "Generator completion did not follow an executing state",
            ));
        }
        *state = GeneratorState::Completed;
        Ok(())
    }

    pub(crate) fn async_generator_snapshot(
        &self,
        id: ObjectId,
    ) -> Result<AsyncGeneratorData, HeapError> {
        let ObjectPayload::AsyncGenerator(data) = &self.object(id)?.payload else {
            return Err(HeapError::Invariant(
                "AsyncGenerator state requested for an object with the wrong class",
            ));
        };
        Ok(data.clone())
    }

    /// Append one request after the runtime has retained any Symbol atom in
    /// `result`. Arena edges transfer transactionally into the FIFO.
    pub(crate) fn async_generator_enqueue(
        &mut self,
        id: ObjectId,
        request: AsyncGeneratorRequestData,
    ) -> Result<(), HeapError> {
        if !is_promise_storable_value(&request.result) {
            return Err(HeapError::Invariant(
                "AsyncGenerator request contains an internal value sentinel",
            ));
        }
        if !matches!(self.object(id)?.payload, ObjectPayload::AsyncGenerator(_)) {
            return Err(HeapError::Invariant(
                "AsyncGenerator request reached an object with the wrong class",
            ));
        }
        let edges = async_generator_request_edges(&request);
        self.retain_edges_transactionally(&edges)?;
        let ObjectPayload::AsyncGenerator(data) = &mut self.object_mut(id)?.payload else {
            unreachable!("AsyncGenerator payload was validated before queue append")
        };
        data.queue.push_back(request);
        Ok(())
    }

    pub(crate) fn async_generator_front_request(
        &self,
        id: ObjectId,
    ) -> Result<Option<AsyncGeneratorRequestData>, HeapError> {
        let ObjectPayload::AsyncGenerator(data) = &self.object(id)?.payload else {
            return Err(HeapError::Invariant(
                "AsyncGenerator request lookup reached an object with the wrong class",
            ));
        };
        Ok(data.queue.front().cloned())
    }

    /// Detach the request which the runtime has already promoted to rooted
    /// values and callables.
    pub(crate) fn async_generator_pop_front(
        &mut self,
        id: ObjectId,
    ) -> Result<(AsyncGeneratorRequestData, HeapCleanup), HeapError> {
        let request = {
            let ObjectPayload::AsyncGenerator(data) = &mut self.object_mut(id)?.payload else {
                return Err(HeapError::Invariant(
                    "AsyncGenerator request removal reached an object with the wrong class",
                ));
            };
            data.queue.pop_front().ok_or(HeapError::Invariant(
                "AsyncGenerator request queue is empty",
            ))?
        };
        for edge in async_generator_request_edges(&request) {
            self.release_raw_no_drain(edge)?;
        }
        let mut cleanup = self.drain_zero_queue()?;
        cleanup.atoms.extend(raw_value_atom(&request.result));
        Ok((request, cleanup))
    }

    /// Move one parked activation into transient rooted runtime ownership.
    pub(crate) fn begin_async_generator_resume(
        &mut self,
        id: ObjectId,
    ) -> Result<(AsyncGeneratorState, GeneratorActivationData, HeapCleanup), HeapError> {
        let (previous, activation, resume_realm) = {
            let ObjectPayload::AsyncGenerator(data) = &mut self.object_mut(id)?.payload else {
                return Err(HeapError::Invariant(
                    "AsyncGenerator resume reached an object with the wrong class",
                ));
            };
            if !matches!(
                data.state,
                AsyncGeneratorState::SuspendedStart
                    | AsyncGeneratorState::SuspendedYield
                    | AsyncGeneratorState::SuspendedYieldStar
                    | AsyncGeneratorState::Executing
            ) {
                return Err(HeapError::Invariant(
                    "AsyncGenerator resume began outside a parked state",
                ));
            }
            let previous = data.state;
            let activation = data.activation.take().ok_or(HeapError::Invariant(
                "parked AsyncGenerator has no activation",
            ))?;
            let resume_realm = data.resume_realm.take();
            data.state = AsyncGeneratorState::Executing;
            (previous, *activation, resume_realm)
        };
        for edge in generator_activation_edges(&activation) {
            self.release_raw_no_drain(edge)?;
        }
        if let Some(realm) = resume_realm {
            self.release_raw_no_drain(RawId::Context(realm))?;
        }
        let mut cleanup = self.drain_zero_queue()?;
        cleanup
            .atoms
            .extend(generator_activation_atoms(&activation));
        Ok((previous, activation, cleanup))
    }

    /// Store a yielded or awaited activation after an executing pump step.
    pub(crate) fn suspend_async_generator(
        &mut self,
        id: ObjectId,
        state: AsyncGeneratorState,
        activation: GeneratorActivationData,
        resume_realm: Option<ContextId>,
    ) -> Result<(), HeapError> {
        if !matches!(
            (state, resume_realm),
            (AsyncGeneratorState::SuspendedYield, None)
                | (AsyncGeneratorState::SuspendedYieldStar, None)
                | (AsyncGeneratorState::Executing, Some(_))
        ) {
            return Err(HeapError::Invariant(
                "AsyncGenerator suspension has an invalid state/realm pair",
            ));
        }
        match &self.object(id)?.payload {
            ObjectPayload::AsyncGenerator(AsyncGeneratorData {
                state: AsyncGeneratorState::Executing,
                activation: None,
                resume_realm: None,
                queue,
            }) if !queue.is_empty() => {}
            ObjectPayload::AsyncGenerator(_) => {
                return Err(HeapError::Invariant(
                    "AsyncGenerator suspension did not follow execution",
                ));
            }
            _ => {
                return Err(HeapError::Invariant(
                    "AsyncGenerator suspension reached an object with the wrong class",
                ));
            }
        }

        let mut candidate = self.object(id)?.clone();
        let ObjectPayload::AsyncGenerator(candidate_data) = &mut candidate.payload else {
            unreachable!("AsyncGenerator payload was validated before suspension")
        };
        candidate_data.state = state;
        candidate_data.activation = Some(Box::new(activation.clone()));
        candidate_data.resume_realm = resume_realm;
        self.validate_object_layout(&candidate)?;

        let mut edges = generator_activation_edges(&activation);
        edges.extend(resume_realm.map(RawId::Context));
        self.retain_edges_transactionally(&edges)?;
        let ObjectPayload::AsyncGenerator(data) = &mut self.object_mut(id)?.payload else {
            unreachable!("AsyncGenerator payload was validated before retaining activation")
        };
        data.state = state;
        data.activation = Some(Box::new(activation));
        data.resume_realm = resume_realm;
        Ok(())
    }

    /// Discard any parked activation and enter the absorbing completed state.
    pub(crate) fn complete_async_generator(
        &mut self,
        id: ObjectId,
    ) -> Result<HeapCleanup, HeapError> {
        let (activation, resume_realm) = {
            let ObjectPayload::AsyncGenerator(data) = &mut self.object_mut(id)?.payload else {
                return Err(HeapError::Invariant(
                    "AsyncGenerator completion reached an object with the wrong class",
                ));
            };
            if matches!(
                data.state,
                AsyncGeneratorState::AwaitingReturn | AsyncGeneratorState::Completed
            ) {
                return Err(HeapError::Invariant(
                    "AsyncGenerator completion repeated or interrupted completed-return await",
                ));
            }
            data.state = AsyncGeneratorState::Completed;
            (data.activation.take(), data.resume_realm.take())
        };
        let mut atoms = Vec::new();
        if let Some(activation) = activation.as_deref() {
            for edge in generator_activation_edges(activation) {
                self.release_raw_no_drain(edge)?;
            }
            atoms.extend(generator_activation_atoms(activation));
        }
        if let Some(realm) = resume_realm {
            self.release_raw_no_drain(RawId::Context(realm))?;
        }
        let mut cleanup = self.drain_zero_queue()?;
        cleanup.atoms.extend(atoms);
        Ok(cleanup)
    }

    pub(crate) fn begin_async_generator_completed_return(
        &mut self,
        id: ObjectId,
        realm: ContextId,
    ) -> Result<(), HeapError> {
        self.context(realm)?;
        match &self.object(id)?.payload {
            ObjectPayload::AsyncGenerator(AsyncGeneratorData {
                state: AsyncGeneratorState::Completed,
                activation: None,
                resume_realm: None,
                queue,
            }) if !queue.is_empty() => {}
            ObjectPayload::AsyncGenerator(_) => {
                return Err(HeapError::Invariant(
                    "AsyncGenerator completed return began in an invalid state",
                ));
            }
            _ => {
                return Err(HeapError::Invariant(
                    "AsyncGenerator completed return reached the wrong class",
                ));
            }
        }
        self.retain_raw(RawId::Context(realm), 1)?;
        let ObjectPayload::AsyncGenerator(data) = &mut self.object_mut(id)?.payload else {
            unreachable!("AsyncGenerator payload was validated before completed return")
        };
        data.state = AsyncGeneratorState::AwaitingReturn;
        data.resume_realm = Some(realm);
        Ok(())
    }

    pub(crate) fn finish_async_generator_completed_return(
        &mut self,
        id: ObjectId,
    ) -> Result<HeapCleanup, HeapError> {
        let realm = {
            let ObjectPayload::AsyncGenerator(data) = &mut self.object_mut(id)?.payload else {
                return Err(HeapError::Invariant(
                    "AsyncGenerator completed-return callback reached the wrong class",
                ));
            };
            if data.state != AsyncGeneratorState::AwaitingReturn || data.activation.is_some() {
                return Err(HeapError::Invariant(
                    "AsyncGenerator completed-return callback reached an invalid state",
                ));
            }
            data.state = AsyncGeneratorState::Completed;
            data.resume_realm.take().ok_or(HeapError::Invariant(
                "AsyncGenerator completed-return callback lost its realm",
            ))?
        };
        self.release_raw_no_drain(RawId::Context(realm))?;
        self.drain_zero_queue()
    }

    /// Clone one async driver state while its hidden object still owns every
    /// referenced edge. The runtime must promote any raw identities it keeps
    /// across a subsequent heap mutation.
    pub(crate) fn async_function_state_snapshot(
        &self,
        id: ObjectId,
    ) -> Result<AsyncFunctionStateData, HeapError> {
        let ObjectPayload::AsyncFunctionState(data) = &self.object(id)?.payload else {
            return Err(HeapError::Invariant(
                "AsyncFunction state requested for an object with the wrong class",
            ));
        };
        Ok(data.clone())
    }

    /// Transfer a newly suspended async VM frame into the hidden driver.
    ///
    /// Atom occurrences are pre-owned by the runtime. Arena edges are retained
    /// transactionally before the phase becomes observable as `Awaiting`.
    pub(crate) fn suspend_async_function(
        &mut self,
        id: ObjectId,
        activation: GeneratorActivationData,
    ) -> Result<(), HeapError> {
        match &self.object(id)?.payload {
            ObjectPayload::AsyncFunctionState(AsyncFunctionStateData {
                phase: AsyncFunctionPhase::Executing,
                activation: None,
                ..
            }) => {}
            ObjectPayload::AsyncFunctionState(_) => {
                return Err(HeapError::Invariant(
                    "AsyncFunction suspension did not follow an executing phase",
                ));
            }
            _ => {
                return Err(HeapError::Invariant(
                    "AsyncFunction suspension reached an object with the wrong class",
                ));
            }
        }

        let mut candidate = self.object(id)?.clone();
        let ObjectPayload::AsyncFunctionState(candidate_data) = &mut candidate.payload else {
            unreachable!("AsyncFunction payload was validated before suspension")
        };
        candidate_data.phase = AsyncFunctionPhase::Awaiting;
        candidate_data.activation = Some(Box::new(activation.clone()));
        self.validate_object_layout(&candidate)?;

        let edges = generator_activation_edges(&activation);
        self.retain_edges_transactionally(&edges)?;
        let ObjectPayload::AsyncFunctionState(data) = &mut self.object_mut(id)?.payload else {
            unreachable!("AsyncFunction payload was validated before retaining its activation")
        };
        data.phase = AsyncFunctionPhase::Awaiting;
        data.activation = Some(Box::new(activation));
        Ok(())
    }

    /// Move an awaiting async driver back to `Executing` and detach its
    /// heap-owned activation. The caller must first root the snapshot it will
    /// execute so releasing the dormant occurrences remains safe.
    pub(crate) fn begin_async_function_resume(
        &mut self,
        id: ObjectId,
    ) -> Result<(GeneratorActivationData, HeapCleanup), HeapError> {
        let activation = {
            let ObjectPayload::AsyncFunctionState(data) = &mut self.object_mut(id)?.payload else {
                return Err(HeapError::Invariant(
                    "AsyncFunction resume reached an object with the wrong class",
                ));
            };
            if data.phase != AsyncFunctionPhase::Awaiting {
                return Err(HeapError::Invariant(
                    "AsyncFunction resume began outside an awaiting phase",
                ));
            }
            let activation = data.activation.take().ok_or(HeapError::Invariant(
                "awaiting AsyncFunction has no activation",
            ))?;
            data.phase = AsyncFunctionPhase::Executing;
            *activation
        };

        let edges = generator_activation_edges(&activation);
        let atoms = generator_activation_atoms(&activation);
        for edge in edges {
            self.release_raw_no_drain(edge)?;
        }
        let mut cleanup = self.drain_zero_queue()?;
        cleanup.atoms.extend(atoms);
        Ok((activation, cleanup))
    }

    /// Permanently finish an async driver after resolving or rejecting its
    /// outer promise. Failure after an `await` has been published may complete
    /// directly from `Awaiting`; in that case dormant frame ownership is
    /// detached and returned through the ordinary cleanup channel.
    pub(crate) fn complete_async_function(
        &mut self,
        id: ObjectId,
    ) -> Result<HeapCleanup, HeapError> {
        let activation = {
            let ObjectPayload::AsyncFunctionState(data) = &mut self.object_mut(id)?.payload else {
                return Err(HeapError::Invariant(
                    "AsyncFunction completion reached an object with the wrong class",
                ));
            };
            let activation = match data.phase {
                AsyncFunctionPhase::Executing if data.activation.is_none() => None,
                AsyncFunctionPhase::Awaiting if data.activation.is_some() => {
                    let Some(activation) = data.activation.take() else {
                        unreachable!("activation presence was checked")
                    };
                    Some(*activation)
                }
                AsyncFunctionPhase::Executing
                | AsyncFunctionPhase::Awaiting
                | AsyncFunctionPhase::Completed => {
                    return Err(HeapError::Invariant(
                        "AsyncFunction completion reached an inconsistent phase",
                    ));
                }
            };
            data.phase = AsyncFunctionPhase::Completed;
            activation
        };

        let Some(activation) = activation else {
            return Ok(HeapCleanup::default());
        };
        let edges = generator_activation_edges(&activation);
        let atoms = generator_activation_atoms(&activation);
        for edge in edges {
            self.release_raw_no_drain(edge)?;
        }
        let mut cleanup = self.drain_zero_queue()?;
        cleanup.atoms.extend(atoms);
        Ok(cleanup)
    }

    /// Advance within one snapshotted level without cloning the complete key
    /// vector or visited set. Non-enumerable and duplicate prototype keys are
    /// consumed internally because neither can be yielded.
    pub fn next_for_in_candidate(&mut self, id: ObjectId) -> Result<ForInCandidate, HeapError> {
        let ObjectPayload::ForInIterator(data) = &mut self.object_mut(id)?.payload else {
            return Err(HeapError::Invariant(
                "for-in advance reached an object with the wrong class",
            ));
        };
        loop {
            let Some(object) = data.object else {
                return Ok(ForInCandidate::Done);
            };
            if data.fast_array {
                let index = u32::try_from(data.index)
                    .map_err(|_| HeapError::Invariant("for-in fast Array index exceeded Uint32"))?;
                if index < data.array_count {
                    data.index += 1;
                    return Ok(ForInCandidate::ArrayIndex { object, index });
                }
                return Ok(ForInCandidate::BaseComplete {
                    object,
                    fast_array: true,
                });
            }
            if data.index >= data.properties.len() {
                if !data.in_prototype_chain {
                    return Ok(ForInCandidate::BaseComplete {
                        object,
                        fast_array: false,
                    });
                }
                return Ok(ForInCandidate::LevelComplete(object));
            }

            let entry = data.properties[data.index].clone();
            data.index += 1;
            if data.in_prototype_chain && !data.visited.insert(entry.name.clone()) {
                continue;
            }
            if !entry.enumerable {
                continue;
            }
            let object = data.object.ok_or(HeapError::Invariant(
                "for-in property snapshot lost its current object",
            ))?;
            return Ok(ForInCandidate::Property {
                object,
                name: entry.name,
            });
        }
    }

    /// Complete QuickJS's one-time prototype-chain preparation. A generic
    /// iterator records its original base snapshot; a fast Array records a
    /// fresh own-key snapshot supplied after the prototype pre-scan.
    pub fn enter_for_in_prototype_chain(
        &mut self,
        id: ObjectId,
        refreshed_fast_properties: Option<Vec<ForInProperty>>,
    ) -> Result<(), HeapError> {
        let ObjectPayload::ForInIterator(data) = &mut self.object_mut(id)?.payload else {
            return Err(HeapError::Invariant(
                "for-in prototype preparation reached an object with the wrong class",
            ));
        };
        if data.in_prototype_chain {
            return Err(HeapError::Invariant(
                "for-in prototype chain was prepared more than once",
            ));
        }
        let properties = refreshed_fast_properties
            .as_ref()
            .unwrap_or(&data.properties);
        data.visited
            .extend(properties.iter().map(|entry| entry.name.clone()));
        data.in_prototype_chain = true;
        Ok(())
    }

    /// Install the next prototype level transactionally. The new current edge
    /// is retained before the old level is detached; `None` marks exhaustion.
    pub fn replace_for_in_level(
        &mut self,
        id: ObjectId,
        next_object: Option<ObjectId>,
        properties: Vec<ForInProperty>,
    ) -> Result<HeapCleanup, HeapError> {
        if !matches!(self.object(id)?.payload, ObjectPayload::ForInIterator(_)) {
            return Err(HeapError::Invariant(
                "for-in level update reached an object with the wrong class",
            ));
        }
        if let Some(object) = next_object {
            self.retain_raw(RawId::Object(object), 1)?;
        }
        let previous = {
            let ObjectPayload::ForInIterator(data) = &mut self.object_mut(id)?.payload else {
                unreachable!("for-in payload was validated before level replacement")
            };
            data.index = 0;
            data.properties = properties;
            data.fast_array = false;
            data.array_count = 0;
            std::mem::replace(&mut data.object, next_object)
        };
        if let Some(object) = previous {
            self.release_raw_no_drain(RawId::Object(object))?;
        }
        self.drain_zero_queue()
    }

    /// Update the ordinary object's extensibility bit without changing its
    /// shape or property payloads.
    pub fn set_object_extensible(
        &mut self,
        id: ObjectId,
        extensible: bool,
    ) -> Result<(), HeapError> {
        self.object_mut(id)?.extensible = extensible;
        Ok(())
    }

    /// Permanently lock the object's prototype, matching QuickJS's
    /// immutable-prototype flag used by selected intrinsics.
    pub fn set_immutable_prototype(&mut self, id: ObjectId) -> Result<(), HeapError> {
        self.object_mut(id)?.immutable_prototype = true;
        Ok(())
    }

    /// Transactionally replace one property payload.
    ///
    /// New edges are retained before the old payload is detached.  Releasing
    /// the old payload can reclaim an unrooted receiver, so callers must treat
    /// `id` as potentially stale after this operation unless they hold a root.
    pub fn replace_object_slot(
        &mut self,
        id: ObjectId,
        slot_index: usize,
        replacement: PropertySlot,
    ) -> Result<HeapCleanup, HeapError> {
        self.validate_replacement_slot(id, slot_index, &replacement)?;
        let new_edges = property_slot_edges(&replacement);
        self.retain_edges_transactionally(&new_edges)?;

        let previous = {
            let object = self.object_mut(id)?;
            let slot = object
                .slots
                .get_mut(slot_index)
                .ok_or(HeapError::Invariant(
                    "validated property slot disappeared before replacement",
                ))?;
            std::mem::replace(slot, replacement)
        };

        let mut cleanup = HeapCleanup::default();
        cleanup.atoms.extend(property_slot_atoms(&previous));
        for edge in property_slot_edges(&previous) {
            self.release_raw_no_drain(edge)?;
        }
        cleanup.merge(self.drain_zero_queue()?);
        Ok(cleanup)
    }

    /// Append one property to an object whose shape has exactly one owner.
    ///
    /// New slot edges are retained before the parallel shape and slot vectors
    /// are mutated. Atom ownership is managed by the enclosing runtime: one
    /// live reference for `atom` and any Symbol slot has to be transferred
    /// before this call succeeds.
    pub fn append_unique_object_property(
        &mut self,
        id: ObjectId,
        atom: Atom,
        flags: PropertyFlags,
        replacement: PropertySlot,
    ) -> Result<(), HeapError> {
        let (shape_id, slot_count) = {
            let object = self.object(id)?;
            (object.shape, object.slots.len())
        };
        if self.shape_strong_count(shape_id)? != 1 {
            return Err(HeapError::Invariant(
                "in-place property append reached a shared shape",
            ));
        }
        let index =
            self.shape(shape_id)?
                .unique_append_index(atom)
                .map_err(|error| match error {
                    ShapeError::NullAtom => {
                        HeapError::Invariant("in-place property append used a null atom")
                    }
                    ShapeError::DuplicateAtom(_) => {
                        HeapError::Invariant("in-place property append duplicated a shape atom")
                    }
                    ShapeError::MissingAtom(_) => HeapError::Invariant(
                        "in-place property append reported an impossible missing atom",
                    ),
                    ShapeError::PropertyIndexOverflow => HeapError::Overflow {
                        operation: "appending an in-place shape property",
                    },
                })?;
        if usize::try_from(index) != Ok(slot_count) {
            return Err(HeapError::Invariant(
                "in-place property append found mismatched shape and slot lengths",
            ));
        }
        if !slot_matches_storage(&replacement, flags.storage) {
            return Err(HeapError::Invariant(
                "appended property storage does not match its shape flags",
            ));
        }
        if matches!(replacement, PropertySlot::Data(RawValue::Private(_))) {
            return Err(HeapError::Invariant(
                "private-name identity escaped into an appended object value slot",
            ));
        }

        self.retain_edges_transactionally(&property_slot_edges(&replacement))?;
        let shape = match self.shape_mut(shape_id) {
            Ok(shape) => shape,
            Err(_) => unreachable!("authenticated unique shape disappeared before append"),
        };
        shape.append_unique_property(atom, flags, index);
        let object = match self.object_mut(id) {
            Ok(object) => object,
            Err(_) => unreachable!("authenticated object disappeared before slot append"),
        };
        object.slots.push(replacement);
        debug_assert!(
            self.object(id)
                .and_then(|object| self.validate_object_layout(object))
                .is_ok()
        );
        Ok(())
    }

    /// Transactionally replace a bytecode function's optional HomeObject.
    ///
    /// The replacement is retained before the previous edge is detached, so
    /// changing from an object which owns the replacement cannot make the new
    /// handle stale mid-operation. Identical `Some` values and `None -> None`
    /// are no-ops and therefore cannot overflow or perturb reference counts.
    /// Releasing the old edge may reclaim an unrooted receiver; callers must
    /// keep `id` rooted if they need to use it after this operation.
    pub fn replace_bytecode_function_home_object(
        &mut self,
        id: ObjectId,
        replacement: Option<ObjectId>,
    ) -> Result<HeapCleanup, HeapError> {
        let previous = self.bytecode_function_home_object(id)?;
        if previous == replacement {
            return Ok(HeapCleanup::default());
        }
        if let Some(home_object) = replacement {
            self.retain_raw(RawId::Object(home_object), 1)?;
        }

        let ObjectPayload::BytecodeFunction { home_object, .. } = &mut self.object_mut(id)?.payload
        else {
            unreachable!("bytecode-function payload was validated before HomeObject replacement")
        };
        *home_object = replacement;

        if let Some(home_object) = previous {
            self.release_raw_no_drain(RawId::Object(home_object))?;
        }
        self.drain_zero_queue()
    }

    /// Atomically attach a fresh instance-field initializer to one class.
    ///
    /// The constructor-to-initializer and initializer-to-prototype edges are a
    /// single publication transaction.  Neither edge can be replaced: these
    /// are compiler-owned capabilities, not mutable JavaScript state.
    pub fn attach_bytecode_class_instance_initializer(
        &mut self,
        constructor: ObjectId,
        prototype: ObjectId,
        initializer: ObjectId,
    ) -> Result<(), HeapError> {
        if constructor == prototype || constructor == initializer || prototype == initializer {
            return Err(HeapError::Invariant(
                "class initializer publication reused an object identity",
            ));
        }
        self.object(prototype)?;
        let constructor_object = self.object(constructor)?;
        let ObjectPayload::BytecodeFunction {
            bytecode: constructor_bytecode,
            class_instance_initializer: existing_initializer,
            ..
        } = &constructor_object.payload
        else {
            return Err(HeapError::Invariant(
                "class initializer owner is not a bytecode function",
            ));
        };
        let constructor_metadata = self.function_bytecode(*constructor_bytecode)?;
        if !constructor_object.is_constructor
            || constructor_metadata.metadata.constructor_kind == ConstructorKind::None
            || constructor_metadata.metadata.has_prototype
            || !constructor_metadata.metadata.strict
            || constructor_metadata
                .metadata
                .class_initializer_kind
                .is_some()
            || existing_initializer.is_some()
        {
            return Err(HeapError::Invariant(
                "class initializer owner is not a fresh class constructor",
            ));
        }
        let constructor_realm = constructor_metadata.realm;

        let initializer_object = self.object(initializer)?;
        let ObjectPayload::BytecodeFunction {
            bytecode: initializer_bytecode,
            home_object,
            class_instance_initializer,
            ..
        } = &initializer_object.payload
        else {
            return Err(HeapError::Invariant(
                "class instance initializer is not a bytecode function",
            ));
        };
        let initializer_bytecode = self.function_bytecode(*initializer_bytecode)?;
        if initializer_object.is_constructor
            || home_object.is_some()
            || class_instance_initializer.is_some()
            || initializer_bytecode.realm != constructor_realm
            || initializer_bytecode.metadata.class_initializer_kind
                != Some(ClassInitializerKind::InstanceFields)
            || !initializer_bytecode.metadata.needs_home_object
        {
            return Err(HeapError::Invariant(
                "class instance initializer is not fresh or has the wrong owner realm",
            ));
        }

        self.retain_edges_transactionally(&[RawId::Object(prototype), RawId::Object(initializer)])?;
        let ObjectPayload::BytecodeFunction { home_object, .. } =
            &mut self.object_mut(initializer)?.payload
        else {
            unreachable!("initializer payload was authenticated before edge publication")
        };
        *home_object = Some(prototype);
        let ObjectPayload::BytecodeFunction {
            class_instance_initializer,
            ..
        } = &mut self.object_mut(constructor)?.payload
        else {
            unreachable!("constructor payload was authenticated before edge publication")
        };
        *class_instance_initializer = Some(initializer);
        Ok(())
    }

    /// Claim the one permitted aggregate static-initializer execution for a
    /// class constructor. The claim is deliberately not rolled back after an
    /// abrupt initializer: a leaked constructor must never replay fields or
    /// static blocks through forged privileged bytecode.
    pub fn begin_bytecode_class_static_initializer(
        &mut self,
        constructor: ObjectId,
    ) -> Result<(), HeapError> {
        {
            let constructor_object = self.object(constructor)?;
            let ObjectPayload::BytecodeFunction {
                bytecode,
                class_static_initializer_started,
                ..
            } = &constructor_object.payload
            else {
                return Err(HeapError::Invariant(
                    "class static initializer owner is not a bytecode function",
                ));
            };
            let metadata = self.function_bytecode(*bytecode)?.metadata;
            if !constructor_object.is_constructor
                || metadata.constructor_kind == ConstructorKind::None
                || metadata.has_prototype
                || !metadata.strict
                || metadata.class_initializer_kind.is_some()
            {
                return Err(HeapError::Invariant(
                    "class static initializer owner is not a class constructor",
                ));
            }
            if *class_static_initializer_started {
                return Err(HeapError::Invariant(
                    "class static initializer was already started",
                ));
            }
        }

        let ObjectPayload::BytecodeFunction {
            class_static_initializer_started,
            ..
        } = &mut self.object_mut(constructor)?.payload
        else {
            unreachable!("static initializer owner was authenticated before its one-shot claim")
        };
        *class_static_initializer_started = true;
        Ok(())
    }

    /// Transactionally replace an object's complete shape/slot layout.
    ///
    /// This is the low-level primitive used by immutable shape transitions.
    /// The caller must already own atom references for symbol values in
    /// `slots`; on success those references transfer to the heap.  The returned
    /// cleanup contains every symbol atom detached from the previous slots.
    pub fn replace_object_layout(
        &mut self,
        id: ObjectId,
        shape: ShapeId,
        slots: Vec<PropertySlot>,
    ) -> Result<HeapCleanup, HeapError> {
        self.validate_property_layout(shape, &slots)?;
        let replacement_prototype = self.shape(shape)?.prototype();
        if matches!(self.object(id)?.payload, ObjectPayload::Proxy(_))
            && replacement_prototype.is_some()
        {
            return Err(HeapError::Invariant(
                "Proxy has invalid null-prototype layout or cached target capabilities",
            ));
        }

        // The class payload, private brand, and capability bits are unchanged.
        // Retaining and releasing only the replacement layout edges keeps that
        // payload in place instead of cloning potentially large non-GC state
        // such as an ArrayBuffer backing store.
        let new_edges = object_layout_edges(shape, &slots);
        self.retain_edges_transactionally(&new_edges)?;

        let (previous_shape, previous_slots) = {
            let object = self
                .object_mut(id)
                .expect("authenticated object disappeared during layout replacement");
            (
                std::mem::replace(&mut object.shape, shape),
                std::mem::replace(&mut object.slots, slots),
            )
        };

        let mut cleanup = HeapCleanup::default();
        cleanup
            .atoms
            .extend(previous_slots.iter().flat_map(property_slot_atoms));
        for edge in object_layout_edges(previous_shape, &previous_slots) {
            self.release_raw_no_drain(edge)?;
        }
        cleanup.merge(self.drain_zero_queue()?);
        Ok(cleanup)
    }

    /// Atomically materialize a fast Array's dense prefix into the indexed
    /// suffix of a prepared shape. Existing slots and dense values move in
    /// place, preserving their edge and Symbol-atom ownership without cloning
    /// or temporarily retaining a second copy of the payload.
    pub fn materialize_array_dense_shape(
        &mut self,
        id: ObjectId,
        shape: ShapeId,
    ) -> Result<HeapCleanup, HeapError> {
        let (previous_shape, previous_slot_len, dense_len) = {
            let object = self.object(id)?;
            let ObjectPayload::Array { dense: Some(dense) } = &object.payload else {
                return Err(HeapError::Invariant(
                    "dense materialization reached a slow Array or wrong object class",
                ));
            };
            (object.shape, object.slots.len(), dense.len())
        };
        let previous = self.shape(previous_shape)?;
        let replacement = self.shape(shape)?;
        let replacement_len =
            previous_slot_len
                .checked_add(dense_len)
                .ok_or(HeapError::Overflow {
                    operation: "materializing fast Array slots",
                })?;
        if replacement.prototype() != previous.prototype()
            || replacement.entries().len() != replacement_len
            || replacement.entries().get(..previous_slot_len) != Some(previous.entries())
            || replacement.entries()[previous_slot_len..]
                .iter()
                .any(|entry| entry.flags != PropertyFlags::data(true, true, true))
        {
            return Err(HeapError::Invariant(
                "materialized Array shape does not extend its dense layout",
            ));
        }
        self.object_mut(id)?
            .slots
            .try_reserve(dense_len)
            .map_err(|_| HeapError::Allocation {
                operation: "materializing fast Array slots",
            })?;
        self.retain_shape(shape)?;

        let detached_shape = {
            let object = self
                .object_mut(id)
                .expect("authenticated Array disappeared during dense materialization");
            let ObjectPayload::Array { dense } = &mut object.payload else {
                unreachable!("Array changed class during dense materialization")
            };
            let previous_dense = dense
                .take()
                .expect("fast Array changed representation during materialization");
            object
                .slots
                .extend(previous_dense.into_iter().map(PropertySlot::Data));
            std::mem::replace(&mut object.shape, shape)
        };
        self.release_and_drain(RawId::Shape(detached_shape))
    }

    /// Change only the object's `[[Construct]]` capability bit.
    /// QuickJS keeps this bit independent from the native cproto used to
    /// initialize it, so changing it must not rewrite callable metadata.
    pub(crate) fn set_object_constructor_bit(
        &mut self,
        id: ObjectId,
        enabled: bool,
    ) -> Result<(), HeapError> {
        self.object_mut(id)?.is_constructor = enabled;
        Ok(())
    }
    /// Strong count for diagnostics.  A zombie remains queryable until all
    /// candidate incoming edges have been detached.
    pub fn object_strong_count(&self, id: ObjectId) -> Result<u32, HeapError> {
        self.strong_count(RawId::Object(id))
    }

    /// Strong count for diagnostics.
    pub fn shape_strong_count(&self, id: ShapeId) -> Result<u32, HeapError> {
        self.strong_count(RawId::Shape(id))
    }

    /// Strong count for captured-variable diagnostics.
    pub fn var_ref_strong_count(&self, id: VarRefId) -> Result<u32, HeapError> {
        self.strong_count(RawId::VarRef(id))
    }

    /// Strong count for context diagnostics.
    pub fn context_strong_count(&self, id: ContextId) -> Result<u32, HeapError> {
        self.strong_count(RawId::Context(id))
    }

    /// Strong count for function-bytecode diagnostics.
    pub fn function_bytecode_strong_count(&self, id: FunctionBytecodeId) -> Result<u32, HeapError> {
        self.strong_count(RawId::FunctionBytecode(id))
    }

    /// Return the lifecycle state at one physical slot, if it exists.
    #[must_use]
    pub fn slot_state(&self, debug_index: u32) -> Option<HeapSlotState> {
        self.slots
            .get(debug_index as usize)
            .map(|slot| slot.state.public_state())
    }

    /// Snapshot aggregate arena counts for tests and runtime diagnostics.
    #[must_use]
    pub fn counts(&self) -> HeapCounts {
        let mut counts = HeapCounts::default();
        for slot in &self.slots {
            match &slot.state {
                SlotState::Initializing { kind, .. } => {
                    counts.initializing = counts.initializing.saturating_add(1);
                    increment_kind_count(&mut counts, *kind);
                }
                SlotState::Live(node) => {
                    counts.live = counts.live.saturating_add(1);
                    increment_kind_count(&mut counts, node.data.kind());
                }
                SlotState::ZeroQueued(node) => {
                    counts.zero_queued = counts.zero_queued.saturating_add(1);
                    increment_kind_count(&mut counts, node.data.kind());
                }
                SlotState::Finalizing(node) => {
                    counts.finalizing = counts.finalizing.saturating_add(1);
                    increment_kind_count(&mut counts, node.data.kind());
                }
                SlotState::Zombie { kind, .. } => {
                    counts.zombies = counts.zombies.saturating_add(1);
                    increment_kind_count(&mut counts, *kind);
                }
                SlotState::Vacant => counts.vacant = counts.vacant.saturating_add(1),
                SlotState::Retired => counts.retired = counts.retired.saturating_add(1),
            }
        }
        counts
    }

    fn reserve(&mut self, kind: HeapNodeKind) -> Result<(u32, u32), HeapError> {
        let index = if let Some(index) = self.free.pop() {
            index
        } else {
            let index = u32::try_from(self.slots.len()).map_err(|_| HeapError::Overflow {
                operation: "allocating an arena slot",
            })?;
            self.slots.push(ArenaSlot {
                generation: 1,
                state: SlotState::Vacant,
                weak_prev: None,
                weak_next: None,
            });
            index
        };
        let slot = self
            .slots
            .get_mut(index as usize)
            .ok_or(HeapError::Invariant("free list referenced a missing slot"))?;
        if !matches!(slot.state, SlotState::Vacant) {
            return Err(HeapError::Invariant(
                "free list referenced an occupied slot",
            ));
        }
        if slot.weak_prev.is_some() || slot.weak_next.is_some() {
            return Err(HeapError::Invariant(
                "free list referenced a linked weak-collection slot",
            ));
        }
        slot.state = SlotState::Initializing { kind, strong: 1 };
        Ok((index, slot.generation))
    }

    fn abort_initializing(&mut self, index: u32) -> Result<(), HeapError> {
        let slot = self
            .slots
            .get_mut(index as usize)
            .ok_or(HeapError::Invariant("initializing slot disappeared"))?;
        if !matches!(slot.state, SlotState::Initializing { .. }) {
            return Err(HeapError::Invariant(
                "attempted to abort a published arena slot",
            ));
        }
        slot.state = SlotState::Vacant;
        self.free.push(index);
        Ok(())
    }

    fn publish(&mut self, index: u32, data: NodeData) -> Result<(), HeapError> {
        let expected = data.kind();
        let slot = self
            .slots
            .get_mut(index as usize)
            .ok_or(HeapError::Invariant("initializing slot disappeared"))?;
        let (kind, strong) = match &slot.state {
            SlotState::Initializing { kind, strong } => (*kind, *strong),
            _ => {
                return Err(HeapError::Invariant(
                    "attempted to publish a non-initializing slot",
                ));
            }
        };
        if kind != expected || strong != 1 {
            return Err(HeapError::Invariant(
                "initializing slot metadata did not match its payload",
            ));
        }
        slot.state = SlotState::Live(Node { strong, data });
        Ok(())
    }
    fn validate_property_layout(
        &self,
        shape: ShapeId,
        slots: &[PropertySlot],
    ) -> Result<(), HeapError> {
        let shape = self.shape(shape)?;
        if shape.entries().len() != slots.len() {
            return Err(HeapError::Invariant(
                "object slot count does not match its shape",
            ));
        }
        for (entry, slot) in shape.entries().iter().zip(slots) {
            if !slot_matches_storage(slot, entry.flags.storage) {
                return Err(HeapError::Invariant(
                    "object property storage does not match its shape flags",
                ));
            }
            if matches!(slot, PropertySlot::Data(RawValue::Private(_))) {
                return Err(HeapError::Invariant(
                    "private-name identity escaped into an object value slot",
                ));
            }
        }
        Ok(())
    }

    fn validate_object_layout(&self, object: &ObjectData) -> Result<(), HeapError> {
        if !matches!(
            (object.kind, &object.payload),
            (
                ObjectKind::Ordinary,
                ObjectPayload::Ordinary | ObjectPayload::RawJson
            ) | (ObjectKind::ModuleNamespace, ObjectPayload::Ordinary)
                | (ObjectKind::Iterator, ObjectPayload::Ordinary)
                | (ObjectKind::Array, ObjectPayload::Array { .. })
                | (ObjectKind::Arguments, ObjectPayload::Arguments { .. })
                | (
                    ObjectKind::ArrayIterator,
                    ObjectPayload::ArrayIterator { .. }
                )
                | (ObjectKind::ForInIterator, ObjectPayload::ForInIterator(_))
                | (ObjectKind::Primitive, ObjectPayload::Primitive(_))
                | (ObjectKind::Date, ObjectPayload::Date(_))
                | (ObjectKind::RegExp, ObjectPayload::RegExp(_))
                | (
                    ObjectKind::RegExpStringIterator,
                    ObjectPayload::RegExpStringIterator { .. }
                )
                | (ObjectKind::Map, ObjectPayload::Map { .. })
                | (ObjectKind::MapIterator, ObjectPayload::MapIterator { .. })
                | (ObjectKind::Set, ObjectPayload::Set { .. })
                | (ObjectKind::SetIterator, ObjectPayload::SetIterator { .. })
                | (ObjectKind::WeakMap, ObjectPayload::WeakMap { .. })
                | (ObjectKind::WeakSet, ObjectPayload::WeakSet { .. })
                | (ObjectKind::WeakRef, ObjectPayload::WeakRef { .. })
                | (
                    ObjectKind::FinalizationRegistry,
                    ObjectPayload::FinalizationRegistry(_)
                )
                | (ObjectKind::GlobalObject, ObjectPayload::GlobalObject { .. })
                | (ObjectKind::Error, ObjectPayload::Error)
                | (
                    ObjectKind::StringIterator,
                    ObjectPayload::StringIterator { .. }
                )
                | (ObjectKind::IteratorHelper, ObjectPayload::IteratorHelper(_))
                | (ObjectKind::IteratorWrap, ObjectPayload::IteratorWrap(_))
                | (
                    ObjectKind::AsyncFromSyncIterator,
                    ObjectPayload::AsyncFromSyncIterator(_)
                )
                | (ObjectKind::IteratorConcat, ObjectPayload::IteratorConcat(_))
                | (ObjectKind::Proxy, ObjectPayload::Proxy(_))
                | (ObjectKind::ArrayBuffer, ObjectPayload::ArrayBuffer(_))
                | (
                    ObjectKind::SharedArrayBuffer,
                    ObjectPayload::SharedArrayBuffer(_)
                )
                | (ObjectKind::DataView, ObjectPayload::DataView(_))
                | (ObjectKind::TypedArray, ObjectPayload::TypedArray(_))
                | (
                    ObjectKind::NativeFunction,
                    ObjectPayload::NativeFunction { .. }
                )
                | (
                    ObjectKind::BoundFunction,
                    ObjectPayload::BoundFunction { .. }
                )
                | (
                    ObjectKind::BytecodeFunction,
                    ObjectPayload::BytecodeFunction { .. }
                )
                | (ObjectKind::Generator, ObjectPayload::Generator { .. })
                | (ObjectKind::AsyncGenerator, ObjectPayload::AsyncGenerator(_))
                | (
                    ObjectKind::AsyncFunctionState,
                    ObjectPayload::AsyncFunctionState(_)
                )
                | (ObjectKind::Promise, ObjectPayload::Promise(_))
        ) {
            return Err(HeapError::Invariant(
                "object kind does not match its class payload",
            ));
        }
        self.validate_property_layout(object.shape, &object.slots)?;
        let shape = self.shape(object.shape)?;
        if let ObjectPayload::Array { dense: Some(dense) } = &object.payload
            && (u32::try_from(dense.len()).is_err()
                || dense.iter().any(|value| !is_map_storable_value(value)))
        {
            return Err(HeapError::Invariant(
                "fast Array contains an invalid dense value prefix",
            ));
        }
        if let ObjectPayload::Proxy(data) = &object.payload {
            let target = self.object(data.target)?;
            self.object(data.handler)?;
            let target_is_callable = object_data_is_callable(target);
            // The ordinary prototype is always null and public operations are
            // exotic, but class initialization may attach private elements to
            // the Proxy object itself. Those private slots therefore remain
            // valid physical shape entries.
            if shape.prototype().is_some()
                || !object.extensible
                || object.immutable_prototype
                || data.is_callable != target_is_callable
                || object.is_constructor != target.is_constructor
            {
                return Err(HeapError::Invariant(
                    "Proxy has invalid null-prototype layout or cached target capabilities",
                ));
            }
        }
        if let ObjectPayload::ArrayBuffer(data) = &object.payload {
            let byte_length = u32::try_from(data.bytes.len()).map_err(|_| {
                HeapError::Invariant("ArrayBuffer byte length exceeds the supported range")
            })?;
            if object.is_constructor
                || (data.detached && byte_length != 0)
                || data
                    .max_byte_length
                    .is_some_and(|maximum| maximum < byte_length)
                || byte_length > i32::MAX as u32
                || data
                    .max_byte_length
                    .is_some_and(|maximum| maximum > i32::MAX as u32)
            {
                return Err(HeapError::Invariant(
                    "ArrayBuffer has invalid backing-store state",
                ));
            }
        }
        if let ObjectPayload::SharedArrayBuffer(data) = &object.payload {
            let byte_length = data.handle.byte_length();
            let maximum = data.handle.max_byte_length_option();
            if object.is_constructor
                || byte_length > i32::MAX as u32
                || maximum.is_some_and(|maximum| maximum < byte_length || maximum > i32::MAX as u32)
                || data.handle.backing_capacity() != data.handle.max_byte_length()
            {
                return Err(HeapError::Invariant(
                    "SharedArrayBuffer has invalid wrapper or backing-store state",
                ));
            }
        }
        if let ObjectPayload::DataView(data) = &object.payload {
            let (maximum, growable) = match &self.object(data.buffer)?.payload {
                ObjectPayload::ArrayBuffer(buffer) => {
                    (buffer.max_byte_length, buffer.max_byte_length.is_some())
                }
                ObjectPayload::SharedArrayBuffer(buffer) => (
                    buffer.handle.max_byte_length_option(),
                    buffer.handle.is_growable(),
                ),
                _ => {
                    return Err(HeapError::Invariant(
                        "DataView backing object is not an ArrayBuffer or SharedArrayBuffer",
                    ));
                }
            };
            let structural_end = data
                .fixed_byte_length
                .map(|byte_length| u64::from(data.byte_offset) + u64::from(byte_length));
            if object.is_constructor
                || data.byte_offset > i32::MAX as u32
                || data
                    .fixed_byte_length
                    .is_some_and(|byte_length| byte_length > i32::MAX as u32)
                || structural_end.is_some_and(|byte_end| byte_end > i32::MAX as u64)
                || (data.fixed_byte_length.is_none() && !growable)
                || maximum.is_some_and(|maximum| {
                    data.byte_offset > maximum
                        || structural_end.is_some_and(|byte_end| byte_end > u64::from(maximum))
                })
            {
                return Err(HeapError::Invariant(
                    "DataView has an invalid structural view layout",
                ));
            }
        }
        if let ObjectPayload::TypedArray(data) = &object.payload {
            let view = data.view;
            let (maximum, growable) = match &self.object(view.buffer)?.payload {
                ObjectPayload::ArrayBuffer(buffer) => {
                    (buffer.max_byte_length, buffer.max_byte_length.is_some())
                }
                ObjectPayload::SharedArrayBuffer(buffer) => (
                    buffer.handle.max_byte_length_option(),
                    buffer.handle.is_growable(),
                ),
                _ => {
                    return Err(HeapError::Invariant(
                        "TypedArray backing object is not an ArrayBuffer or SharedArrayBuffer",
                    ));
                }
            };
            let width = u32::from(data.element.byte_length());
            let structural_end = view
                .fixed_byte_length
                .map(|byte_length| u64::from(view.byte_offset) + u64::from(byte_length));
            if object.is_constructor
                || view.byte_offset > i32::MAX as u32
                || view.byte_offset % width != 0
                || view.fixed_byte_length.is_some_and(|byte_length| {
                    byte_length > i32::MAX as u32 || byte_length % width != 0
                })
                || structural_end.is_some_and(|byte_end| byte_end > i32::MAX as u64)
                || (view.fixed_byte_length.is_none() && !growable)
                || maximum.is_some_and(|maximum| {
                    view.byte_offset > maximum
                        || structural_end.is_some_and(|byte_end| byte_end > u64::from(maximum))
                })
            {
                return Err(HeapError::Invariant(
                    "TypedArray has an invalid structural view layout",
                ));
            }
        }
        if let ObjectPayload::NativeFunction { data, internal } = &object.payload {
            match (data.target, internal) {
                (
                    NativeFunctionId::ProxyRevoke,
                    Some(InternalCallableData::ProxyRevoke { proxy }),
                ) => {
                    let proxy_is_valid = proxy
                        .map(|proxy| {
                            self.object(proxy)
                                .map(|proxy| matches!(&proxy.payload, ObjectPayload::Proxy(_)))
                        })
                        .transpose()?
                        .unwrap_or(true);
                    if object.is_constructor || !proxy_is_valid {
                        return Err(HeapError::Invariant(
                            "Proxy revoke callable has invalid hidden state",
                        ));
                    }
                }
                (
                    NativeFunctionId::AsyncFunctionResume(target_kind),
                    Some(InternalCallableData::AsyncFunctionResume { state, kind }),
                ) if target_kind == *kind => {
                    let ObjectPayload::AsyncFunctionState(state_data) =
                        &self.object(*state)?.payload
                    else {
                        return Err(HeapError::Invariant(
                            "AsyncFunction resume callable has invalid hidden state",
                        ));
                    };
                    if object.is_constructor || data.realm != Some(state_data.driver_realm) {
                        return Err(HeapError::Invariant(
                            "AsyncFunction resume callable has invalid hidden state",
                        ));
                    }
                }
                (
                    NativeFunctionId::DynamicImportHandler(target_kind),
                    Some(InternalCallableData::DynamicImportHandler {
                        module,
                        resolve,
                        reject,
                        kind,
                    }),
                ) if target_kind == *kind => {
                    let realm = data.realm.ok_or(HeapError::Invariant(
                        "dynamic-import handler has no defining realm",
                    ))?;
                    self.context(realm)?;
                    let cache = self.context(module.cache)?;
                    let resolve_is_callable = object_data_is_callable(self.object(*resolve)?);
                    let reject_is_callable = object_data_is_callable(self.object(*reject)?);
                    let module_is_ready = cache
                        .loaded_modules
                        .records
                        .get(module.module.0)
                        .and_then(Option::as_ref)
                        .is_some_and(|record| {
                            matches!(
                                &record.body,
                                RawModuleRecordBody::SourceText { .. }
                                    | RawModuleRecordBody::Json { .. }
                            )
                        });
                    if object.is_constructor
                        || !resolve_is_callable
                        || !reject_is_callable
                        || !module_is_ready
                    {
                        return Err(HeapError::Invariant(
                            "dynamic-import handler has invalid hidden state",
                        ));
                    }
                }
                (
                    NativeFunctionId::AsyncGeneratorResume(target_kind),
                    Some(InternalCallableData::AsyncGeneratorResume { generator, kind }),
                ) if target_kind == *kind => {
                    if object.is_constructor
                        || !matches!(
                            self.object(*generator)?.payload,
                            ObjectPayload::AsyncGenerator(_)
                        )
                    {
                        return Err(HeapError::Invariant(
                            "AsyncGenerator resume callable has invalid hidden state",
                        ));
                    }
                }
                (
                    NativeFunctionId::PromiseResolving(target_kind),
                    Some(InternalCallableData::PromiseResolving { promise, kind, .. }),
                ) if target_kind == *kind => {
                    if object.is_constructor
                        || !matches!(self.object(*promise)?.payload, ObjectPayload::Promise(_))
                    {
                        return Err(HeapError::Invariant(
                            "Promise resolving callable has invalid hidden state",
                        ));
                    }
                }
                (
                    NativeFunctionId::PromiseCapabilityExecutor,
                    Some(InternalCallableData::PromiseCapabilityExecutor(capture)),
                ) => {
                    if object.is_constructor
                        || capture
                            .resolve
                            .iter()
                            .chain(capture.reject.iter())
                            .any(|value| !is_promise_storable_value(value))
                    {
                        return Err(HeapError::Invariant(
                            "Promise capability executor has invalid hidden state",
                        ));
                    }
                }
                (
                    NativeFunctionId::PromiseFinallyHandler(_),
                    Some(InternalCallableData::PromiseFinallyHandler {
                        constructor,
                        on_finally,
                    }),
                ) => {
                    let constructor_is_valid = constructor
                        .map(|constructor| {
                            self.object(constructor)
                                .map(|constructor| constructor.is_constructor)
                        })
                        .transpose()?
                        .unwrap_or(true);
                    let on_finally = self.object(*on_finally)?;
                    let on_finally_is_callable = object_data_is_callable(on_finally);
                    if object.is_constructor || !constructor_is_valid || !on_finally_is_callable {
                        return Err(HeapError::Invariant(
                            "Promise finally handler has invalid hidden state",
                        ));
                    }
                }
                (
                    NativeFunctionId::PromiseFinallyThunk(_),
                    Some(InternalCallableData::PromiseFinallyThunk { value }),
                ) => {
                    if object.is_constructor || !is_promise_storable_value(value) {
                        return Err(HeapError::Invariant(
                            "Promise finally thunk has invalid hidden state",
                        ));
                    }
                }
                (
                    NativeFunctionId::PromiseAllResolveElement,
                    Some(InternalCallableData::PromiseAllResolveElement {
                        values,
                        resolve,
                        index,
                        ..
                    }),
                ) => {
                    let values = self.object(*values)?;
                    let resolve = self.object(*resolve)?;
                    let resolve_is_callable = object_data_is_callable(resolve);
                    if object.is_constructor
                        || !matches!(values.payload, ObjectPayload::Array { .. })
                        || !resolve_is_callable
                        || *index == u32::MAX
                    {
                        return Err(HeapError::Invariant(
                            "Promise.all resolve-element callable has invalid hidden state",
                        ));
                    }
                }
                (
                    NativeFunctionId::PromiseAllSettledElement(target_outcome),
                    Some(InternalCallableData::PromiseAllSettledElement {
                        values,
                        resolve,
                        index,
                        outcome,
                        ..
                    }),
                ) if target_outcome == *outcome => {
                    let values = self.object(*values)?;
                    let resolve = self.object(*resolve)?;
                    let resolve_is_callable = object_data_is_callable(resolve);
                    if object.is_constructor
                        || !matches!(values.payload, ObjectPayload::Array { .. })
                        || !resolve_is_callable
                        || *index == u32::MAX
                    {
                        return Err(HeapError::Invariant(
                            "Promise.allSettled element callable has invalid hidden state",
                        ));
                    }
                }
                (
                    NativeFunctionId::PromiseAnyRejectElement,
                    Some(InternalCallableData::PromiseAnyRejectElement {
                        errors,
                        reject,
                        index,
                        ..
                    }),
                ) => {
                    let errors = self.object(*errors)?;
                    let reject = self.object(*reject)?;
                    let reject_is_callable = object_data_is_callable(reject);
                    if object.is_constructor
                        || !matches!(errors.payload, ObjectPayload::Array { .. })
                        || !reject_is_callable
                        || *index == u32::MAX
                    {
                        return Err(HeapError::Invariant(
                            "Promise.any reject-element callable has invalid hidden state",
                        ));
                    }
                }
                (
                    NativeFunctionId::ModuleEvaluation(target_kind),
                    Some(InternalCallableData::ModuleEvaluation { module, kind }),
                ) if target_kind == *kind => {
                    let realm = data.realm.ok_or(HeapError::Invariant(
                        "module-evaluation callback has no defining realm",
                    ))?;
                    self.context(realm)?;
                    let cache = self.context(module.cache)?;
                    let module_is_ready = cache
                        .loaded_modules
                        .records
                        .get(module.module.0)
                        .and_then(Option::as_ref)
                        .is_some_and(|record| {
                            matches!(
                                &record.body,
                                RawModuleRecordBody::SourceText { .. }
                                    | RawModuleRecordBody::Json { .. }
                            )
                        });
                    if object.is_constructor || !module_is_ready {
                        return Err(HeapError::Invariant(
                            "module-evaluation callback has invalid hidden state",
                        ));
                    }
                }
                (
                    NativeFunctionId::AsyncFromSyncIteratorUnwrap,
                    Some(InternalCallableData::AsyncFromSyncIteratorUnwrap { .. }),
                ) => {
                    if object.is_constructor {
                        return Err(HeapError::Invariant(
                            "Async-from-Sync unwrap callable has invalid hidden state",
                        ));
                    }
                }
                (
                    NativeFunctionId::AsyncFromSyncIteratorClose,
                    Some(InternalCallableData::AsyncFromSyncIteratorClose { sync_iterator }),
                ) => {
                    self.object(*sync_iterator)?;
                    if object.is_constructor {
                        return Err(HeapError::Invariant(
                            "Async-from-Sync close callable has invalid hidden state",
                        ));
                    }
                }
                (NativeFunctionId::ProxyRevoke, _)
                | (NativeFunctionId::AsyncFunctionResume(_), _)
                | (NativeFunctionId::AsyncGeneratorResume(_), _)
                | (NativeFunctionId::PromiseResolving(_), _)
                | (NativeFunctionId::PromiseCapabilityExecutor, _)
                | (NativeFunctionId::PromiseFinallyHandler(_), _)
                | (NativeFunctionId::PromiseFinallyThunk(_), _)
                | (NativeFunctionId::PromiseAllResolveElement, _)
                | (NativeFunctionId::PromiseAllSettledElement(_), _)
                | (NativeFunctionId::PromiseAnyRejectElement, _)
                | (NativeFunctionId::ModuleEvaluation(_), _)
                | (NativeFunctionId::DynamicImportHandler(_), _)
                | (NativeFunctionId::AsyncFromSyncIteratorUnwrap, _)
                | (NativeFunctionId::AsyncFromSyncIteratorClose, _)
                | (_, Some(_)) => {
                    return Err(HeapError::Invariant(
                        "native target does not match its internal callable capture",
                    ));
                }
                (_, None) => {}
            }
        }
        if let ObjectPayload::Promise(data) = &object.payload {
            if !is_promise_storable_value(&data.result)
                || (data.state == PromiseState::Pending && data.result != RawValue::Undefined)
                || (data.state != PromiseState::Pending
                    && (!data.fulfill_reactions.is_empty() || !data.reject_reactions.is_empty()))
                || data
                    .fulfill_reactions
                    .iter()
                    .any(|reaction| reaction.kind != PromiseReactionKind::Fulfill)
                || data
                    .reject_reactions
                    .iter()
                    .any(|reaction| reaction.kind != PromiseReactionKind::Reject)
            {
                return Err(HeapError::Invariant(
                    "Promise payload has invalid hidden state",
                ));
            }
        }
        if let ObjectPayload::BytecodeFunction {
            bytecode,
            class_instance_initializer,
            class_static_initializer_started,
            closure_slots,
            ..
        } = &object.payload
        {
            let owner_bytecode = self.function_bytecode(*bytecode)?;
            let expected = usize::from(owner_bytecode.metadata.closure_count);
            if closure_slots.len() != expected {
                return Err(HeapError::Invariant(
                    "function closure slot count does not match its bytecode metadata",
                ));
            }
            if *class_static_initializer_started
                && (!object.is_constructor
                    || owner_bytecode.metadata.constructor_kind == ConstructorKind::None
                    || owner_bytecode.metadata.has_prototype
                    || !owner_bytecode.metadata.strict
                    || owner_bytecode.metadata.class_initializer_kind.is_some())
            {
                return Err(HeapError::Invariant(
                    "class static initializer guard has malformed ownership metadata",
                ));
            }
            if let Some(initializer) = class_instance_initializer {
                let initializer_object = self.object(*initializer)?;
                let ObjectPayload::BytecodeFunction {
                    bytecode: initializer_bytecode,
                    home_object,
                    class_instance_initializer: nested_initializer,
                    ..
                } = &initializer_object.payload
                else {
                    return Err(HeapError::Invariant(
                        "class instance initializer is not a bytecode function",
                    ));
                };
                let initializer_bytecode = self.function_bytecode(*initializer_bytecode)?;
                if !object.is_constructor
                    || owner_bytecode.metadata.constructor_kind == ConstructorKind::None
                    || owner_bytecode.metadata.has_prototype
                    || !owner_bytecode.metadata.strict
                    || owner_bytecode.metadata.class_initializer_kind.is_some()
                    || initializer_object.is_constructor
                    || home_object.is_none()
                    || nested_initializer.is_some()
                    || initializer_bytecode.realm != owner_bytecode.realm
                    || initializer_bytecode.metadata.class_initializer_kind
                        != Some(ClassInitializerKind::InstanceFields)
                    || !initializer_bytecode.metadata.needs_home_object
                {
                    return Err(HeapError::Invariant(
                        "class instance initializer edge has malformed ownership metadata",
                    ));
                }
            }
        }
        if let ObjectPayload::Generator { state, activation } = &object.payload {
            if object.is_constructor
                || matches!(state, GeneratorState::Executing | GeneratorState::Completed)
                    != activation.is_none()
            {
                return Err(HeapError::Invariant(
                    "generator object has inconsistent state and activation",
                ));
            }
            if let Some(activation) = activation.as_deref() {
                let bytecode = self.function_bytecode(activation.bytecode)?;
                let vm = &activation.vm;
                let function = self.object(vm.current_function)?;
                if bytecode.metadata.function_kind != FunctionKind::Generator
                    || bytecode.metadata.constructor_kind != ConstructorKind::None
                    || !bytecode.metadata.has_prototype
                    || bytecode.realm != vm.callee_realm
                    || vm.strict != bytecode.metadata.strict
                    || self.context(vm.callee_realm)?.global_object != vm.callee_global
                    || !matches!(
                        function.payload,
                        ObjectPayload::BytecodeFunction {
                            bytecode: owner,
                            ..
                        } if owner == activation.bytecode
                    )
                    || function.is_constructor
                    || activation.arguments.len() < usize::from(bytecode.metadata.argument_count)
                    || activation.locals.len() != usize::from(bytecode.metadata.local_count)
                    || activation.reusable_captured_locals.len() != activation.locals.len()
                    || vm.stack.len() > usize::from(bytecode.metadata.max_stack)
                    || vm.pc == 0
                    || vm.pc > bytecode.code.len()
                {
                    return Err(HeapError::Invariant(
                        "generator activation has invalid frame metadata",
                    ));
                }
                let suspension_matches = matches!(
                    (state, bytecode.code.get(vm.pc - 1)),
                    (
                        GeneratorState::SuspendedStart,
                        Some(Instruction::InitialYield)
                    ) | (GeneratorState::SuspendedYield, Some(Instruction::Yield))
                        | (
                            GeneratorState::SuspendedYieldStar,
                            Some(Instruction::YieldStar)
                        )
                );
                if !suspension_matches {
                    return Err(HeapError::Invariant(
                        "generator activation is not parked after its suspension opcode",
                    ));
                }
                for value in vm
                    .stack
                    .iter()
                    .chain(std::iter::once(&vm.this_value))
                    .chain(vm.normalized_this.iter())
                    .chain(std::iter::once(&vm.new_target))
                {
                    if !is_map_storable_value(value) {
                        return Err(HeapError::Invariant(
                            "generator activation contains an internal-only value",
                        ));
                    }
                }
                for binding in activation.arguments.iter().chain(activation.locals.iter()) {
                    match binding {
                        GeneratorFrameBinding::Direct(value) if !is_map_storable_value(value) => {
                            return Err(HeapError::Invariant(
                                "generator frame binding contains an internal-only value",
                            ));
                        }
                        GeneratorFrameBinding::Private(atom) if atom.is_null() => {
                            return Err(HeapError::Invariant(
                                "generator private binding contains the null atom",
                            ));
                        }
                        GeneratorFrameBinding::PrivateCallable(callable) => {
                            let callable = self.object(*callable)?;
                            if !object_data_is_callable(callable) {
                                return Err(HeapError::Invariant(
                                    "generator private callable binding is not callable",
                                ));
                            }
                        }
                        GeneratorFrameBinding::Captured(var_ref) => {
                            self.var_ref(*var_ref)?;
                        }
                        GeneratorFrameBinding::Direct(_)
                        | GeneratorFrameBinding::Private(_)
                        | GeneratorFrameBinding::Uninitialized => {}
                    }
                }
                for region in &vm.regions {
                    match *region {
                        crate::vm::VmUnwindRegion::Catch {
                            target,
                            stack_depth,
                        } if target >= bytecode.code.len() || stack_depth > vm.stack.len() => {
                            return Err(HeapError::Invariant(
                                "generator catch region is outside its saved frame",
                            ));
                        }
                        crate::vm::VmUnwindRegion::Iterator { record_base, .. }
                            if record_base.saturating_add(1) >= vm.stack.len() =>
                        {
                            return Err(HeapError::Invariant(
                                "generator iterator region is outside its saved frame",
                            ));
                        }
                        crate::vm::VmUnwindRegion::Iterator {
                            asynchronous: true, ..
                        } => {
                            return Err(HeapError::Invariant(
                                "generator activation contains an invalid iterator state",
                            ));
                        }
                        crate::vm::VmUnwindRegion::Iterator {
                            record_base,
                            enabled: false,
                            asynchronous: false,
                        } if !matches!(vm.stack.get(record_base), Some(RawValue::Undefined)) => {
                            return Err(HeapError::Invariant(
                                "generator activation contains an invalid completed iterator",
                            ));
                        }
                        crate::vm::VmUnwindRegion::Catch { .. }
                        | crate::vm::VmUnwindRegion::Iterator { .. } => {}
                    }
                }
            }
        }
        if let ObjectPayload::AsyncGenerator(data) = &object.payload {
            validate_async_generator_state(self, object, data)?;
        }
        if let ObjectPayload::AsyncFunctionState(data) = &object.payload {
            validate_async_function_state(self, object, data)?;
        }
        if let ObjectPayload::IteratorHelper(data) = &object.payload {
            if object.is_constructor {
                return Err(HeapError::Invariant(
                    "Iterator Helper object is constructable",
                ));
            }
            validate_iterator_helper_data(self, data)?;
        }
        if let ObjectPayload::IteratorWrap(data) = &object.payload {
            if object.is_constructor {
                return Err(HeapError::Invariant(
                    "Iterator Wrap object is constructable",
                ));
            }
            validate_iterator_wrap_data(self, data)?;
        }
        if let ObjectPayload::AsyncFromSyncIterator(data) = &object.payload {
            if object.is_constructor {
                return Err(HeapError::Invariant(
                    "Async-from-Sync Iterator object is constructable",
                ));
            }
            validate_async_from_sync_iterator_data(self, data)?;
        }
        if let ObjectPayload::IteratorConcat(data) = &object.payload {
            if object.is_constructor {
                return Err(HeapError::Invariant(
                    "Iterator Concat object is constructable",
                ));
            }
            validate_iterator_concat_data(self, data)?;
        }
        if let ObjectPayload::BoundFunction {
            target,
            this_value,
            arguments,
        } = &object.payload
        {
            let target = self.object(*target)?;
            if !object_data_is_callable(target) {
                return Err(HeapError::Invariant(
                    "bound function target is not callable",
                ));
            }
            if std::iter::once(this_value)
                .chain(arguments.iter())
                .any(|value| !is_map_storable_value(value))
            {
                return Err(HeapError::Invariant(
                    "bound function payload contains an internal-only value",
                ));
            }
        }
        if let ObjectPayload::Map {
            records,
            live_indices,
            size,
        } = &object.payload
        {
            let mut live = 0usize;
            let mut expected_indices = BTreeSet::new();
            for (index, record) in records.iter().enumerate() {
                match &record.key {
                    Some(key) => {
                        if !is_map_storable_value(key) || !is_map_storable_value(&record.value) {
                            return Err(HeapError::Invariant(
                                "Map record contains an internal value sentinel",
                            ));
                        }
                        live = live.checked_add(1).ok_or(HeapError::Overflow {
                            operation: "validating Map size",
                        })?;
                        expected_indices.insert(index);
                    }
                    None if !matches!(record.value, RawValue::Undefined) => {
                        return Err(HeapError::Invariant(
                            "Map tombstone retains a value payload",
                        ));
                    }
                    None => {}
                }
            }
            if live != *size {
                return Err(HeapError::Invariant(
                    "Map live record count does not match its payload",
                ));
            }
            if *live_indices != expected_indices {
                return Err(HeapError::Invariant(
                    "Map live index does not match its record layout",
                ));
            }
        }
        if let ObjectPayload::MapIterator {
            object: source,
            next_index,
            current_index,
            ..
        } = &object.payload
        {
            match (source, current_index) {
                (Some(map), current) => {
                    let ObjectPayload::Map { records, .. } = &self.object(*map)?.payload else {
                        return Err(HeapError::Invariant(
                            "Map Iterator source does not have the Map class",
                        ));
                    };
                    if current.is_some_and(|index| index >= *next_index || index >= records.len()) {
                        return Err(HeapError::Invariant(
                            "Map Iterator current record is outside its stable cursor",
                        ));
                    }
                }
                (None, None) => {}
                (None, Some(_)) => {
                    return Err(HeapError::Invariant(
                        "completed Map Iterator retains a current record",
                    ));
                }
            }
        }
        if let ObjectPayload::Set {
            records,
            live_indices,
            size,
        } = &object.payload
        {
            let mut live = 0usize;
            let mut expected_indices = BTreeSet::new();
            for (index, record) in records.iter().enumerate() {
                if !matches!(record.value, RawValue::Undefined) {
                    return Err(HeapError::Invariant(
                        "Set record value slot is not undefined",
                    ));
                }
                if let Some(key) = &record.key {
                    if !is_map_storable_value(key) {
                        return Err(HeapError::Invariant(
                            "Set record contains an internal value sentinel",
                        ));
                    }
                    live = live.checked_add(1).ok_or(HeapError::Overflow {
                        operation: "validating Set size",
                    })?;
                    expected_indices.insert(index);
                }
            }
            if live != *size {
                return Err(HeapError::Invariant(
                    "Set live record count does not match its payload",
                ));
            }
            if *live_indices != expected_indices {
                return Err(HeapError::Invariant(
                    "Set live index does not match its record layout",
                ));
            }
        }
        if let ObjectPayload::SetIterator {
            object: source,
            next_index,
            current_index,
            ..
        } = &object.payload
        {
            match (source, current_index) {
                (Some(set), current) => {
                    let ObjectPayload::Set { records, .. } = &self.object(*set)?.payload else {
                        return Err(HeapError::Invariant(
                            "Set Iterator source does not have the Set class",
                        ));
                    };
                    if current.is_some_and(|index| index >= *next_index || index >= records.len()) {
                        return Err(HeapError::Invariant(
                            "Set Iterator current record is outside its stable cursor",
                        ));
                    }
                }
                (None, None) => {}
                (None, Some(_)) => {
                    return Err(HeapError::Invariant(
                        "completed Set Iterator retains a current record",
                    ));
                }
            }
        }
        if let ObjectPayload::WeakMap { records } = &object.payload {
            records.validate_order()?;
            if records.values().any(|value| !is_map_storable_value(value)) {
                return Err(HeapError::Invariant(
                    "WeakMap record contains an internal value sentinel",
                ));
            }
            for key in records.keys() {
                if let WeakCollectionKey::Object(key) = key {
                    // Weak records deliberately outlive a zero-refcount key
                    // until the next explicit weak-record pruning pass. A
                    // stale generational identity is therefore valid here;
                    // only revalidate keys which are still live.
                    if self.is_live(RawId::Object(*key)) {
                        self.object(*key)?;
                    }
                }
            }
        }
        if let ObjectPayload::WeakSet { records } = &object.payload {
            records.validate_order()?;
            for key in records.keys() {
                if let WeakCollectionKey::Object(key) = key {
                    if self.is_live(RawId::Object(*key)) {
                        self.object(*key)?;
                    }
                }
            }
        }
        if let ObjectPayload::WeakRef {
            target: Some(WeakCollectionKey::Object(target)),
        } = &object.payload
            && self.is_live(RawId::Object(*target))
        {
            self.object(*target)?;
        }
        if let ObjectPayload::FinalizationRegistry(data) = &object.payload {
            if !object_data_is_callable(self.object(data.callback)?) {
                return Err(HeapError::Invariant(
                    "FinalizationRegistry callback is not callable",
                ));
            }
            self.context(data.realm)?;
            for entry in &data.entries {
                if !is_map_storable_value(&entry.held_value)
                    || raw_value_matches_weak_key(&entry.held_value, entry.target)
                {
                    return Err(HeapError::Invariant(
                        "FinalizationRegistry contains an invalid held value",
                    ));
                }
                for weak in [Some(entry.target), entry.unregister_token]
                    .into_iter()
                    .flatten()
                {
                    if let WeakCollectionKey::Object(target) = weak
                        && self.is_live(RawId::Object(target))
                    {
                        self.object(target)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn validate_replacement_slot(
        &self,
        id: ObjectId,
        slot_index: usize,
        replacement: &PropertySlot,
    ) -> Result<(), HeapError> {
        let object = self.object(id)?;
        let shape = self.shape(object.shape)?;
        let entry = shape.entries().get(slot_index).ok_or(HeapError::Invariant(
            "property slot index is outside the object shape",
        ))?;
        if !slot_matches_storage(replacement, entry.flags.storage) {
            return Err(HeapError::Invariant(
                "replacement property storage does not match its shape flags",
            ));
        }
        if matches!(replacement, PropertySlot::Data(RawValue::Private(_))) {
            return Err(HeapError::Invariant(
                "private-name identity escaped into an object value slot",
            ));
        }
        Ok(())
    }
    fn validate_slot_identity(&self, id: RawId) -> Result<usize, HeapError> {
        let index = id.index() as usize;
        let slot = self.slots.get(index).ok_or(HeapError::Stale {
            index: id.index(),
            generation: id.generation(),
        })?;
        if slot.generation != id.generation() {
            return Err(HeapError::Stale {
                index: id.index(),
                generation: id.generation(),
            });
        }
        let actual = slot.state.kind().ok_or(HeapError::Stale {
            index: id.index(),
            generation: id.generation(),
        })?;
        if actual != id.kind() {
            return Err(HeapError::WrongKind {
                expected: id.kind(),
                actual,
            });
        }
        Ok(index)
    }

    fn live_index(&self, id: RawId) -> Result<usize, HeapError> {
        let index = self.validate_slot_identity(id)?;
        if !matches!(self.slots[index].state, SlotState::Live(_)) {
            return Err(HeapError::Invariant(
                "heap edge targeted a node outside Live state",
            ));
        }
        Ok(index)
    }

    fn live_node(&self, id: RawId) -> Result<&Node, HeapError> {
        let index = self.validate_slot_identity(id)?;
        match &self.slots[index].state {
            SlotState::Live(node) => Ok(node),
            _ => Err(HeapError::Stale {
                index: id.index(),
                generation: id.generation(),
            }),
        }
    }

    fn live_node_mut(&mut self, id: RawId) -> Result<&mut Node, HeapError> {
        let index = self.validate_slot_identity(id)?;
        match &mut self.slots[index].state {
            SlotState::Live(node) => Ok(node),
            _ => Err(HeapError::Stale {
                index: id.index(),
                generation: id.generation(),
            }),
        }
    }

    fn object_mut(&mut self, id: ObjectId) -> Result<&mut ObjectData, HeapError> {
        match &mut self.live_node_mut(RawId::Object(id))?.data {
            NodeData::Object(object) => Ok(object),
            NodeData::Shape(_)
            | NodeData::VarRef(_)
            | NodeData::Context(_)
            | NodeData::FunctionBytecode(_) => Err(HeapError::Invariant(
                "typed object lookup reached another node payload",
            )),
        }
    }

    fn var_ref_mut(&mut self, id: VarRefId) -> Result<&mut VarRefData, HeapError> {
        match &mut self.live_node_mut(RawId::VarRef(id))?.data {
            NodeData::VarRef(var_ref) => Ok(var_ref),
            NodeData::Object(_)
            | NodeData::Shape(_)
            | NodeData::Context(_)
            | NodeData::FunctionBytecode(_) => Err(HeapError::Invariant(
                "typed var-ref lookup reached another node payload",
            )),
        }
    }

    fn strong_count(&self, id: RawId) -> Result<u32, HeapError> {
        let index = self.validate_slot_identity(id)?;
        self.slots[index].state.strong().ok_or(HeapError::Stale {
            index: id.index(),
            generation: id.generation(),
        })
    }

    fn is_live(&self, id: RawId) -> bool {
        self.validate_slot_identity(id)
            .is_ok_and(|index| matches!(self.slots[index].state, SlotState::Live(_)))
    }
}
fn validate_module_storable_value(value: &RawValue) -> Result<(), HeapError> {
    if matches!(
        value,
        RawValue::Private(_) | RawValue::Uninitialized | RawValue::Exception
    ) {
        return Err(HeapError::Invariant(
            "loaded-module record contains an internal value sentinel",
        ));
    }
    Ok(())
}

/// Return the occurrence-count difference `left - right` while retaining
/// `left`'s deterministic order. All allocation happens before a caller may
/// publish a record mutation.
fn multiset_difference<T>(
    left: &[T],
    right: &[T],
    operation: &'static str,
) -> Result<Vec<T>, HeapError>
where
    T: Copy + Eq + Hash,
{
    let mut remaining = HashMap::<T, usize>::new();
    remaining
        .try_reserve(right.len())
        .map_err(|_| HeapError::Allocation { operation })?;
    for &item in right {
        let count = remaining.entry(item).or_default();
        *count = count
            .checked_add(1)
            .ok_or(HeapError::Overflow { operation })?;
    }

    let mut difference = Vec::new();
    difference
        .try_reserve(left.len())
        .map_err(|_| HeapError::Allocation { operation })?;
    for &item in left {
        match remaining.get_mut(&item) {
            Some(count) if *count != 0 => *count -= 1,
            Some(_) | None => difference.push(item),
        }
    }
    Ok(difference)
}
const fn is_map_storable_value(value: &RawValue) -> bool {
    !matches!(
        value,
        RawValue::Private(_) | RawValue::Uninitialized | RawValue::Exception
    )
}

const fn is_promise_storable_value(value: &RawValue) -> bool {
    !matches!(
        value,
        RawValue::Private(_) | RawValue::Uninitialized | RawValue::Exception
    )
}

fn object_data_is_callable(object: &ObjectData) -> bool {
    matches!(
        &object.payload,
        ObjectPayload::NativeFunction { .. }
            | ObjectPayload::BoundFunction { .. }
            | ObjectPayload::BytecodeFunction { .. }
            | ObjectPayload::Proxy(ProxyData {
                is_callable: true,
                ..
            })
    )
}

fn validate_async_function_state(
    heap: &Heap,
    object: &ObjectData,
    data: &AsyncFunctionStateData,
) -> Result<(), HeapError> {
    if object.is_constructor
        || (data.phase == AsyncFunctionPhase::Awaiting) != data.activation.is_some()
    {
        return Err(HeapError::Invariant(
            "AsyncFunction state has inconsistent phase and activation",
        ));
    }
    heap.context(data.driver_realm)?;
    for resolving_function in [data.outer_resolve, data.outer_reject] {
        if !object_data_is_callable(heap.object(resolving_function)?) {
            return Err(HeapError::Invariant(
                "AsyncFunction state retains a non-callable resolving function",
            ));
        }
    }

    let Some(activation) = data.activation.as_deref() else {
        return Ok(());
    };
    let bytecode = heap.function_bytecode(activation.bytecode)?;
    let vm = &activation.vm;
    let function = heap.object(vm.current_function)?;
    if bytecode.metadata.function_kind != FunctionKind::Async
        || bytecode.metadata.constructor_kind != ConstructorKind::None
        || bytecode.metadata.has_prototype
        || bytecode.metadata.class_initializer_kind.is_some()
        || bytecode.realm != vm.callee_realm
        || vm.strict != bytecode.metadata.strict
        || heap.context(vm.callee_realm)?.global_object != vm.callee_global
        || !matches!(
            function.payload,
            ObjectPayload::BytecodeFunction {
                bytecode: owner,
                ..
            } if owner == activation.bytecode
        )
        || function.is_constructor
        || activation.arguments.len() < usize::from(bytecode.metadata.argument_count)
        || activation.actual_argument_count > activation.arguments.len()
        || activation.locals.len() != usize::from(bytecode.metadata.local_count)
        || activation.reusable_captured_locals.len() != activation.locals.len()
        || vm.stack.len() > usize::from(bytecode.metadata.max_stack)
        || vm.pc == 0
        || vm.pc > bytecode.code.len()
    {
        return Err(HeapError::Invariant(
            "AsyncFunction activation has invalid frame metadata",
        ));
    }
    if !matches!(bytecode.code.get(vm.pc - 1), Some(Instruction::Await)) {
        return Err(HeapError::Invariant(
            "AsyncFunction activation is not parked after its await opcode",
        ));
    }

    for value in vm
        .stack
        .iter()
        .chain(std::iter::once(&vm.this_value))
        .chain(vm.normalized_this.iter())
        .chain(std::iter::once(&vm.new_target))
    {
        if !is_map_storable_value(value) {
            return Err(HeapError::Invariant(
                "AsyncFunction activation contains an internal-only value",
            ));
        }
    }
    for binding in activation.arguments.iter().chain(activation.locals.iter()) {
        match binding {
            GeneratorFrameBinding::Direct(value) if !is_map_storable_value(value) => {
                return Err(HeapError::Invariant(
                    "AsyncFunction frame binding contains an internal-only value",
                ));
            }
            GeneratorFrameBinding::Private(atom) if atom.is_null() => {
                return Err(HeapError::Invariant(
                    "AsyncFunction private binding contains the null atom",
                ));
            }
            GeneratorFrameBinding::PrivateCallable(callable) => {
                if !object_data_is_callable(heap.object(*callable)?) {
                    return Err(HeapError::Invariant(
                        "AsyncFunction private callable binding is not callable",
                    ));
                }
            }
            GeneratorFrameBinding::Captured(var_ref) => {
                heap.var_ref(*var_ref)?;
            }
            GeneratorFrameBinding::Direct(_)
            | GeneratorFrameBinding::Private(_)
            | GeneratorFrameBinding::Uninitialized => {}
        }
    }
    for (region_index, region) in vm.regions.iter().enumerate() {
        match *region {
            crate::vm::VmUnwindRegion::Catch {
                target,
                stack_depth,
            } if target >= bytecode.code.len() || stack_depth > vm.stack.len() => {
                return Err(HeapError::Invariant(
                    "AsyncFunction catch region is outside its saved frame",
                ));
            }
            crate::vm::VmUnwindRegion::Iterator { record_base, .. }
                if record_base.saturating_add(1) >= vm.stack.len() =>
            {
                return Err(HeapError::Invariant(
                    "AsyncFunction iterator region is outside its saved frame",
                ));
            }
            crate::vm::VmUnwindRegion::Iterator {
                record_base,
                enabled: false,
                asynchronous: false,
            } if !matches!(vm.stack.get(record_base), Some(RawValue::Undefined)) => {
                return Err(HeapError::Invariant(
                    "AsyncFunction activation contains an invalid completed iterator",
                ));
            }
            crate::vm::VmUnwindRegion::Iterator {
                record_base,
                enabled: false,
                asynchronous,
                ..
            } if asynchronous
                && (region_index + 1 != vm.regions.len()
                    || record_base.checked_add(3) != Some(vm.stack.len())
                    || !matches!(vm.stack.last(), Some(RawValue::Undefined))
                    || !matches!(
                        bytecode.code.get(vm.pc),
                        Some(Instruction::IteratorGetValueDone)
                    )) =>
            {
                return Err(HeapError::Invariant(
                    "AsyncFunction activation contains an invalid pending iterator state",
                ));
            }
            crate::vm::VmUnwindRegion::Catch { .. }
            | crate::vm::VmUnwindRegion::Iterator { .. } => {}
        }
    }
    Ok(())
}

fn validate_async_generator_state(
    heap: &Heap,
    object: &ObjectData,
    data: &AsyncGeneratorData,
) -> Result<(), HeapError> {
    let state_shape_is_valid = match data.state {
        AsyncGeneratorState::SuspendedStart
        | AsyncGeneratorState::SuspendedYield
        | AsyncGeneratorState::SuspendedYieldStar => {
            data.activation.is_some() && data.resume_realm.is_none()
        }
        AsyncGeneratorState::Executing => {
            (data.activation.is_none() && data.resume_realm.is_none())
                || (data.resume_realm.is_some()
                    && data.activation.as_deref().is_some_and(|activation| {
                        matches!(
                            heap.function_bytecode(activation.bytecode),
                            Ok(bytecode)
                                if matches!(
                                    bytecode.code.get(activation.vm.pc.saturating_sub(1)),
                                    Some(Instruction::Await)
                                )
                        )
                    }))
        }
        AsyncGeneratorState::AwaitingReturn => {
            data.activation.is_none()
                && data.resume_realm.is_some()
                && data
                    .queue
                    .front()
                    .is_some_and(|request| request.completion == GeneratorResumeKind::Return)
        }
        AsyncGeneratorState::Completed => data.activation.is_none() && data.resume_realm.is_none(),
    };
    if object.is_constructor
        || !state_shape_is_valid
        || matches!(
            data.state,
            AsyncGeneratorState::Executing | AsyncGeneratorState::AwaitingReturn
        ) && data.queue.is_empty()
    {
        return Err(HeapError::Invariant(
            "AsyncGenerator has inconsistent state, activation, or queue",
        ));
    }
    if let Some(realm) = data.resume_realm {
        heap.context(realm)?;
    }
    for request in &data.queue {
        if !is_promise_storable_value(&request.result)
            || !matches!(
                heap.object(request.promise)?.payload,
                ObjectPayload::Promise(_)
            )
        {
            return Err(HeapError::Invariant(
                "AsyncGenerator request retains an invalid value or Promise",
            ));
        }
        let ObjectPayload::NativeFunction {
            data: resolve_data,
            internal:
                Some(InternalCallableData::PromiseResolving {
                    promise: resolve_promise,
                    already_resolved: resolve_cell,
                    kind: PromiseResolvingKind::Resolve,
                }),
        } = &heap.object(request.resolve)?.payload
        else {
            return Err(HeapError::Invariant(
                "AsyncGenerator request retains an invalid resolve function",
            ));
        };
        let ObjectPayload::NativeFunction {
            data: reject_data,
            internal:
                Some(InternalCallableData::PromiseResolving {
                    promise: reject_promise,
                    already_resolved: reject_cell,
                    kind: PromiseResolvingKind::Reject,
                }),
        } = &heap.object(request.reject)?.payload
        else {
            return Err(HeapError::Invariant(
                "AsyncGenerator request retains an invalid reject function",
            ));
        };
        if resolve_data.target != NativeFunctionId::PromiseResolving(PromiseResolvingKind::Resolve)
            || reject_data.target
                != NativeFunctionId::PromiseResolving(PromiseResolvingKind::Reject)
            || *resolve_promise != request.promise
            || *reject_promise != request.promise
            || !Rc::ptr_eq(resolve_cell, reject_cell)
        {
            return Err(HeapError::Invariant(
                "AsyncGenerator request capability does not resolve its Promise",
            ));
        }
    }

    let Some(activation) = data.activation.as_deref() else {
        return Ok(());
    };
    let bytecode = heap.function_bytecode(activation.bytecode)?;
    let vm = &activation.vm;
    let function = heap.object(vm.current_function)?;
    if bytecode.metadata.function_kind != FunctionKind::AsyncGenerator
        || bytecode.metadata.constructor_kind != ConstructorKind::None
        || !bytecode.metadata.has_prototype
        || bytecode.metadata.class_initializer_kind.is_some()
        || bytecode.realm != vm.callee_realm
        || vm.strict != bytecode.metadata.strict
        || heap.context(vm.callee_realm)?.global_object != vm.callee_global
        || !matches!(
            function.payload,
            ObjectPayload::BytecodeFunction {
                bytecode: owner,
                ..
            } if owner == activation.bytecode
        )
        || function.is_constructor
        || activation.arguments.len() < usize::from(bytecode.metadata.argument_count)
        || activation.actual_argument_count > activation.arguments.len()
        || activation.locals.len() != usize::from(bytecode.metadata.local_count)
        || activation.reusable_captured_locals.len() != activation.locals.len()
        || vm.stack.len() > usize::from(bytecode.metadata.max_stack)
        || vm.pc == 0
        || vm.pc > bytecode.code.len()
    {
        return Err(HeapError::Invariant(
            "AsyncGenerator activation has invalid frame metadata",
        ));
    }
    let suspension_matches = matches!(
        (data.state, bytecode.code.get(vm.pc - 1)),
        (
            AsyncGeneratorState::SuspendedStart,
            Some(Instruction::InitialYield)
        ) | (
            AsyncGeneratorState::SuspendedYield,
            Some(Instruction::Yield)
        ) | (
            AsyncGeneratorState::SuspendedYieldStar,
            Some(Instruction::AsyncYieldStar)
        ) | (AsyncGeneratorState::Executing, Some(Instruction::Await))
    );
    if !suspension_matches {
        return Err(HeapError::Invariant(
            "AsyncGenerator activation is parked after the wrong opcode",
        ));
    }
    for value in vm
        .stack
        .iter()
        .chain(std::iter::once(&vm.this_value))
        .chain(vm.normalized_this.iter())
        .chain(std::iter::once(&vm.new_target))
    {
        if !is_map_storable_value(value) {
            return Err(HeapError::Invariant(
                "AsyncGenerator activation contains an internal-only value",
            ));
        }
    }
    for binding in activation.arguments.iter().chain(activation.locals.iter()) {
        match binding {
            GeneratorFrameBinding::Direct(value) if !is_map_storable_value(value) => {
                return Err(HeapError::Invariant(
                    "AsyncGenerator frame binding contains an internal-only value",
                ));
            }
            GeneratorFrameBinding::Private(atom) if atom.is_null() => {
                return Err(HeapError::Invariant(
                    "AsyncGenerator private binding contains the null atom",
                ));
            }
            GeneratorFrameBinding::PrivateCallable(callable) => {
                if !object_data_is_callable(heap.object(*callable)?) {
                    return Err(HeapError::Invariant(
                        "AsyncGenerator private callable binding is not callable",
                    ));
                }
            }
            GeneratorFrameBinding::Captured(var_ref) => {
                heap.var_ref(*var_ref)?;
            }
            GeneratorFrameBinding::Direct(_)
            | GeneratorFrameBinding::Private(_)
            | GeneratorFrameBinding::Uninitialized => {}
        }
    }
    for (region_index, region) in vm.regions.iter().enumerate() {
        match *region {
            crate::vm::VmUnwindRegion::Catch {
                target,
                stack_depth,
            } if target >= bytecode.code.len() || stack_depth > vm.stack.len() => {
                return Err(HeapError::Invariant(
                    "AsyncGenerator catch region is outside its saved frame",
                ));
            }
            crate::vm::VmUnwindRegion::Iterator { record_base, .. }
                if record_base.saturating_add(1) >= vm.stack.len() =>
            {
                return Err(HeapError::Invariant(
                    "AsyncGenerator iterator region is outside its saved frame",
                ));
            }
            crate::vm::VmUnwindRegion::Iterator {
                record_base,
                enabled: false,
                asynchronous: false,
            } if !matches!(vm.stack.get(record_base), Some(RawValue::Undefined)) => {
                return Err(HeapError::Invariant(
                    "AsyncGenerator activation contains an invalid completed iterator",
                ));
            }
            crate::vm::VmUnwindRegion::Iterator {
                record_base,
                enabled: false,
                asynchronous,
                ..
            } if asynchronous
                && (region_index + 1 != vm.regions.len()
                    || !matches!(data.state, AsyncGeneratorState::Executing)
                    || record_base.checked_add(3) != Some(vm.stack.len())
                    || !matches!(vm.stack.last(), Some(RawValue::Undefined))
                    || !matches!(
                        bytecode.code.get(vm.pc),
                        Some(Instruction::IteratorGetValueDone)
                    )) =>
            {
                return Err(HeapError::Invariant(
                    "AsyncGenerator activation contains an invalid pending iterator state",
                ));
            }
            crate::vm::VmUnwindRegion::Catch { .. }
            | crate::vm::VmUnwindRegion::Iterator { .. } => {}
        }
    }
    Ok(())
}

fn validate_iterator_helper_data(heap: &Heap, data: &IteratorHelperData) -> Result<(), HeapError> {
    heap.object(data.source)?;
    if let Some(inner) = data.inner {
        heap.object(inner)?;
    }
    if !is_map_storable_value(&data.next)
        || !is_map_storable_value(&data.callback)
        || data.count < 0
        || (data.kind != IteratorHelperKind::FlatMap && data.inner.is_some())
    {
        return Err(HeapError::Invariant(
            "Iterator Helper payload has invalid hidden state",
        ));
    }
    match data.kind {
        IteratorHelperKind::Drop | IteratorHelperKind::Take => {
            if !matches!(data.callback, RawValue::Undefined) {
                return Err(HeapError::Invariant(
                    "limit Iterator Helper unexpectedly retains a callback",
                ));
            }
        }
        IteratorHelperKind::Filter | IteratorHelperKind::FlatMap | IteratorHelperKind::Map => {
            let RawValue::Object(callback) = data.callback else {
                return Err(HeapError::Invariant(
                    "callback Iterator Helper does not retain a callable",
                ));
            };
            if !object_data_is_callable(heap.object(callback)?) {
                return Err(HeapError::Invariant(
                    "callback Iterator Helper does not retain a callable",
                ));
            }
        }
    }
    Ok(())
}

fn validate_iterator_wrap_data(heap: &Heap, data: &IteratorWrapData) -> Result<(), HeapError> {
    if !is_map_storable_value(&data.source) || !is_map_storable_value(&data.next) {
        return Err(HeapError::Invariant(
            "Iterator Wrap payload contains an internal value sentinel",
        ));
    }
    for edge in raw_value_edges(&data.source)
        .into_iter()
        .chain(raw_value_edges(&data.next))
    {
        let RawId::Object(object) = edge else {
            unreachable!("RawValue only owns object edges")
        };
        heap.object(object)?;
    }
    Ok(())
}

fn validate_async_from_sync_iterator_data(
    heap: &Heap,
    data: &AsyncFromSyncIteratorData,
) -> Result<(), HeapError> {
    heap.object(data.sync_iterator)?;
    if !is_map_storable_value(&data.next) {
        return Err(HeapError::Invariant(
            "Async-from-Sync Iterator payload contains an internal value sentinel",
        ));
    }
    for edge in raw_value_edges(&data.next) {
        let RawId::Object(object) = edge else {
            unreachable!("RawValue only owns object edges")
        };
        heap.object(object)?;
    }
    Ok(())
}

fn validate_iterator_concat_data(heap: &Heap, data: &IteratorConcatData) -> Result<(), HeapError> {
    if data.index > data.items.len() || !is_map_storable_value(&data.next) {
        return Err(HeapError::Invariant(
            "Iterator Concat payload has invalid hidden state",
        ));
    }
    for (index, item) in data.items.iter().enumerate() {
        if (index < data.index) != item.is_none() {
            return Err(HeapError::Invariant(
                "Iterator Concat released-input boundary is inconsistent",
            ));
        }
        let Some(item) = item else {
            continue;
        };
        heap.object(item.iterable)?;
        let RawValue::Object(method) = item.method else {
            return Err(HeapError::Invariant(
                "Iterator Concat input method is not callable",
            ));
        };
        if !object_data_is_callable(heap.object(method)?) {
            return Err(HeapError::Invariant(
                "Iterator Concat input method is not callable",
            ));
        }
    }
    if let Some(iterator) = data.iterator {
        heap.object(iterator)?;
        if data.index >= data.items.len() {
            return Err(HeapError::Invariant(
                "Iterator Concat retains an iterator after exhaustion",
            ));
        }
    } else if !matches!(data.next, RawValue::Undefined) {
        return Err(HeapError::Invariant(
            "Iterator Concat caches next without a current iterator",
        ));
    }
    for edge in raw_value_edges(&data.next) {
        let RawId::Object(object) = edge else {
            unreachable!("RawValue only owns object edges")
        };
        heap.object(object)?;
    }
    Ok(())
}

fn validate_var_ref_payload(var_ref: &VarRefData) -> Result<(), HeapError> {
    validate_var_ref_value(
        var_ref.kind,
        var_ref.is_lexical,
        var_ref.is_const,
        &var_ref.value,
    )
}

fn validate_var_ref_value(
    kind: ClosureVariableKind,
    is_lexical: bool,
    is_const: bool,
    value: &RawValue,
) -> Result<(), HeapError> {
    if kind == ClosureVariableKind::ModuleImportView {
        return Err(HeapError::Invariant(
            "module-import view escaped into a VarRef cell",
        ));
    }
    if kind.is_private() && (!is_lexical || !is_const) {
        return Err(HeapError::Invariant(
            "private-element VarRef is not an immutable lexical binding",
        ));
    }
    match kind {
        ClosureVariableKind::PrivateField
            if !matches!(value, RawValue::Private(_) | RawValue::Uninitialized) =>
        {
            return Err(HeapError::Invariant(
                "private-name VarRef contains an ordinary ECMAScript value",
            ));
        }
        ClosureVariableKind::PrivateMethod
        | ClosureVariableKind::PrivateGetter
        | ClosureVariableKind::PrivateSetter
        | ClosureVariableKind::PrivateGetterSetter
            if !matches!(value, RawValue::Object(_) | RawValue::Uninitialized) =>
        {
            return Err(HeapError::Invariant(
                "private-method VarRef contains a non-callable representation",
            ));
        }
        _ => {}
    }
    if !kind.is_private() && matches!(value, RawValue::Private(_)) {
        return Err(HeapError::Invariant(
            "private-name identity escaped into an ordinary VarRef",
        ));
    }
    Ok(())
}
const fn slot_matches_storage(slot: &PropertySlot, storage: PropertyStorageKind) -> bool {
    matches!(
        (slot, storage),
        (PropertySlot::Data(_), PropertyStorageKind::Data)
            | (PropertySlot::VarRef(_), PropertyStorageKind::Data)
            | (PropertySlot::AutoInit(_), PropertyStorageKind::Data)
            | (PropertySlot::Accessor { .. }, PropertyStorageKind::Accessor)
    )
}

fn increment_kind_count(counts: &mut HeapCounts, kind: HeapNodeKind) {
    match kind {
        HeapNodeKind::Object => counts.object_nodes = counts.object_nodes.saturating_add(1),
        HeapNodeKind::Shape => counts.shape_nodes = counts.shape_nodes.saturating_add(1),
        HeapNodeKind::VarRef => counts.var_ref_nodes = counts.var_ref_nodes.saturating_add(1),
        HeapNodeKind::Context => counts.context_nodes = counts.context_nodes.saturating_add(1),
        HeapNodeKind::FunctionBytecode => {
            counts.function_bytecode_nodes = counts.function_bytecode_nodes.saturating_add(1);
        }
    }
}

#[cfg(test)]
mod tests;
