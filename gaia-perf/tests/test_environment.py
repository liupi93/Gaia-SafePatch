import sys
import tempfile
import unittest
import os
from types import SimpleNamespace
from unittest.mock import patch
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from common import write_json, sha256


class BuildLockTests(unittest.TestCase):
    def test_debug_and_hash_mismatch_cannot_be_used_as_formal_release(self):
        from environment import validate_builds
        with tempfile.TemporaryDirectory() as tmp:
            binary, build = Path(tmp)/'gaia', Path(tmp)/'build.json'
            binary.write_bytes(b'fixture-not-a-real-gaia-binary')
            lock = {'toolchain': {'rustc': 'frozen'}, 'cargo_lock_sha256': 'a'*64}
            config = {'variants': {'original': {'binary': str(binary), 'build_manifest': str(build)}}, 'synthetic': False}
            write_json(build, {'source_commit': 'a'*40, 'profile': 'debug', 'binary_sha256': 'b'*64,
                               'toolchain': lock['toolchain'], 'cargo_lock_sha256': lock['cargo_lock_sha256']})
            errors = '\n'.join(validate_builds(config, [{'variant': 'original'}], lock))
            self.assertIn('profile', errors)
            self.assertIn('binary_sha256', errors)
            write_json(build, {'source_commit': 'a'*40, 'profile': 'release', 'binary_sha256': sha256(binary),
                               'toolchain': lock['toolchain'], 'cargo_lock_sha256': lock['cargo_lock_sha256']})
            self.assertEqual(validate_builds(config, [{'variant': 'original'}], lock), [])

    def test_preflight_snapshots_build_versions_for_portable_results(self):
        from environment import preflight
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            binary, build_path, lock_path = root/'gaia', root/'build.json', root/'environment.json'
            binary.write_bytes(b'SYNTHETIC version-evidence fixture')
            lock = {'toolchain': {'rustc': 'frozen'}, 'cargo_lock_sha256': 'a'*64}
            build = dict(source_commit='a'*40, profile='release', binary_sha256=sha256(binary),
                         toolchain=lock['toolchain'], cargo_lock_sha256=lock['cargo_lock_sha256'])
            write_json(build_path, build)
            write_json(lock_path, lock)
            config = {'synthetic': False, 'environment_lock': str(lock_path),
                      'variants': {'original': {'binary': str(binary), 'build_manifest': str(build_path)}},
                      'workload': {'binary': str(binary)},
                      'gaia': {'config_template': str(Path(__file__).resolve().parents[1]/'templates'/'monitor.toml')},
                      'environment': {'min_free_mib': 0}, 'output_root': str(root)}
            errors, _, evidence = preflight(config, [{'variant': 'original', 'mode': 'steady'}], False)
            self.assertEqual(errors, [])
            self.assertEqual(evidence['builds']['original']['source_commit'], 'a'*40)
            write_json(build_path, dict(build, source_commit='b'*40))
            self.assertEqual(evidence['builds']['original']['source_commit'], 'a'*40)

    def test_environment_probes_use_repository_cwd_and_disable_rustup_auto_install(self):
        from environment import observe_environment, tool_version
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp)/'Cargo.lock'
            path.write_text('# synthetic dependency fixture', encoding='utf-8')
            with (patch.dict(os.environ, {'RUSTUP_AUTO_INSTALL': '1'}),
                  patch('environment.platform.machine', return_value='x86_64'),
                  patch('environment.platform.release', return_value='fixture-kernel'),
                  patch('environment.subprocess.run') as run):
                run.side_effect = [SimpleNamespace(stdout='rustc fixture'), SimpleNamespace(stdout='cargo fixture')]
                observed = observe_environment(path)
                self.assertEqual(observed['toolchain'], {'rustc': 'rustc fixture', 'cargo': 'cargo fixture'})
                self.assertEqual(len(run.call_args_list), 2)
                for call in run.call_args_list:
                    self.assertEqual(Path(call.kwargs.get('cwd', '.')).resolve(), path.parent.resolve())
                    self.assertEqual(call.kwargs.get('env', {}).get('RUSTUP_AUTO_INSTALL'), '0')
            # Also exercise the real subprocess boundary without running Rust.
            output = tool_version([sys.executable, '-c', 'import os; print(os.environ["RUSTUP_AUTO_INSTALL"]); print(os.getcwd())'], cwd=path.parent)
            self.assertEqual(output.splitlines(), ['0', str(path.parent.resolve())])


if __name__ == '__main__':
    unittest.main()
