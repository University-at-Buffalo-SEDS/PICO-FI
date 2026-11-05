import contextlib
import importlib.util
import io
import os
from pathlib import Path
import subprocess
import unittest
from unittest import mock

SPEC = importlib.util.spec_from_file_location('pico_build_role', Path(__file__).resolve().parents[1] / 'build-role.py')
cli = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(cli)
import build_support as flashing


class ProbeProcess:
    def __init__(self, output='', status=0):
        self.stdout = io.StringIO(output)
        self.status = status
    def __enter__(self):
        return self
    def __exit__(self, *_):
        self.stdout.close()
    def wait(self):
        return self.status


class BuildRoleCliTests(unittest.TestCase):
    def setUp(self):
        # --version is a local executable query; keep even that mocked here.
        patcher = mock.patch.object(flashing, "run")
        self.version_run = patcher.start()
        self.addCleanup(patcher.stop)

    def run_flash(self, outputs, *, speed=100, probe='2e8a:000c:serial'):
        stream = io.StringIO()
        with mock.patch.object(flashing, 'find_tool', return_value='/tool/probe-rs'), mock.patch.object(flashing.subprocess, 'Popen', side_effect=outputs) as popen, contextlib.redirect_stdout(stream):
            cli.flash_with_probe(Path('/build/client.elf'), probe, speed)
        return popen.call_args_list, stream.getvalue()

    def test_version_and_executable_are_reported_before_flash(self):
        self.version_run.side_effect = lambda *args, **kwargs: print("probe-rs 0.32.0")
        _, output = self.run_flash([ProbeProcess()])
        self.assertIn("probe-rs 0.32.0", output)
        self.version_run.assert_called_once_with(['/tool/probe-rs', '--version'], cwd=cli.ROOT)
        self.assertTrue(output.startswith('Using probe-rs executable: /tool/probe-rs'))

    def test_verified_download_and_reset_use_one_session_with_probe_and_speed(self):
        calls, output = self.run_flash([ProbeProcess('Verified\n')])
        self.assertEqual(len(calls), 1, 'no second attach/reset session')
        self.assertEqual(calls[0].args[0], [
            '/tool/probe-rs', 'download', '/build/client.elf', '--chip', 'RP2040',
            '--protocol', 'swd', '--non-interactive', '--probe', '2e8a:000c:serial',
            '--speed', '100', '--verify', '--reset'])
        self.assertIn('ELF download verified; reset command returned success.', output)
        self.assertIn('startup and bridge connectivity have not been checked', output)

    def test_cleanup_warnings_remain_visible_and_are_not_clean_success(self):
        for warning in ('Could not clear all hardware breakpoints: DP Multidrop(285223207)',
                        'Failed to deconfigure device during shutdown: target did not respond'):
            calls, output = self.run_flash([ProbeProcess('WARN ' + warning + '\n')])
            self.assertEqual(len(calls), 1)
            self.assertIn(warning, output)
            self.assertIn('debug-session cleanup failure', output)
            self.assertNotIn('ELF download verified; reset command returned success.', output)

    def test_other_diagnostics_remain_visible(self):
        _, output = self.run_flash([ProbeProcess('\x1b[33mWARN\x1b[0m SWD speed was adjusted\n')])
        self.assertIn('SWD speed was adjusted', output)
        self.assertIn('emitted diagnostics above', output)

    def test_unspecified_speed_preserves_probe_default(self):
        calls, _ = self.run_flash([ProbeProcess()], speed=None, probe=None)
        self.assertNotIn('--speed', calls[0].args[0])
        self.assertNotIn('--probe', calls[0].args[0])

    def test_download_or_reset_failure_preserves_exit_and_never_reports_success(self):
        with mock.patch.object(flashing, 'find_tool', return_value='probe-rs'), mock.patch.object(flashing.subprocess, 'Popen', return_value=ProbeProcess('Reset failed\n', 7)) as popen, contextlib.redirect_stdout(io.StringIO()) as output, self.assertRaises(SystemExit) as error:
            cli.flash_with_probe(Path('firmware'), None, 100)
        self.assertEqual(error.exception.code, 7)
        self.assertEqual(popen.call_count, 1)
        self.assertNotIn('returned success', output.getvalue())

    def test_stream_output_arrives_before_process_wait(self):
        output = io.StringIO()
        process = ProbeProcess('live diagnostic\n')
        def wait():
            self.assertIn('live diagnostic', output.getvalue())
            return 0
        process.wait = wait
        with mock.patch.object(flashing.subprocess, 'Popen', return_value=process), contextlib.redirect_stdout(output):
            self.assertFalse(flashing.run_probe_command(['probe-rs', 'download']))

    def test_probe_start_failure_is_nonzero(self):
        with mock.patch.object(flashing.subprocess, 'Popen', side_effect=OSError('unavailable')), self.assertRaises(SystemExit) as error:
            flashing.run_probe_command(['probe-rs'])
        self.assertIn('could not run probe-rs', str(error.exception))

    def test_both_entry_points_use_shared_flash_and_conservative_speed_default(self):
        spec = importlib.util.spec_from_file_location('pico_build', cli.ROOT / 'build.py')
        build = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(build)
        self.assertIs(build.flash_with_probe, cli.flash_with_probe)
        with mock.patch.dict(os.environ, {}, clear=True):
            self.assertEqual(cli.parse_args(['client']).speed, 100)
            self.assertEqual(build.parse_args(['--config', 'pico-fi-gateway-ethernet.json']).speed, 100)
        with mock.patch.dict(os.environ, {'PROBE_RS_SPEED': '500', 'PROBE_RS_PROBE': 'chosen'}, clear=True):
            args = build.parse_args(['--flash'])
            self.assertEqual((args.speed, args.probe), (500, 'chosen'))
            self.assertEqual(build.parse_args(['--speed', '200']).speed, 200)
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
            build.parse_args(['--speed', '0'])

    def test_speed_and_probe_env_defaults_and_explicit_overrides(self):
        with mock.patch.dict(os.environ, {'PROBE_RS_SPEED': '100', 'PROBE_RS_PROBE': 'env-probe'}, clear=True):
            args = cli.parse_args(['--flash', '--release', 'client'])
            self.assertEqual((args.speed, args.probe, args.role), (100, 'env-probe', 'client'))
            args = cli.parse_args(['server', '--probe', 'chosen', '--speed', '500'])
            self.assertEqual((args.speed, args.probe), (500, 'chosen'))

    def test_reject_invalid_speed_before_build_or_hardware(self):
        for value in ('0', '-1', '1.5', 'oops'):
            with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
                cli.parse_args(['client', '--speed', value])

    def test_role_config_and_default_build_flow_unchanged(self):
        for role in ('client', 'server'):
            with mock.patch.object(cli, 'find_tool', side_effect=lambda name: '/tool/' + name), mock.patch.object(cli, 'run') as run, mock.patch.object(cli.Path, 'is_file', return_value=True), mock.patch.object(cli, 'flash_with_probe') as flash, mock.patch.dict(os.environ, {}, clear=True), contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(cli.build_role(role, release=True, flash=True, probe='chosen', speed=100), 0)
                self.assertEqual(run.call_count, 2)
                self.assertEqual(run.call_args_list[0].kwargs['env']['PICO_FI_CONFIG'], str(cli.ROOT / ('pico-fi-' + role + '.json')))
                self.assertEqual(run.call_args_list[0].kwargs['env']['CARGO_TARGET_DIR'], str(cli.ROOT / 'target' / role))
                self.assertIn('--release', run.call_args_list[0].args[0])
                flash.assert_called_once_with(cli.ROOT / 'target' / role / cli.TARGET / 'release' / cli.BIN_NAME, 'chosen', 100)

    def test_config_override_is_retained(self):
        with mock.patch.object(cli, 'find_tool', return_value='/tool'), mock.patch.object(cli, 'run') as run, mock.patch.object(cli.Path, 'is_file', return_value=True), mock.patch.dict(os.environ, {'PICO_FI_CONFIG': 'pico-fi-gateway-ethernet.json'}, clear=True), contextlib.redirect_stdout(io.StringIO()):
            cli.build_role('client', release=True, flash=False, probe=None)
            self.assertEqual(run.call_args_list[0].kwargs['env']['PICO_FI_CONFIG'], str(cli.ROOT / 'pico-fi-gateway-ethernet.json'))


if __name__ == '__main__':
    unittest.main()
