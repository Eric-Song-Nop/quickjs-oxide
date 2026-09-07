expect_rejected image-public-module image-module-visibility \
    src/runtime/binary_object/bytecode_image/mod.rs \
    'pub(in crate::runtime) mod leaked;'
expect_rejected common-cursor-u64 forbidden-common-cursor-capability \
    src/runtime/binary_object/read_cursor.rs \
    'fn read_u64_le(&mut self) -> u64 { 0 }'
expect_rejected common-cursor-sab-hook forbidden-common-cursor-capability \
    src/runtime/binary_object/read_cursor.rs \
    'fn allows_shared_array_buffers(&self) -> bool { true }'
expect_rejected common-cursor-extra-impl common-cursor-implementation-set \
    src/runtime/binary_object/read_cursor.rs \
    "impl<'input> CheckedReadCursor<'input> for ThirdCursor<'input> {}"
expect_rejected common-cursor-nongeneric-impl common-cursor-implementation-set \
    src/runtime/binary_object/read_cursor.rs \
    "impl CheckedReadCursor<'static> for ThirdCursor {}"
expect_rejected common-cursor-qualified-impl common-cursor-implementation-set \
    src/runtime/binary_object/read_cursor.rs \
    "impl<'input> CheckedReadCursor<'input> for crate::ThirdCursor<'input> {}"
expect_rejected common-cursor-turbofish-impl common-cursor-implementation-set \
    src/runtime/binary_object/read_cursor.rs \
    "impl CheckedReadCursor::<'static> for ThirdCursor {}"
expect_rejected common-cursor-aliased-impl common-cursor-trait-alias \
    src/runtime/binary_object/read_cursor.rs \
    "use self::CheckedReadCursor as Alias; impl Alias<'static> for ThirdCursor {}"
expect_rejected common-cursor-cross-file-alias common-cursor-trait-alias \
    src/runtime/binary_object/atoms.rs \
    "use super::read_cursor::CheckedReadCursor as Alias; impl Alias<'static> for ThirdCursor {}"
expect_rejected common-cursor-extra-seal common-cursor-seal-implementation-set \
    src/runtime/binary_object/read_cursor.rs \
    "impl sealed::Sealed for ThirdCursor {}"
expect_rejected common-cursor-turbofish-seal common-cursor-seal-implementation-set \
    src/runtime/binary_object/read_cursor.rs \
    "impl sealed::Sealed::<> for ThirdCursor {}"
expect_rejected common-cursor-aliased-seal common-cursor-seal-alias \
    src/runtime/binary_object/read_cursor.rs \
    "use self::sealed::Sealed as Seal; impl Seal for ThirdCursor {}"
expect_rejected retired-sab-permit retired-sab-permit \
    src/runtime/binary_object/atoms.rs \
    'struct GraphSabDecodePermit;'
expect_rejected sab-native-token-production-api sab-native-token-implementation-set \
    src/runtime/binary_object/graph/sab_transport.rs \
    'impl NativeSabToken { pub(in crate::runtime) const fn bits(&self) -> u64 { self.native_token_bits } }'
expect_rewrite_rejected sab-native-token-public-field sab-native-token-shape \
    src/runtime/binary_object/graph/sab_transport.rs \
    '    native_token_bits: u64,' \
    '    pub(in crate::runtime) native_token_bits: u64,'
expect_rewrite_rejected sab-native-token-derive sab-native-token-derive \
    src/runtime/binary_object/graph/sab_transport.rs \
    'pub(in crate::runtime) struct NativeSabToken {' \
    '#[derive(Default)] pub(in crate::runtime) struct NativeSabToken {'
expect_rejected sab-extra-cursor-build sab-archive-call-site-set \
    src/runtime/binary_object/graph/sab_transport.rs \
    'fn leak_build(input: SabTransportInput) { let _ = input.build_cursor(); }'
expect_rewrite_rejected sab-test-cursor-build-wrapper sab-input-implementation-set \
    src/runtime/binary_object/graph/sab_transport.rs \
    '        self.build_cursor(mode, wire_limits, graph_limits)' \
    '        audit_build_cursor(self, mode, wire_limits, graph_limits)'
expect_rejected sab-cursor-build-function-item sab-archive-call-site-set \
    src/runtime/binary_object/graph/sab_transport.rs \
    'fn leak_build_item() { let _ = SabTransportInput::build_cursor; }'
expect_rejected sab-image-finalizer-function-item sab-archive-call-site-set \
    src/runtime/binary_object/graph/sab_transport.rs \
    'fn leak_finish_item() { let _ = SabTransportCursor::finish_bytecode_image; }'
