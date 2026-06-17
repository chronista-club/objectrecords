//! Decision #5: the `State` trait is sealed — no downstream crate may
//! invent additional state markers that bypass the immutability guarantees.
//!
//! `tests/*.rs` are compiled as independent (external) crates by Cargo, so
//! this file exercises the sealed-trait protection from outside the crate.

use objectrecords_core::State;

struct RogueState;

// The line below MUST NOT compile (sealed trait).
impl State for RogueState {}

fn main() {}
