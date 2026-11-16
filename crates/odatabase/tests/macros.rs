//! The derive macros reject mistakes at compile time with messages that say what is wrong.
//! The expected messages are in `tests/ui/*.stderr`; after an intended change, regenerate them
//! with `TRYBUILD=overwrite cargo test -p odatabase --test macros` and review the diff.

#[test]
fn macro_errors() {
    trybuild::TestCases::new().compile_fail("tests/ui/*.rs");
}
