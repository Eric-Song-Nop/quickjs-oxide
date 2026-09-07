// Keep the Promise oracle implementations in separate modules so their
// private helpers remain isolated while Cargo builds one integration target.

#[path = "promise/oracle_promise_aggregates.rs"]
mod oracle_promise_aggregates;
#[path = "promise/oracle_promise_all.rs"]
mod oracle_promise_all;
#[path = "promise/oracle_promise_finally.rs"]
mod oracle_promise_finally;
#[path = "promise/oracle_promise_jobs.rs"]
mod oracle_promise_jobs;
#[path = "promise/oracle_promise_static.rs"]
mod oracle_promise_static;
