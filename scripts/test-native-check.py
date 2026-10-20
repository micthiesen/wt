#!/usr/bin/env python3
"""Unit tests for the shared native check command; no native processes run."""

import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("native_check", Path(__file__).with_name("native-check.py"))
runner = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = runner
spec.loader.exec_module(runner)

ui_spec = importlib.util.spec_from_file_location("native_ui_check", Path(__file__).with_name("native-ui-check.py"))
ui = importlib.util.module_from_spec(ui_spec)
ui_spec.loader.exec_module(ui)


class AnsiScreenTests(unittest.TestCase):
    def test_startup_waits_for_initial_metadata_sample_not_visible_titles(self):
        # CI capture: titles and a blank refresh gutter preceded the first
        # fingerprint by ~1s. Its legitimate refresh was misclassified as idle.
        with tempfile.TemporaryDirectory() as directory:
            log = Path(directory) / "wt-native.log"
            registered = {"target": "wt::freshness", "fields": {
                "message": "filesystem watches registered", "paths": 9}}
            baseline = {"target": "wt::freshness", "fields": {
                "message": "filesystem metadata invalidation", "git_changed": True}}
            log.write_text(json.dumps(registered) + "\n")
            self.assertFalse(ui.startup_metadata_sample_seen([log]))
            log.write_text(json.dumps(registered) + "\n" + json.dumps(baseline)[:40])
            self.assertFalse(ui.startup_metadata_sample_seen([log]))
            log.write_text(json.dumps(registered) + "\n" + json.dumps(baseline) + "\n")
            self.assertTrue(ui.startup_metadata_sample_seen([log]))
        screen = ui.AnsiScreen()
        # Exact refresh-gutter patch from the failed macOS artifact. This must
        # remain forbidden during idle; readiness moves the sample after it.
        patch = (b"\x1b[2;13H\x1b[38;2;116;123;138;48;2;27;29;35m "
                 + "↻".encode() + b"\x1b[38;2;59;66;82;48;2;27;29;35m \x1b[?25l")
        with self.assertRaisesRegex(AssertionError, "non-clock"):
            ui.assert_idle_frames(screen, patch)

    def test_idle_accepts_only_bounded_real_age_ticks(self):
        def fixture(age):
            screen = ui.AnsiScreen(rows=2, columns=80)
            screen.feed(f"│ git committed {age} · created {age} · tail".ljust(79).encode() + "│".encode())
            return screen

        def frame(committed, created, tail="tail"):
            return b"\x1b[1;1H" + f"│ git committed {committed} · created {created} · {tail}".ljust(79).encode() + "│".encode() + b"\x1b[?25l"

        ui.assert_idle_frames(fixture("9s"), b"")
        ui.assert_idle_frames(fixture("9s"), frame("10s", "9s") + frame("10s", "10s"))
        ui.assert_idle_frames(fixture("59s"), frame("1m", "1m"))
        for output in [frame("9s", "9s"), frame("10s", "9s") * 2,
                       frame("10s", "9s") + frame("11s", "9s"),
                       frame("11s", "9s"), frame("10s", "9s", "changed"),
                       b"\x1b[2;1Hunrelated\x1b[?25l", b"\x1b[1;1Hpartial"]:
            with self.subTest(output=output), self.assertRaises(AssertionError):
                ui.assert_idle_frames(fixture("9s"), output)

    def test_creation_differential_frames_reuse_existing_characters(self):
        screen = ui.AnsiScreen(rows=3, columns=60)
        screen.feed(b"\x1b[2;3HWorking\xe2\x80\xa6")
        self.assertTrue(screen.contains("Working"))
        # The next draw reuses 'Work'. The final draw also preserves matching
        # cells, so neither frame contains the complete visible sentence.
        screen.feed(b"\x1b[2;7Htree is still being created")
        self.assertTrue(screen.contains("Worktree is still being created"))
        before = "".join(screen.cells[1])
        after = "  Created created-feedback".ljust(60)
        patch = b"".join(f"\x1b[2;{column + 1}H{new}".encode()
                         for column, (old, new) in enumerate(zip(before, after))
                         if old != new)
        self.assertNotIn(b"Created created-feedback", patch)
        screen.feed(patch)
        self.assertTrue(screen.contains("Created created-feedback"))
        self.assertFalse(screen.contains("Worktree is still being created"))
        screen.feed(b"\x1b[2;3H\x1b[2K")
        self.assertFalse(screen.contains("Created created-feedback"))

    def test_chunked_utf8_glyph_does_not_shift_ascii_cells(self):
        screen = ui.AnsiScreen(rows=2, columns=30)
        data = "⠋ Working…".encode()
        for byte in data:
            screen.feed(bytes([byte]))
        self.assertEqual(screen.column, 10)
        screen.feed(b"\x1b[1;3HCreated\x1b[K")
        self.assertTrue(screen.text().startswith("⠋ Created".encode()))

    def test_wide_and_combining_cells_and_overwriting_continuation(self):
        screen = ui.AnsiScreen(rows=2, columns=20)
        screen.feed("界e\u0301 done".encode())
        self.assertEqual(screen.column, 8)
        self.assertEqual(screen.cells[0][:4], ["界", "", "e\u0301", " "])
        screen.feed(b"\x1b[1;5HDone")
        self.assertTrue(screen.text().startswith("界e\u0301 Done".encode()))
        screen.feed(b"\x1b[1;2HX")
        self.assertTrue(screen.text().startswith(" Xe\u0301 Done".encode()))

    def test_erase_screen_preserves_cursor_and_erase_line_clears_whole_line(self):
        screen = ui.AnsiScreen(rows=3, columns=20)
        screen.feed(b"old\x1b[2;6H\x1b[2Jnew")
        self.assertEqual((screen.row, screen.column), (1, 8))
        self.assertEqual(screen.cells[0], [" "] * 20)
        self.assertEqual("".join(screen.cells[1]), "     new            ")
        screen.feed(b"\x1b[2;7H\x1b[2KX")
        self.assertEqual("".join(screen.cells[1]), "      X             ")
        self.assertEqual((screen.row, screen.column), (1, 7))

    def test_styles_and_chunked_osc_do_not_become_text(self):
        screen = ui.AnsiScreen(rows=2, columns=20)
        for part in [b"\x1b]0;Working", b"\x1b", b"\\", b"\x1b[38;2;1;2;3", b"mReady\x1b[0m"]:
            screen.feed(part)
        self.assertTrue(screen.contains("Ready"))
        self.assertFalse(screen.contains("Working"))


