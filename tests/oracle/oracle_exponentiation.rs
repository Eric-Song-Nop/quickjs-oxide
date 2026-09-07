// Keep the exponentiation oracle implementations in isolated modules while
// Cargo builds one integration target.

#[path = "exponentiation/oracle_power_bigints.rs"]
mod oracle_power_bigints;
#[path = "exponentiation/oracle_power_numbers.rs"]
mod oracle_power_numbers;
