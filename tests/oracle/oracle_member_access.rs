// Keep the member-access oracle implementations in isolated modules while
// Cargo builds one integration target.

#[path = "member_access/oracle_member_reads.rs"]
mod oracle_member_reads;
#[path = "member_access/oracle_member_writes.rs"]
mod oracle_member_writes;
