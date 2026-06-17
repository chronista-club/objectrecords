//! trybuild compile_fail entry point.
//!
//! Each `tests/compile_fail/*.rs` is compiled as an independent crate and is
//! expected to fail at type-check. This is how decision #5 (type-state
//! pattern's compile-time guarantees) is pinned as a permanent test instead
//! of a comment.
//!
//! On API changes the expected `.stderr` files may shift. Regenerate with:
//!
//! ```text
//! TRYBUILD=overwrite cargo +1.95.0 test --test compile_fail
//! ```

#[test]
fn type_state_guards_are_compile_failures() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/compile_fail/*.rs");
}