expect_rejected sab-graph-finalizer-function-item sab-archive-call-site-set \
    src/runtime/binary_object/graph/sab_transport.rs \
    'fn leak_graph_finish_item() { let _ = SabTransportCursor::finish_graph_archive; }'
expect_rejected sab-cursor-type-alias sab-cursor-alias \
    src/runtime/binary_object/graph/sab_transport.rs \
    "type r#CursorAlias<'a> = SabTransportCursor<'a>;"
expect_rejected sab-cursor-extra-impl sab-cursor-implementation-set \
    src/runtime/binary_object/graph/sab_transport.rs \
    "impl SabTransportCursor<'_> { fn leak(self) {} }"
expect_rejected sab-cursor-extra-literal sab-transport-field-escape \
    src/runtime/binary_object/graph/sab_transport.rs \
    'fn forge_cursor<'"'"'a>(wire: WireCursor<'"'"'a>, occurrences: &'"'"'a [NativeSabToken], archive: SabArchiveState) -> SabTransportCursor<'"'"'a> { SabTransportCursor { cursor_wire: wire, cursor_writer_occurrences: occurrences, cursor_next_occurrence: 0, cursor_archive: archive } }'
expect_rejected sab-cursor-leak-wire sab-transport-field-escape \
    src/runtime/binary_object/graph/sab_transport.rs \
    "impl<'a> SabTransportCursor<'a> { pub(in crate::runtime::binary_object) fn leak_wire(self) -> WireCursor<'a> { self.cursor_wire } }"
expect_rejected sab-input-split sab-transport-field-escape \
    src/runtime/binary_object/graph/sab_transport.rs \
    "impl<'a> SabTransportInput<'a> { pub(in crate::runtime::binary_object) const fn split(&self) -> (&'a [u8], &'a [NativeSabToken]) { (self.transport_wire_bytes, self.transport_writer_occurrences) } }"
expect_rejected sab-graph-owner-detach sab-graph-field-escape \
    src/runtime/binary_object/graph/sab_transport.rs \
    'fn detach_graph(value: ArchivedWireGraph) { let _ = (value.archived_graph_payload, value.archived_graph_shared_backings); }'
expect_rejected sab-graph-extra-impl sab-graph-aggregate-escape \
    src/runtime/binary_object/graph/sab_transport.rs \
    'impl ArchivedWireGraph { fn leak_graph(&self) -> &WireGraph { &self.archived_graph_payload } }'
expect_rewrite_rejected sab-public-graph-field sab-graph-aggregate-shape \
    src/runtime/binary_object/graph/sab_transport.rs \
    '    archived_graph_payload: WireGraph,' \
    '    pub(in crate::runtime::binary_object) archived_graph_payload: WireGraph,'
expect_rejected sab-image-owner-detach sab-image-field-escape \
    src/runtime/binary_object/graph/sab_transport.rs \
    'fn detach(value: ArchivedBytecodeImage) { let _ = value.archived_image_payload; }'
expect_rejected sab-image-external-detach sab-image-field-escape \
    src/runtime/binary_object/atoms.rs \
    'fn detach(value: ArchivedBytecodeImage) { let _ = value.archived_image_shared_backings; }'
expect_rejected sab-image-extra-impl sab-image-aggregate-escape \
    src/runtime/binary_object/graph/sab_transport.rs \
    'impl ArchivedBytecodeImage { fn leak(&self) -> &BytecodeImage { &self.archived_image_payload } }'
expect_rejected sab-image-trait-impl sab-image-aggregate-escape \
    src/runtime/binary_object/graph/sab_transport.rs \
    'impl LeakArchive for ArchivedBytecodeImage { fn leak(self) -> BytecodeImage { self.archived_image_payload } }'
expect_rejected sab-image-raw-alias-literal sab-image-field-escape \
    src/runtime/binary_object/graph/sab_transport.rs \
    'type r#Archive = ArchivedBytecodeImage; fn forge(image: BytecodeImage, table: Box<[SharedBackingDescriptor]>) { let _ = r#Archive { archived_image_payload: image, archived_image_shared_backings: table }; }'
expect_rewrite_rejected sab-image-finalizer-wrapper sab-transport-entrypoint-body \
    src/runtime/binary_object/graph/sab_transport.rs \
    '    cursor.finish_bytecode_image(image).map_err(Into::into)' \
    '    audit_finalize_image(cursor, image).map_err(Into::into)'
