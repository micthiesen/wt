#!/usr/bin/env python3
"""Unit tests for the shared native check command; no native processes run."""

import importlib.util
from pathlib import Path
import subprocess
import sys
import unittest

spec = importlib.util.spec_from_file_location("native_check", Path(__file__).with_name("native-check.py"))
runner = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = runner
spec.loader.exec_module(runner)


class NativeCheckTests(unittest.TestCase):
    def test_complete_order_and_platform_selection(self):
        linux = runner.plan(Path("/candidate/wt"), Path("/diagnostics"), "Linux")
        expected = ["install", "release", "ui", "ui", "session", "lifecycle", "cleanup",
                    "resource-cleanup", "dev", "actions", "remote", "host", "command", "stages", "ui"]
        self.assertEqual([check.args[1] for check in linux],
                         [f"scripts/native-{name}-check.py" for name in expected])
        mac = runner.plan(Path("/candidate/wt"), Path("/diagnostics"), "Darwin")
        self.assertEqual(mac[:-1], linux)
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
        self.assertIn("--create", checks[-1].args)
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
