// Keep the eval oracle implementations in isolated modules while Cargo builds
// one integration target.

#[path = "eval/oracle_eval_intrinsic.rs"]
mod oracle_eval_intrinsic;
#[path = "eval/oracle_eval_var_destructuring.rs"]
mod oracle_eval_var_destructuring;
#[path = "eval/oracle_eval_wtf8_source.rs"]
mod oracle_eval_wtf8_source;
