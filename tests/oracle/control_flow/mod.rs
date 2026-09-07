// Keep the control-flow oracle implementations in isolated modules while
// Cargo builds one integration target.

use crate::quickjs_argv_completion_oracle;
use crate::quickjs_syntax_diagnostic_oracle;
use crate::support::quickjs_control_value_oracle;

mod oracle_annex_b_statements;
mod oracle_catch_destructuring;
mod oracle_for_in;
mod oracle_for_lexicals;
mod oracle_for_of;
mod oracle_statement_control_flow;
mod oracle_switch_control_flow;
mod oracle_try_catch_finally;
mod oracle_with;