class NativeCheckTests(unittest.TestCase):
    def test_complete_order_and_platform_selection(self):
        linux = runner.plan(Path("/candidate/wt"), Path("/diagnostics"), "Linux")
        expected = ["install", "release", "ui", "ui", "session", "lifecycle", "cleanup",
                    "resource-cleanup", "dev", "actions", "controller", "remote", "host", "command", "stages", "ui", "ui", "workload"]
        self.assertEqual([check.args[1] for check in linux],
                         [f"scripts/native-{name}-check.py" for name in expected])
        mac = runner.plan(Path("/candidate/wt"), Path("/diagnostics"), "Darwin")
        self.assertEqual(mac[:-1], linux[:-1])
        self.assertEqual(mac[-1].args[1:], ("scripts/native-events-check.py", "--binary", "/candidate/wt"))
        with self.assertRaises(ValueError):
            runner.plan(Path("wt"), Path("out"), "Windows")

    def test_binary_environment_and_diagnostic_paths(self):
        checks = runner.plan(Path("/candidate path/wt"), Path("/diagnostic path"), "Linux")
        self.assertEqual(checks[5].env, {"WT_NATIVE_BIN": "/candidate path/wt", "WT_GITHUB": "off"})
        self.assertEqual(checks[6].env["WT_NATIVE_BIN"], "/candidate path/wt")
        self.assertEqual(checks[1].args[-2:], ("--launcher", "/candidate path/wt-launcher"))
        self.assertIn("--sections", checks[2].args)
        self.assertIn("--feeds", checks[2].args)
        self.assertIn("--create", checks[-3].args)
        self.assertIn("--removal", checks[-2].args)
        self.assertEqual(checks[-1].args[-1], "/diagnostic path/wt-native-workload")
        self.assertEqual(checks[3].args[-4:], ("--exit-signal", "TERM", "--output", "/diagnostic path/wt-native-signal"))
        for index, leaf in [(2, "ui"), (4, "session"), (9, "actions")]:
            self.assertEqual(checks[index].args[-1], f"/diagnostic path/wt-native-{leaf}")

    def test_failure_stops_suite_and_inherits_streams_and_environment(self):
        calls = []

        def run(args, **kwargs):
            calls.append((args, kwargs))
            return subprocess.CompletedProcess(args, 23)

        checks = [runner.Check("failure", ("child",), {"WT_GITHUB": "off"}),
                  runner.Check("must not run", ("second",))]
        self.assertEqual(runner.run_checks(checks, {"WT_BUILD_ID": "a" * 40}, run), 23)
        self.assertEqual(len(calls), 1)
        self.assertEqual(calls[0][1], {"cwd": runner.ROOT,
                                     "env": {"WT_BUILD_ID": "a" * 40, "WT_GITHUB": "off"},
                                     "check": False})

    def test_probe_requires_exact_candidate_identity(self):
        expected = "a" * 40
        for output, status, valid in [
            (f"wt-build-id:{expected}:test-target\n", 0, True),
            (f"wt-build-id:{'b' * 40}:test-target\n", 0, False),
            (f"wt-build-id:{expected}:test-target\n", 1, False),
            (f"wt-build-id:{expected}:\n", 0, False),
            ("malformed\n", 0, False),
        ]:
            def probe(args, **kwargs):
                self.assertEqual(args, ("/candidate/wt", "--_boot-probe"))
                self.assertEqual(kwargs["timeout"], 10)
                return subprocess.CompletedProcess(args, status, output, "")
            self.assertEqual(runner.verify_identity(Path("/candidate/wt"), expected, probe), valid)

    def test_probe_timeout_refuses_candidate(self):
        def probe(args, **kwargs):
            raise subprocess.TimeoutExpired(args, kwargs["timeout"])
        self.assertFalse(runner.verify_identity(Path("/candidate/wt"), "a" * 40, probe))

    def test_missing_child_is_an_explicit_failure(self):
        def missing(*args, **kwargs):
            raise FileNotFoundError("missing executable")

        self.assertEqual(runner.run_checks([runner.Check("missing", ("missing",))], {}, missing), 127)

    def test_child_signal_is_a_failure(self):
        self.assertEqual(runner.run_checks([runner.Check("signal", ("child",))], {},
                         lambda args, **kwargs: subprocess.CompletedProcess(args, -15)), 143)


if __name__ == "__main__":
    unittest.main()
