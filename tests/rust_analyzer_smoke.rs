use std::ffi::OsStr;

use rustrace::rust_analyzer_spike::run_real_spike;

#[test]
#[ignore = "requires rust-analyzer; run explicitly as documented"]
fn real_rust_analyzer_completes_the_spike() {
    let mut output = Vec::new();

    run_real_spike(None, OsStr::new("rust-analyzer"), &mut output)
        .expect("real rust-analyzer protocol spike");

    let output = String::from_utf8(output).expect("UTF-8 output");
    assert!(output.contains("diagnostics v1:"));
    assert!(output.contains("diagnostics v2:"));
    assert!(output.contains("UTF-16 completion position:"));
    assert!(output.contains("completion:"));
}
