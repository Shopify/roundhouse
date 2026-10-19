//! The fractal query runtime (`src/emit/fractal/fractal.rb`)
//! against real Active Record: `tests/support/fractal_runtime_test.rb`.
//! Each case checks that a specialized site gives the original chain's
//! answer, and that the fast path ran or that the runtime stepped aside
//! and said why: empty tables, transactions, `scoping`, reloads, finders
//! and association readers the app redefines, placeholders out of SQL
//! order, relations that cannot be built, and errors.
//!
//! Needs `activerecord` (~> 8.1) and `sqlite3`, which CI's unit job installs.

#[test]
fn query_runtime_matches_active_record_and_counts_the_fast_path() {
    let root = env!("CARGO_MANIFEST_DIR");
    let output = std::process::Command::new("ruby")
        .arg(format!("{root}/tests/support/fractal_runtime_test.rb"))
        .arg(format!("{root}/src/emit/fractal/fractal.rb"))
        .output()
        .expect("ruby must be on PATH");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
