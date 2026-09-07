expect_rewrite_rejected ordinary-consumer-float-normalization ordinary-leaf-consumer-lowering \
    src/runtime/binary_object_publish.rs \
    'DetachedPrimitive::Float64Bits(bits) => Value::Float(f64::from_bits(bits)),' \
    'DetachedPrimitive::Float64Bits(bits) => Value::number(f64::from_bits(bits)),'
expect_rewrite_rejected ordinary-consumer-op-remap ordinary-leaf-consumer-lowering \
    src/runtime/binary_object_publish.rs \
    'OrdinaryLeafBinaryOp::Add => Instruction::Add,' \
    'OrdinaryLeafBinaryOp::Add => Instruction::Sub,'
expect_rewrite_rejected ordinary-consumer-verifier-dead-branch ordinary-leaf-consumer-publication \
    src/runtime/binary_object_publish.rs \
    $'        super::bytecode_publish::verify_unlinked_ordinary_leaf(&function)\n            .map_err(map_ordinary_leaf_verification_error)?;' \
    $'        if false {\n            super::bytecode_publish::verify_unlinked_ordinary_leaf(&function)\n                .map_err(map_ordinary_leaf_verification_error)?;\n        }'
expect_rewrite_rejected ordinary-consumer-generic-publisher ordinary-leaf-consumer-publication \
    src/runtime/binary_object_publish.rs \
    'self.publish_verified_unlinked_function(realm, function)?' \
    'self.publish_unlinked_function(realm, function)?'
expect_full_rewrite_rejected ordinary-consumer-raw-native-plan ordinary-leaf-consumer-import \
    src/runtime/binary_object_publish.rs \
    $'#[cfg(test)]\nmod tests {' \
    $'fn leak_native_plan(_: NativeCodePlan<'"'"'_>) {}\n\n#[cfg(test)]\nmod tests {'
expect_full_rewrite_rejected ordinary-consumer-test262-branch ordinary-leaf-consumer-special-casing \
    src/runtime/binary_object_publish.rs \
    $'#[cfg(test)]\nmod tests {' \
    $'fn fixture_dispatch(bytes: &[u8]) -> bool { bytes.starts_with(&[0x05, 0x00]) } // Test262 fixture\n\n#[cfg(test)]\nmod tests {'
expect_rejected root-public-module root-module-visibility \
    src/runtime/binary_object/mod.rs \
    'pub(in crate::runtime) mod leaked;'
expect_rewrite_rejected ordinary-leaf-public-module root-module-visibility \
    src/runtime/binary_object/mod.rs \
    'mod ordinary_leaf;' \
    'pub(super) mod ordinary_leaf;'
expect_rewrite_rejected scalar-script-public-module root-module-visibility \
    src/runtime/binary_object/mod.rs \
    'mod scalar_script;' \
    'pub(super) mod scalar_script;'
expect_rejected root-extra-private-module root-private-module-set \
    src/runtime/binary_object/mod.rs \
    'mod unreviewed_admission;'
expect_rejected root-reexport root-reexport \
    src/runtime/binary_object/mod.rs \
    'pub(in crate::runtime) use bytecode_image::*;'
expect_rewrite_rejected scalar-facade-extra-type scalar-script-facade-shape \
    src/runtime/binary_object/mod.rs \
    'pub(super) use scalar_script::{ScalarScriptReadError, ScalarStringDraft, ScalarUnaryOp, ScalarValueDraft, decode_trusted_scalar_script};' \
    'pub(super) use scalar_script::{BytecodeImage, ScalarScriptReadError, ScalarStringDraft, ScalarUnaryOp, ScalarValueDraft, decode_trusted_scalar_script};'
expect_rewrite_rejected scalar-facade-wider-visibility scalar-script-facade-shape \
    src/runtime/binary_object/mod.rs \
    'pub(super) use scalar_script::{ScalarScriptReadError, ScalarStringDraft, ScalarUnaryOp, ScalarValueDraft, decode_trusted_scalar_script};' \
    'pub(crate) use scalar_script::{ScalarScriptReadError, ScalarStringDraft, ScalarUnaryOp, ScalarValueDraft, decode_trusted_scalar_script};'
expect_rewrite_rejected ordinary-facade-extra-type ordinary-leaf-facade-shape \
    src/runtime/binary_object/mod.rs \
    'DetachedPrimitive, OrdinaryLeafApplyKind, OrdinaryLeafBinaryOp, OrdinaryLeafDraft,' \
    'BytecodeImage, DetachedPrimitive, OrdinaryLeafApplyKind, OrdinaryLeafBinaryOp, OrdinaryLeafDraft,'
