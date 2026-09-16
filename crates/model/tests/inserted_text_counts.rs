use rustrace_model::inserted_text_counts;

#[test]
fn counts_exact_inserted_text_in_unicode_scalars_and_lf_delimited_lines() {
    for (text, characters, lines) in [
        ("", 0, 0),
        ("abc", 3, 1),
        ("é🦀漢", 3, 1),
        ("e\u{301}", 2, 1),
        ("👩‍💻", 3, 1),
        ("\n", 1, 2),
        ("\r\n", 2, 2),
        ("a\r\nb\r\n", 6, 3),
        ("\r", 1, 1),
        ("a\u{2028}b", 3, 1),
    ] {
        let counts = inserted_text_counts(text);
        assert_eq!(counts.character_count, characters, "{text:?}");
        assert_eq!(counts.line_count, lines, "{text:?}");
    }
}
