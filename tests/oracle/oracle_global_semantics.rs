// Keep the global-semantics oracle implementations in isolated modules while
// Cargo builds one integration target.

#[path = "../support/quickjs_plain_eval_oracle.rs"]
mod quickjs_plain_eval_oracle;

#[path = "global/oracle_global_numeric_predicates.rs"]
mod oracle_global_numeric_predicates;
#[path = "global/oracle_global_this.rs"]
mod oracle_global_this;
#[path = "global/oracle_global_to_string_tag.rs"]
mod oracle_global_to_string_tag;
#[path = "global/oracle_global_uri_codecs.rs"]
mod oracle_global_uri_codecs;
