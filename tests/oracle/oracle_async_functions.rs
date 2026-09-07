// Keep the async callable oracle implementations in isolated modules while
// Cargo builds one integration target.

#[path = "async_functions/oracle_async_arrow.rs"]
mod oracle_async_arrow;
#[path = "async_functions/oracle_async_function.rs"]
mod oracle_async_function;
#[path = "async_functions/oracle_async_generator.rs"]
mod oracle_async_generator;
#[path = "async_functions/oracle_async_generator_yield_star.rs"]
mod oracle_async_generator_yield_star;
