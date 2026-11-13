import importlib.util
from pathlib import Path
import time
import os
import tempfile
import json
import subprocess
import sys
import unittest

spec = importlib.util.spec_from_file_location('gate', Path(__file__).with_name('bash_compat_gate.py'))
gate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gate)

class GateTests(unittest.TestCase):
    def test_stderr_is_observed(self):
        a = gate.run('/bin/sh', ['-c'], 'printf out; printf a >&2')
        b = gate.run('/bin/sh', ['-c'], 'printf out; printf b >&2')
        self.assertNotEqual(a, b)

    def test_file_content_and_mode_are_observed(self):
        a = gate.run('/bin/sh', ['-c'], 'printf a >result; chmod 600 result', ['result'])
        b = gate.run('/bin/sh', ['-c'], 'printf a >result; chmod 644 result', ['result'])
        self.assertNotEqual(a, b)
        self.assertEqual(a['files']['result']['bytes'], '61')

    def test_timeout_with_inherited_output_is_bounded(self):
        start = time.monotonic()
        with self.assertRaisesRegex(RuntimeError, 'timeout'):
            gate.run('/bin/sh', ['-c'], 'sleep 30 & wait', timeout=.1)
        self.assertLess(time.monotonic() - start, 3)

    def test_background_output_does_not_hang_after_exit(self):
        start = time.monotonic()
        gate.run('/bin/sh', ['-c'], 'sleep 30 & exit 0')
        self.assertLess(time.monotonic() - start, 3)

    def test_cli_rejects_stderr_status_and_file_mismatches(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            fixtures = root / 'cases.json'
            fixtures.write_text(json.dumps([{'name': 'sentinel', 'script': 'printf same; printf original >result', 'files': ['result']}]))
            for mutation in ('printf extra >&2', 'exit 9', 'printf changed >result'):
                with self.subTest(mutation=mutation):
                    wrapper = root / 'shell'
                    wrapper.write_text('#!/bin/sh\n/bin/bash "$@"\n' + mutation + '\n')
                    wrapper.chmod(0o700)
                    result = subprocess.run([sys.executable, str(Path(gate.__file__)),
                                             '--jsh', str(wrapper), '--fixtures', str(fixtures)],
                                            capture_output=True, timeout=5)
                    self.assertEqual(result.returncode, 1, result.stderr)
                    self.assertEqual(len(json.loads(result.stdout)['failures']), 1)

    def test_jsh_job_process_group_is_cleaned(self):
        jsh = os.environ.get('JSH_GATE_BINARY')
        if not jsh:
            self.skipTest('run again after building with JSH_GATE_BINARY')
        with tempfile.TemporaryDirectory() as directory:
            pidfile = Path(directory) / 'pid'
            with self.assertRaisesRegex(RuntimeError, 'timeout'):
                gate.run(jsh, ['--norc', '-c'],
                         f"sh -c 'echo $$ >{pidfile}; exec sleep 30'", timeout=.3)
            pid = pidfile.read_text().strip()
            deadline = time.monotonic() + 2
            while time.monotonic() < deadline:
                try:
                    state = Path('/proc', pid, 'stat').read_text().rsplit(')', 1)[1].split()[0]
                except FileNotFoundError:
                    return
                if state == 'Z':
                    return
                time.sleep(.01)
            self.fail('jsh job survived cleanup')

    def test_output_limit_is_failure_not_matching_result(self):
        with self.assertRaisesRegex(RuntimeError, 'output limit'):
            gate.run('/bin/sh', ['-c'], 'while :; do printf xxxxxxxxxxxxxxxxxxxx; done', output_limit=100)

if __name__ == '__main__':
    unittest.main()
