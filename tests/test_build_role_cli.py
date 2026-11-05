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
        patcher = mock.patch.object(cli, "run")
        self.version_run = patcher.start()
        self.addCleanup(patcher.stop)

    def run_flash(self, outputs, *, speed=100, probe='2e8a:000c:serial'):
        stream = io.StringIO()
        with mock.patch.object(cli, 'find_tool', return_value='/tool/probe-rs'), mock.patch.object(cli.subprocess, 'Popen', side_effect=outputs) as popen, contextlib.redirect_stdout(stream):
            cli.flash_with_probe(Path('/build/client.elf'), probe, speed)
        return popen.call_args_list, stream.getvalue()

    def test_version_and_executable_are_reported_before_flash(self):
        self.version_run.side_effect = lambda *args, **kwargs: print("probe-rs 0.32.0")
        _, output = self.run_flash([ProbeProcess(), ProbeProcess()])
        self.assertIn("probe-rs 0.32.0", output)
        self.version_run.assert_called_once_with(['/tool/probe-rs', '--version'], cwd=cli.ROOT)
        self.assertTrue(output.startswith('Using probe-rs executable: /tool/probe-rs'))

    def test_zero_exit_reset_cleanup_warning_retains_status_and_raw_diagnostic(self):
        for warning in ('Could not clear all hardware breakpoints: DP Multidrop(285223207)',
                        'Failed to deconfigure device during shutdown: target did not respond'):
            calls, output = self.run_flash([ProbeProcess('Verified\n'), ProbeProcess('WARN ' + warning + '\n')])
            self.assertEqual(len(calls), 2, 'no retry or reflash')
            self.assertIn(warning, output)
            self.assertIn('cleanup failure after reset', output)
            self.assertIn('reset command returned zero', output)
            self.assertNotIn('ELF download verified; reset command returned success.', output)

    def test_zero_exit_download_cleanup_warning_is_classified_separately(self):
        calls, output = self.run_flash([ProbeProcess('WARN Could not clear all hardware breakpoints\n'), ProbeProcess()])
        self.assertEqual(len(calls), 2)
        self.assertIn('download-session cleanup reported a failure', output)
        self.assertNotIn('cleanup failure after reset', output)

    def test_verified_download_and_reset_share_exact_probe_speed(self):
        calls, output = self.run_flash([ProbeProcess('Verified\n'), ProbeProcess('Reset complete\n')])
        common = ['--chip', 'RP2040', '--protocol', 'swd', '--non-interactive', '--probe', '2e8a:000c:serial', '--speed', '100']
        self.assertEqual(calls[0].args[0], ['/tool/probe-rs', 'download', '/build/client.elf', *common, '--verify'])
        self.assertEqual(calls[1].args[0], ['/tool/probe-rs', 'reset', *common])
        self.assertIn('ELF download verified; reset command returned success.', output)
        self.assertIn('startup and bridge connectivity have not been checked', output)
        self.assertNotIn('Flashed over SWD', output)

    def test_unspecified_speed_preserves_probe_default(self):
        calls, _ = self.run_flash([ProbeProcess(), ProbeProcess()], speed=None, probe=None)
        self.assertTrue(all('--speed' not in call.args[0] and '--probe' not in call.args[0] for call in calls))

    def test_warning_on_either_stage_is_preserved_and_not_clean_success(self):
        for index in (0, 1):
            results = [ProbeProcess(), ProbeProcess()]
            results[index] = ProbeProcess('\x1b[33mWARN\x1b[0m SWD speed was adjusted\n')
            _, output = self.run_flash(results)
            self.assertIn('SWD speed was adjusted', output)
            self.assertIn('emitted diagnostics above', output)
            self.assertNotIn('ELF download verified; reset command returned success.', output)

    def test_download_failure_never_resets_or_reports_success(self):
        stream = io.StringIO()
        with mock.patch.object(cli, 'find_tool', return_value='probe-rs'), mock.patch.object(cli.subprocess, 'Popen', return_value=ProbeProcess('Target not responding\n', 7)) as popen, contextlib.redirect_stdout(stream), self.assertRaises(SystemExit) as error:
            cli.flash_with_probe(Path('firmware'), None, 100)
        self.assertEqual(error.exception.code, 7)
        self.assertEqual(popen.call_count, 1)
        self.assertIn('Target not responding', stream.getvalue())
        self.assertNotIn('verified', stream.getvalue())

    def test_reset_failure_preserves_exit_and_download_only_message(self):
        stream = io.StringIO()
        with mock.patch.object(cli, 'find_tool', return_value='probe-rs'), mock.patch.object(cli.subprocess, 'Popen', side_effect=[ProbeProcess(), ProbeProcess('Reset failed\n', 9)]), contextlib.redirect_stdout(stream), self.assertRaises(SystemExit) as error:
            cli.flash_with_probe(Path('firmware'), None)
        self.assertEqual(error.exception.code, 9)
        self.assertIn('verified the ELF download', stream.getvalue())
        self.assertNotIn('reset command returned success', stream.getvalue())

    def test_stream_output_arrives_before_process_wait(self):
        output = io.StringIO()
        process = ProbeProcess('live diagnostic\n')
        def wait():
            self.assertIn('live diagnostic', output.getvalue())
            return 0
        process.wait = wait
        with mock.patch.object(cli.subprocess, 'Popen', return_value=process), contextlib.redirect_stdout(output):
            self.assertFalse(cli.run_probe_command(['probe-rs', 'download']))

    def test_probe_start_failure_is_nonzero(self):
        with mock.patch.object(cli.subprocess, 'Popen', side_effect=OSError('unavailable')), self.assertRaises(SystemExit) as error:
            cli.run_probe_command(['probe-rs'])
        self.assertIn('could not run probe-rs', str(error.exception))

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
