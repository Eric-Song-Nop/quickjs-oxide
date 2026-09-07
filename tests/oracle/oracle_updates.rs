// Keep the update-expression oracle implementations in isolated modules while
// Cargo builds one integration target.

#[path = "update/oracle_update_expressions.rs"]
mod oracle_update_expressions;
#[path = "update/oracle_update_function_constructor.rs"]
mod oracle_update_function_constructor;
#[path = "update/oracle_update_numeric_matrix.rs"]
mod oracle_update_numeric_matrix;
