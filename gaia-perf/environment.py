"""Read-only preflight and frozen build evidence. Never installs, upgrades or builds."""
import os
import platform
import re
import shutil
import socket
import subprocess
from pathlib import Path

from common import read_json, sha256


def tool_version(argv, cwd=None):
    environment = {**os.environ, 'RUSTUP_AUTO_INSTALL': '0'}
    result = subprocess.run(argv, cwd=cwd, env=environment,
                            capture_output=True, text=True, timeout=3, check=True)
    return result.stdout.strip()


def observe_environment(cargo_lock_path):
    cargo_lock_path = Path(cargo_lock_path).resolve()
    lock_hash = sha256(cargo_lock_path)
    distribution = {}
    if Path('/etc/os-release').is_file():
        for line in Path('/etc/os-release').read_text().splitlines():
            key, sep, value = line.partition('=')
            if sep and key in ('ID', 'VERSION_ID', 'PRETTY_NAME'):
                distribution[key] = value.strip('"')
    return {'schema_version': 1, 'distribution': distribution, 'architecture': platform.machine(),
            'kernel': platform.release(), 'cpu_count': os.cpu_count(),
            'toolchain': {'rustc': tool_version(['rustc', '--version', '--verbose'], cwd=cargo_lock_path.parent),
                          'cargo': tool_version(['cargo', '--version'], cwd=cargo_lock_path.parent)},
            'cargo_lock_path': str(cargo_lock_path), 'cargo_lock_sha256': lock_hash}


def validate_builds(config, plan, lock):
    errors = []
    for variant in sorted({r['variant'] for r in plan}):
        value = config['variants'][variant]
        prefix = f'variants.{variant}.build_manifest'
        try:
            build = read_json(value['build_manifest'])
            if build.get('profile') != 'release':
                errors.append(prefix + '.profile: formal comparison requires release')
            if build.get('binary_sha256') != sha256(value['binary']):
                errors.append(prefix + '.binary_sha256: binary changed or wrong build manifest')
            if not re.fullmatch(r'[0-9a-fA-F]{40}', build.get('source_commit', '')):
                errors.append(prefix + '.source_commit: 40-character source commit required')
            expected = value.get('expected_commit')
            if expected and build.get('source_commit') != expected:
                errors.append(prefix + '.source_commit: differs from variants expected_commit')
            for key in ('toolchain', 'cargo_lock_sha256'):
                if not lock.get(key) or build.get(key) != lock.get(key):
                    errors.append(prefix + '.' + key + ': differs from frozen environment')
        except (OSError, ValueError, TypeError) as exc:
            errors.append(prefix + ': ' + str(exc))
    return errors


def check_port():
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    try:
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        sock.bind(('127.0.0.1', 17890))
    finally:
        sock.close()


def preflight(config, plan, check_environment=True):
    errors, warnings, evidence = [], [], {}
    files = [('workload.binary', config['workload']['binary']), ('gaia.config_template', config['gaia']['config_template'])]
    for variant in sorted({r['variant'] for r in plan}):
        files.append((f'variants.{variant}.binary', config['variants'][variant]['binary']))
    if (any(r['mode'] == 'patch' for r in plan)
            and config['patch'].get('action', 'replace_function') == 'replace_function'):
        files.append(('patch.library', config['patch']['library']))
    for field, path in files:
        if not Path(path).is_file():
            errors.append(field + ': file not found: ' + str(path))
        elif field.endswith('.binary') and os.name == 'posix' and not os.access(path, os.X_OK):
            errors.append(field + ': file is not executable: ' + str(path))
    output = Path(config['output_root'])
    while not output.exists() and output != output.parent:
        output = output.parent
    free = shutil.disk_usage(output).free/(1024*1024)
    evidence['free_mib'] = free
    if free < config.get('environment', {}).get('min_free_mib', 512):
        errors.append('environment.min_free_mib: insufficient free disk space')
    if not config.get('synthetic'):
        try:
            lock = read_json(config['environment_lock'])
            evidence['environment_lock'] = lock
            errors.extend(validate_builds(config, plan, lock))
            evidence['builds'] = {variant: read_json(config['variants'][variant]['build_manifest'])
                                  for variant in sorted({r['variant'] for r in plan})}
            if check_environment:
                current = observe_environment(lock['cargo_lock_path'])
                evidence['environment'] = current
                for field in ('distribution', 'architecture', 'kernel', 'toolchain', 'cargo_lock_sha256'):
                    if current[field] != lock.get(field):
                        errors.append(f'environment_lock.{field}: current environment differs from frozen lock')
        except (OSError, ValueError, KeyError, subprocess.SubprocessError) as exc:
            errors.append('environment_lock: ' + str(exc))
        if check_environment:
            if platform.system() != 'Linux':
                errors.append('environment: formal collection requires Linux /proc (local tests use --synthetic)')
            else:
                if os.geteuid() != 0:
                    errors.append('environment.root: Gaia requires root')
                if not Path('/sys/kernel/btf/vmlinux').is_file():
                    errors.append('environment.BTF: /sys/kernel/btf/vmlinux missing')
                cgroups = []
                for line in Path('/proc/mounts').read_text().splitlines():
                    parts = line.split()
                    if len(parts) >= 3 and parts[2] == 'cgroup2':
                        mount = parts[1].replace('\\040', ' ').replace('\\134', '\\')
                        if Path(mount, 'cgroup.controllers').is_file():
                            cgroups.append(mount)
                evidence['cgroup2_mounts'] = cgroups
                if not cgroups:
                    errors.append('environment.cgroup2: no usable mount; after reboot, mount -t cgroup2 none /run/gaia-cgroup2 (create directory first)')
                expected = config.get('environment', {}).get('expected_cgroup2')
                if expected and expected not in cgroups:
                    warnings.append(f'expected cgroup2 {expected} not mounted; discovered mounts: {cgroups}')
    if check_environment:
        try:
            check_port()
        except OSError as exc:
            errors.append('gaia.listen: 127.0.0.1:17890 unavailable: ' + str(exc))
    if config.get('synthetic'):
        warnings.append('SYNTHETIC ONLY: no actual Gaia binary/build/environment validation; never formal performance data')
    elif any(r['mode'] == 'patch' for r in plan) and not config['patch'].get('events_adapter'):
        warnings.append('patch.events_adapter missing: HTTP can be recorded but patch performance runs remain unverified')
    return errors, warnings, evidence
