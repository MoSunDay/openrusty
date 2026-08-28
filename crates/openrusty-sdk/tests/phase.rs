//! Compile-fail tests for the `#[phase]` handler contract: handlers must
//! take no parameters and return `Decision` (see `openrusty-macros`).
//!
//! Expected compiler output lives next to each fixture
//! (`tests/ui/phase_fail/*.stderr`); regenerate with `TRYBUILD=overwrite`.

#[test]
fn phase_signature_validation() {
    let t = trybuild::TestCases::new();
    t.pass("tests/ui/phase_pass/*.rs");
    t.compile_fail("tests/ui/phase_fail/*.rs");
}
