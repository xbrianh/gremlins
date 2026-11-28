"""Tests for the all-file physical line gate; no third-party dependencies."""

import contextlib
import importlib.util
import io
from pathlib import Path
import subprocess
import tempfile
import unittest

spec = importlib.util.spec_from_file_location(
    "session_chat_lines", Path(__file__).with_name("check-session-chat-lines.py"))
gate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gate)


class LineGateTests(unittest.TestCase):
    def test_path_classification(self):
        for path in ("src/chat_tests.rs", "src/tests.rs", "tests/chat.rs",
                     "src/tests/helpers.rs", "docs/test_session_chat_lines.py"):
            with self.subTest(path=path):
                self.assertTrue(gate.is_test_only(path))
        # These may contain inline tests: the entire file stays non-test.
        for path in ("src/tui/app.rs", "src/tui/mod.rs", "src/tui/screen.rs",
                     "src/tui/app/conversations.rs", "src/test_support.rs",
                     "src/contests/chat.rs", "src/tests_helper.rs", "Makefile",
                     "docs/check-session-chat-lines.py", "tests/README.md",
                     "docs/testing.py", "docs/session-chat-migration-gate.md"):
            with self.subTest(path=path):
                self.assertFalse(gate.is_test_only(path))

    def status(self, records):
        with contextlib.redirect_stdout(io.StringIO()) as output:
            status = gate.report(gate.totals(records))
        self.assertIn("added=", output.getvalue())
        self.assertIn("deleted=", output.getvalue())
        self.assertIn("net=", output.getvalue())
        return status

    def test_test_savings_cannot_offset_non_test_growth(self):
        self.assertEqual(self.status([
            ("src/app.rs", 2, 1), ("src/app_tests.rs", 0, 100)]), 1)

    def test_zero_fails_strict_gate(self):
        self.assertEqual(self.status([]), 1)
        self.assertEqual(self.status([
            ("src/app.rs", 2, 2), ("tests/chat.rs", 0, 100)]), 1)

    def test_test_growth_does_not_block_non_test_reduction(self):
        self.assertEqual(self.status([
            ("src/app.rs", 1, 2), ("tests/chat.rs", 100, 0)]), 0)

    def test_git_fixture_includes_untracked_and_staged_without_double_count(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)

            def git(*args):
                return subprocess.check_output(
                    ["git", "-C", directory, *args], stderr=subprocess.STDOUT)

            git("init", "-q")
            (root / "mixed.rs").write_text(
                "fn production() {}\n#[cfg(test)]\nmod tests {}\n")
            (root / "deleted.txt").write_text("removed\n")
            git("add", ".")
            git("-c", "user.name=Fixture", "-c", "user.email=fixture@example.com",
                "commit", "-qm", "baseline")
            baseline = git("rev-parse", "HEAD").decode().strip()
            (root / "mixed.rs").write_text("fn production() {}\n")
            (root / "staged.rs").write_text("// staged\n")
            git("add", ".")
            (root / "staged.rs").write_text("// staged\n// unstaged\n")
            (root / "new file.rs").write_bytes(b"// new\n\n// no final newline")
            (root / "empty.rs").write_bytes(b"")
            (root / "chat_tests.rs").write_text("// test\n")
            (root / "ignored.rs").write_text("// ignored\n")
            (root / ".gitignore").write_text("ignored.rs\n")
            (root / "not-rust.txt").write_text("not Rust\n")
            (root / "Makefile").write_text("check:\n")
            (root / "check.py").write_text("# checker\n")
            (root / "docs").mkdir()
            (root / "docs/gate.md").write_text("# gate\n")
            (root / "test_gate.py").write_text("# test\n")
            (root / "deleted.txt").unlink()
            records = list(gate.changes(root, baseline))
            self.assertEqual(set(records), {
                ("mixed.rs", 0, 2), ("staged.rs", 2, 0),
                ("new file.rs", 3, 0), ("empty.rs", 0, 0),
                ("chat_tests.rs", 1, 0), (".gitignore", 1, 0),
                ("not-rust.txt", 1, 0), ("Makefile", 1, 0),
                ("check.py", 1, 0), ("docs/gate.md", 1, 0),
                ("test_gate.py", 1, 0), ("deleted.txt", 0, 1),
            })
            self.assertEqual(gate.totals(records), {
                "non-test": [10, 3], "Rust test-only": [1, 0],
                "Python test-only": [1, 0]})

    def test_python_savings_cannot_offset_non_test_growth(self):
        self.assertEqual(self.status([
            ("Makefile", 2, 1), ("test_gate.py", 0, 100)]), 1)

    def test_binary_changes_fail_explicitly(self):
        for tracked in (False, True):
            with self.subTest(tracked=tracked), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                gate.git(root, "init", "-q")
                (root / "file.bin").write_bytes(b"old\0data")
                gate.git(root, "add", ".")
                gate.git(root, "-c", "user.name=Fixture", "-c",
                         "user.email=fixture@example.com", "commit", "-qm", "base")
                name = "file.bin" if tracked else "untracked.bin"
                (root / name).write_bytes(b"new\0data")
                with self.assertRaisesRegex(ValueError, "cannot count binary file: " + name):
                    list(gate.changes(root, "HEAD"))


if __name__ == "__main__":
    unittest.main()
