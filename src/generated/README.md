# Generated production data

`unicode/` contains the four checksum-pinned Rust Unicode tables. Handwritten
algorithms remain in `src/unicode*.rs`, which include these files within their
existing private table modules. No generated file expands the public API.

Regenerate with the corresponding `scripts/generate-unicode-*-tables` command
and the pinned QuickJS source, following the source and license information in
each header. Generators default to this directory. Ordinary builds consume the
checked-in tables and do not run the generators or require a reference engine.

To verify an update, generate into a temporary output file and compare it with
the checked-in file; run the Unicode library tests and the normalization
fingerprint check. Test262 metadata belongs in `dev-support/test262/generated/`.