expect_rejected sab-extra-transport-function sab-transport-free-function-set \
    src/runtime/binary_object/graph/sab_transport.rs \
    'fn audit_finalize_image() {}'
expect_rejected sab-unicode-transport-function sab-transport-free-function-set \
    src/runtime/binary_object/graph/sab_transport.rs \
    'pub(in crate::runtime::binary_object) fn 完成图归档() {}'
expect_rejected sab-unicode-finalizer-wrapper sab-archive-call-site-set \
    src/runtime/binary_object/graph/sab_transport.rs \
    'pub(in crate::runtime::binary_object) fn 完成图归档(cursor: SabTransportCursor<'"'"'_>, graph: WireGraph) -> Result<ArchivedWireGraph, SabArchiveError> { cursor.finish_graph_archive(graph) }'
expect_rejected sab-finalizer-const-wrapper sab-archive-call-site-set \
    src/runtime/binary_object/graph/sab_transport.rs \
    'pub(in crate::runtime::binary_object) const AUDIT_FINALIZE_GRAPH: for<'"'"'a> fn(SabTransportCursor<'"'"'a>, WireGraph) -> Result<ArchivedWireGraph, SabArchiveError> = |cursor, graph| cursor.finish_graph_archive(graph);'
expect_rejected sab-extra-top-level-const sab-transport-top-level-item-set \
    src/runtime/binary_object/graph/sab_transport.rs \
    'pub(in crate::runtime::binary_object) const AUDIT_ITEM: usize = 0;'
expect_rewrite_rejected sab-associated-finalizer-const sab-archive-call-site-set \
    src/runtime/binary_object/graph/sab_transport.rs \
    '    fn finish_bytecode_image(self, image: BytecodeImage) -> Result<ArchivedBytecodeImage, Error> {' \
    '    const 审计图终结器: for<'"'"'b> fn(SabTransportCursor<'"'"'b>, WireGraph) -> Result<ArchivedWireGraph, SabArchiveError> = |cursor, graph| cursor.finish_graph_archive(graph); fn finish_bytecode_image(self, image: BytecodeImage) -> Result<ArchivedBytecodeImage, Error> {'
expect_rewrite_rejected sab-unicode-cursor-method sab-cursor-method-set \
    src/runtime/binary_object/graph/sab_transport.rs \
    '    fn finish_bytecode_image(self, image: BytecodeImage) -> Result<ArchivedBytecodeImage, Error> {' \
    '    fn 完成图归档(&self) {} fn finish_bytecode_image(self, image: BytecodeImage) -> Result<ArchivedBytecodeImage, Error> {'
expect_rewrite_rejected sab-public-build-cursor sab-transport-private-member \
    src/runtime/binary_object/graph/sab_transport.rs \
    '    fn build_cursor(' \
    '    pub(in crate::runtime::binary_object) fn build_cursor('
expect_rewrite_rejected sab-public-graph-finalizer sab-transport-private-member \
    src/runtime/binary_object/graph/sab_transport.rs \
    '    fn finish_graph_archive(self, graph: WireGraph) -> Result<ArchivedWireGraph, Error> {' \
    '    pub(in crate::runtime::binary_object) fn finish_graph_archive(self, graph: WireGraph) -> Result<ArchivedWireGraph, Error> {'
expect_rewrite_rejected sab-public-image-finalizer sab-transport-private-member \
    src/runtime/binary_object/graph/sab_transport.rs \
    '    fn finish_bytecode_image(self, image: BytecodeImage) -> Result<ArchivedBytecodeImage, Error> {' \
    '    pub(in crate::runtime::binary_object) fn finish_bytecode_image(self, image: BytecodeImage) -> Result<ArchivedBytecodeImage, Error> {'
expect_rewrite_rejected sab-public-cursor-field sab-cursor-shape \
    src/runtime/binary_object/graph/sab_transport.rs \
    '    cursor_next_occurrence: usize,' \
    '    pub(in crate::runtime::binary_object) cursor_next_occurrence: usize,'
expect_rewrite_rejected sab-public-input-field sab-input-shape \
    src/runtime/binary_object/graph/sab_transport.rs \
    '    transport_wire_bytes: &'"'"'a [u8],' \
    '    pub(in crate::runtime::binary_object) transport_wire_bytes: &'"'"'a [u8],'
expect_rewrite_rejected sab-public-image-field sab-image-aggregate-shape \
    src/runtime/binary_object/graph/sab_transport.rs \
    '    archived_image_payload: BytecodeImage,' \
    '    pub(in crate::runtime::binary_object) archived_image_payload: BytecodeImage,'
