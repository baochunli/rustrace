"""Captured Ratatui protocol regressions; no production binary is required."""
import unittest

from pty_screen import rendered_screen


class ScreenTests(unittest.TestCase):
    def test_startup_does_not_fill_skipped_menu_space(self):
        capture = (b"russ toolchain startup text\r\n"
                   b"\x1b[?1049h\x1b[1;1Hnew\x1b[1;5Hfile" + "…".encode())
        screen = rendered_screen(capture)
        self.assertIn("new file…", screen)
        self.assertNotIn("startup", screen)
        self.assertNotIn("newsfile…", screen)

    def test_captured_delete_confirmation(self):
        screen = rendered_screen(b"\x1b[15;37HDelete\x1b[15;44Hother.rs?")
        self.assertIn("Delete other.rs?", screen)

    def test_captured_discard_confirmation(self):
        screen = rendered_screen(b"\x1b[15;37HDiscard\x1b[15;45Hunsaved"
                                 b"\x1b[15;53Hbuffer\x1b[15;60Hchanges?")
        self.assertIn("Discard unsaved buffer changes?", screen)

    def test_captured_unicode_positions(self):
        capture = ("\x1b[6;27HE東\x1b[6;30H京\x1b[6;33Hé"
                   "\x1b[6;35H👩🏽‍🔬\x1b[6;39Hend").encode()
        self.assertIn("E東京 é 👩🏽‍🔬  end", rendered_screen(capture))

    def test_unicode_advances_by_cells_not_codepoints(self):
        screen = rendered_screen("東京 é 👩🏽‍🔬!\x1b[1;11Hend".encode())
        self.assertIn("東京 é 👩🏽‍🔬!end", screen)

    def test_style_splits_do_not_insert_spaces(self):
        self.assertIn("rename file", rendered_screen(
            b"re\x1b[31mname\x1b[0m\x1b[1;8Hfile"))

    def test_overwritten_prompt_is_not_a_match(self):
        self.assertNotIn("Delete other.rs?", rendered_screen(
            b"Delete other.rs?\x1b[1;1H\x1b[2Kcancelled"))

    def test_clear_display_removes_stale_prompt(self):
        self.assertNotIn("Discard unsaved buffer changes?", rendered_screen(
            b"Discard unsaved buffer changes?\x1b[2J"))

    def test_second_alternate_screen_starts_empty(self):
        self.assertNotIn("Delete other.rs?", rendered_screen(
            b"\x1b[?1049hDelete other.rs?\x1b[?1049l"
            b"normal output\x1b[?1049hnew screen"))

    def test_exit_preserves_last_ui_not_wrapper_output(self):
        screen = rendered_screen(b"\x1b[?1049hDelete other.rs?\x1b[?1049l"
                                 b"\r\nTERMINAL_RESTORED")
        self.assertIn("Delete other.rs?", screen)
        self.assertNotIn("TERMINAL_RESTORED", screen)

    def test_cursor_gap_cannot_manufacture_contiguous_literal(self):
        self.assertNotIn("Delete other.rs?", rendered_screen(
            b"Delete\x1b[1;20Hother.rs?"))

    def test_different_rows_cannot_manufacture_literal(self):
        self.assertNotIn("Delete other.rs?", rendered_screen(
            b"Delete\x1b[2;8Hother.rs?"))

    def test_overwriting_wide_continuation_removes_old_glyph(self):
        screen = rendered_screen("東\x1b[1;2Hx".encode())
        self.assertTrue(screen.startswith(" x"))
        self.assertNotIn("東", screen)

    def test_erasing_wide_continuation_removes_old_glyph(self):
        self.assertNotIn("東", rendered_screen("東\x1b[1;2H\x1b[K".encode()))

    def test_relative_positions_and_line_erasure(self):
        screen = rendered_screen(b"stale\rnew\x1b[K\n\x1b[3Gx\x1b[D!\x1b[A?")
        self.assertEqual([line.rstrip() for line in screen.splitlines()[:2]],
                         ["new?", "  !"])

    def test_incomplete_frame_is_not_visible_text(self):
        for tail in [b"\x1b", b"\x1b[", b"\x1b[15;37"]:
            with self.subTest(tail=tail):
                self.assertTrue(rendered_screen(b"Delete" + tail).startswith("Delete "))

    def test_combining_cluster_bound_is_explicit(self):
        with self.assertRaises(ValueError):
            rendered_screen(("e" + "\u0301" * 64).encode())

    def test_unsupported_escape_payload_is_not_visible_text(self):
        with self.assertRaises(ValueError):
            rendered_screen(b"\x1b]0;Delete other.rs?\x07")

    def test_captured_clipboard_write_does_not_change_cells(self):
        self.assertTrue(rendered_screen(b"A\x1b]52;c;QQ==\x07b").startswith("Ab "))

    def test_clipboard_payload_cannot_become_visible_prompt(self):
        screen = rendered_screen(b"\x1b]52;c;RGVsZXRlIG90aGVyLnJzPw==\x07")
        self.assertNotIn("Delete other.rs?", screen)
        self.assertNotIn("RGVsZXRl", screen)

    def test_incomplete_clipboard_frame_is_not_visible_text(self):
        for tail in [b"\x1b]", b"\x1b]52;", b"\x1b]52;c;QQ=="]:
            with self.subTest(tail=tail):
                self.assertTrue(rendered_screen(b"A" + tail).startswith("A "))

    def test_raw_control_cannot_manufacture_literal(self):
        with self.assertRaises(ValueError):
            rendered_screen(b"Delete \x00other.rs?")

    def test_requested_dimensions_bound_output(self):
        screen = rendered_screen(b"\x1b[999999;999999Hhidden", rows=2, columns=8)
        self.assertEqual(screen, "        \n        ")

    def test_capture_bound_is_explicit(self):
        with self.assertRaises(ValueError):
            rendered_screen(b"x" * (2 * 1024 * 1024 + 1))

    def test_dimensions_bound_is_explicit(self):
        for rows, columns in [(0, 80), (24, 0), (81, 80), (24, 201)]:
            with self.subTest(rows=rows, columns=columns):
                with self.assertRaises(ValueError):
                    rendered_screen(b"", rows=rows, columns=columns)


if __name__ == "__main__":
    unittest.main()
