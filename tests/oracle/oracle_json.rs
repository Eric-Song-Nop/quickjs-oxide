// Keep the JSON oracle implementations in isolated modules while Cargo builds
// one integration target.

#[path = "json/oracle_json_parse.rs"]
mod oracle_json_parse;
#[path = "json/oracle_json_raw.rs"]
mod oracle_json_raw;
#[path = "json/oracle_json_stringify.rs"]
mod oracle_json_stringify;
