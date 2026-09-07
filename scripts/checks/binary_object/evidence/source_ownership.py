"""Pinned data and expected shapes for source_ownership."""

EXPECTED_MODEL_BYTECODE_IMAGE_METHODS = [('pub(super)', 'new'),
 ('pub(in crate::runtime::binary_object)', 'input_atom_slot_count'),
 ('pub(in crate::runtime)', 'atoms'),
 ('pub(super)', 'nodes'),
 ('pub(in crate::runtime::binary_object)', 'sab_archive_occurrences'),
 ('pub(in crate::runtime)', 'reference_table'),
 ('pub(in crate::runtime)', 'functions'),
 ('pub(in crate::runtime)', 'function'),
 ('pub(in crate::runtime)', 'modules'),
 ('pub(in crate::runtime)', 'module'),
 ('pub(in crate::runtime)', 'root')]

EXPECTED_ATOM_SENSITIVE_VISIBLE_SITES = [('src/runtime/binary_object/bytecode_image/model.rs',
  'pub(in crate::runtime::binary_object)',
  'name_is_null'),
 ('src/runtime/binary_object/bytecode_image/model.rs',
  'pub(in crate::runtime::binary_object)',
  'name_is_pinned_eval')]
