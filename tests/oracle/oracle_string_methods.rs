// Keep the String oracle implementations in separate modules so their private
// helpers remain isolated while Cargo builds one integration target.

use crate::quickjs_oracle;

#[path = "string/oracle_string_byte_codec.rs"]
mod oracle_string_byte_codec;
#[path = "string/oracle_string_case.rs"]
mod oracle_string_case;
#[path = "string/oracle_string_conversion_core.rs"]
mod oracle_string_conversion_core;
#[path = "string/oracle_string_create_html.rs"]
mod oracle_string_create_html;
#[path = "string/oracle_string_exotic.rs"]
mod oracle_string_exotic;
#[path = "string/oracle_string_includes.rs"]
mod oracle_string_includes;
#[path = "string/oracle_string_index_search.rs"]
mod oracle_string_index_search;
#[path = "string/oracle_string_intrinsic.rs"]
mod oracle_string_intrinsic;
#[path = "string/oracle_string_match.rs"]
mod oracle_string_match;
#[path = "string/oracle_string_match_all.rs"]
mod oracle_string_match_all;
#[path = "string/oracle_string_pad.rs"]
mod oracle_string_pad;
#[path = "string/oracle_string_repeat.rs"]
mod oracle_string_repeat;
#[path = "string/oracle_string_replace.rs"]
mod oracle_string_replace;
#[path = "string/oracle_string_rope.rs"]
mod oracle_string_rope;
#[path = "string/oracle_string_search.rs"]
mod oracle_string_search;
#[path = "string/oracle_string_split.rs"]
mod oracle_string_split;
#[path = "string/oracle_string_subrange.rs"]
mod oracle_string_subrange;
#[path = "string/oracle_string_trim.rs"]
mod oracle_string_trim;
#[path = "string/oracle_string_utf16_prefix.rs"]
mod oracle_string_utf16_prefix;