expect_rewrite_rejected ordinary-facade-wider-visibility ordinary-leaf-facade-shape \
    src/runtime/binary_object/mod.rs \
    'pub(super) use ordinary_leaf::{' \
    'pub(crate) use ordinary_leaf::{'
expect_rejected ordinary-extra-visible-item ordinary-leaf-visible-item-set \
    src/runtime/binary_object/ordinary_leaf.rs \
    'pub(in crate::runtime) fn leak_archive_identity() {}'
expect_rejected ordinary-private-helper ordinary-leaf-helper-set \
    src/runtime/binary_object/ordinary_leaf.rs \
    'fn bypass_ordinary_admission() {}'
expect_rejected ordinary-raw-code-dependency ordinary-leaf-native-plan-boundary \
    src/runtime/binary_object/ordinary_leaf.rs \
    'fn leak_raw_code(_: &ImageCode) {}'
expect_rewrite_rejected ordinary-input-prefix-dispatch ordinary-leaf-special-casing \
    src/runtime/binary_object/ordinary_leaf.rs \
    '    if input.len() > MAX_INPUT_BYTES {' \
    '    if input.starts_with(&[0x05, 0x00]) || input.len() > MAX_INPUT_BYTES {'
expect_rewrite_rejected ordinary-verifier-strip-bypass ordinary-leaf-verifier-role \
    src/runtime/bytecode_publish.rs \
    '                        || !metadata.strip_variable_debug' \
    '                        || false'
expect_rewrite_rejected ordinary-verifier-debug-bypass ordinary-leaf-verifier-role \
    src/runtime/bytecode_publish.rs \
    '                        || function.debug().is_some()' \
    '                        || false'
expect_rewrite_rejected ordinary-verifier-primitive-broadening ordinary-leaf-plain-primitive \
    src/function.rs \
    'matches!(self.0, UnlinkedConstantKind::Primitive(_))' \
    'matches!(self.0, UnlinkedConstantKind::Primitive(_) | UnlinkedConstantKind::AtomString(_))'
expect_rewrite_rejected ordinary-verifier-empty-atom-broadening ordinary-leaf-plain-primitive \
    src/function.rs \
    'UnlinkedConstantKind::AtomString(Value::String(value)) if value.is_empty()' \
    'UnlinkedConstantKind::AtomString(Value::String(_))'
expect_rewrite_rejected ordinary-public-api-selector-collapse ordinary-leaf-public-api \
    src/runtime/context/bytecode.rs \
    $'            bytes,\n            root_constant_index,\n        );' \
    $'            bytes,\n            0,\n        );'
expect_rewrite_rejected ordinary-public-api-pending-broadening ordinary-leaf-public-api \
    src/runtime/context/bytecode.rs \
    $'            Ok(function) => Ok(function),\n            Err(RuntimeError::Engine(error))\n                if NativeErrorKind::from_javascript_error(error.kind()).is_some() =>' \
    $'            Ok(function) => Ok(function),\n            Err(RuntimeError::Engine(error))\n                if true || NativeErrorKind::from_javascript_error(error.kind()).is_some() =>'
expect_rewrite_rejected scalar-draft-copy-regression scalar-script-draft-shape \
    src/runtime/binary_object/scalar_script.rs \
    $'#[derive(Clone, Debug, Eq, PartialEq)]\npub(in crate::runtime) enum ScalarValueDraft' \
    $'#[derive(Clone, Copy, Debug, Eq, PartialEq)]\npub(in crate::runtime) enum ScalarValueDraft'
expect_rewrite_rejected scalar-unary-chain-storage scalar-script-sequence-shape \
    src/runtime/binary_object/scalar_script.rs \
    '    unary_ops: Box<[ScalarUnaryOp]>,' \
    '    unary_ops: Vec<ScalarUnaryOp>,'
expect_rejected scalar-opcode-set-widening scalar-script-opcode-set \
    src/runtime/binary_object/scalar_script.rs \
    'const OP_PUSH_THIS: u8 = 0x08;'
expect_rewrite_rejected scalar-unary-name-widening scalar-unary-operation-shape \
    src/runtime/binary_object/scalar_script.rs \
    '            FunctionUnaryOp::TypeOf => Self::TypeOf,' \
    $'            FunctionUnaryOp::TypeOf => Self::TypeOf,\n            FunctionUnaryOp::Neg => Self::TypeOf,'
