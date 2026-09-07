// Keep the Iterator method oracle implementations in isolated modules while
// Cargo builds one integration target.

#[path = "../support/quickjs_string_result_oracle.rs"]
mod quickjs_string_result_oracle;

#[path = "iterator/oracle_iterator_concat.rs"]
mod oracle_iterator_concat;
#[path = "iterator/oracle_iterator_helpers.rs"]
mod oracle_iterator_helpers;
