/// A benign doctest exercises the explicitly selected rustdoc.
/// ```
/// assert_eq!(2 + 2, 4);
/// ```
pub fn value() -> u32 {
    4
}

#[test]
fn controlled_test() {
    assert_eq!(value(), 4);
    assert!(std::env::var_os("UNRELATED_SECRET").is_none());
}
