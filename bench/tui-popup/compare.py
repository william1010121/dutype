#!/usr/bin/env python3
"""Build production-source popup harnesses, then measure only in an authorized window."""
import argparse
import datetime
import hashlib
import json
import os
import platform
import shutil
import signal
import statistics
import subprocess
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent

def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()

def write_json(path, value):
    path.write_text(json.dumps(value, indent=2) + '\n')

def git_read(source, *arguments):
    result = subprocess.run(['git', '-C', str(source), *arguments], capture_output=True, text=True)
    return result.stdout.strip() if result.returncode == 0 else None

def binary_hashes(root):
    return {f.name: sha(f) for f in sorted((root / 'bin').iterdir()) if f.is_file()}

def check_identities(root, manifest, record_path):
    identities = {}
    for variant in ('before', 'after'):
        for suffix, instrumented in (('time', False), ('metrics', True)):
            name = f'{variant}-{suffix}'
            path = root / 'bin' / name
            identity = json.loads(subprocess.check_output([str(path), 'identity'], text=True))
            source = manifest['variants'][variant]
            assert identity['variant'] == variant and identity['instrumented'] == instrumented, identity
            assert identity['expectation'] == source['expectation'], identity
            assert identity['app_rs_sha256'] == source['app_rs_sha256'], identity
            identity['binary_sha256'] = sha(path)
            identities[name] = identity
    for suffix in ('time', 'metrics'):
        assert identities[f'before-{suffix}']['binary_sha256'] != identities[f'after-{suffix}']['binary_sha256']
    write_json(record_path, identities)
    return identities

def build(args):
    root = args.output.resolve()
    if root.exists():
        raise ValueError('Use a new external output directory; existing artifacts are never overwritten.')
    sources = {'before': args.before.resolve(), 'after': args.after.resolve()}
    for source in sources.values():
        if root.is_relative_to(source) or source.is_relative_to(root):
            raise ValueError('Output must be outside both source checkouts.')
        for name in ('Cargo.toml', 'Cargo.lock', 'src/app.rs'):
            if not (source / name).is_file():
                raise ValueError(f'Missing source file: {source / name}')
    assert sha(sources['before'] / 'Cargo.lock') == sha(sources['after'] / 'Cargo.lock'), 'Compare identical locked dependencies.'
    root.mkdir(parents=True)
    (root / 'bin').mkdir()
    (root / 'logs').mkdir()
    metadata = {
        'prepared_utc': datetime.datetime.now(datetime.timezone.utc).isoformat(),
        'harness_sha256': sha(HERE / 'main.rs'),
        'comparison_script_sha256': sha(Path(__file__)),
        'variants': {},
    }
    for variant, source in sources.items():
        target = root / variant
        target.mkdir()
        shutil.copytree(source / 'src', target / 'src')
        shutil.copy2(source / 'Cargo.lock', target / 'Cargo.lock')
        cargo = (source / 'Cargo.toml').read_text()
        if '[features]' not in cargo:
            cargo += '\n[features]\nbench_metrics = []\n'
        elif 'bench_metrics = []' not in cargo:
            cargo = cargo.replace('[features]', '[features]\nbench_metrics = []', 1)
        (target / 'Cargo.toml').write_text(cargo)
        expectation = 'buggy' if variant == 'before' and args.before_buggy else 'correct'
        production = {str(f.relative_to(source)): sha(f) for f in sorted((source / 'src').rglob('*'))
                      if f.is_file() and f.name not in {'main.rs', 'bench_variant.txt', 'bench_expectation.txt', 'bench_app_sha256.txt'}}
        metadata['variants'][variant] = {
            'source': str(source), 'expectation': expectation,
            'head': git_read(source, 'rev-parse', 'HEAD'),
            'git_status': git_read(source, 'status', '--porcelain'),
            'app_rs_sha256': sha(source / 'src/app.rs'),
            'cargo_lock_sha256': sha(source / 'Cargo.lock'),
            'copied_module_hashes': production,
        }
        harness = (HERE / 'main.rs').read_text()
        if (target / 'src/display.rs').exists():
            harness = harness.replace('mod category;', 'mod category;\nmod display;')
        (target / 'src/main.rs').write_text(harness)
        (target / 'src/bench_variant.txt').write_text(variant)
        (target / 'src/bench_expectation.txt').write_text(expectation)
        (target / 'src/bench_app_sha256.txt').write_text(metadata['variants'][variant]['app_rs_sha256'])
        for name, digest in production.items():
            assert sha(target / name) == digest, (variant, name)
    write_json(root / 'source-manifest.json', metadata)
    env = dict(os.environ, RUSTUP_TOOLCHAIN='1.99.1')
    with (root / 'toolchain.txt').open('w') as stream:
        subprocess.run(['rustc', '--version', '--verbose'], env=env, stdout=stream, check=True)
        subprocess.run(['cargo', '--version'], env=env, stdout=stream, check=True)
    for variant in ('before', 'after'):
        env['CARGO_TARGET_DIR'] = str(root / f'target-{variant}')
        for suffix, instrumented in (('time', False), ('metrics', True)):
            command = ['cargo', 'build', '--offline', '--locked', '--release', '--manifest-path', str(root / variant / 'Cargo.toml')]
            if instrumented:
                command += ['--features', 'bench_metrics']
            with (root / 'logs' / f'build-{variant}-{suffix}.log').open('w') as stream:
                subprocess.run(command, env=env, stdout=stream, stderr=subprocess.STDOUT, check=True)
            shutil.copy2(root / f'target-{variant}/release/dutype', root / 'bin' / f'{variant}-{suffix}')
    if platform.system() == 'Linux':
        subprocess.run([os.environ.get('CC', 'cc'), '-O2', '-Wall', '-Wextra', '-Werror', str(HERE / 'rss_exec.c'), '-o', str(root / 'bin/rss_exec')], check=True)
    check_identities(root, metadata, root / 'binary-identities.json')
    write_json(root / 'binary-sha256.json', binary_hashes(root))
    print(f'Built and identity-checked {root}. No tree or popup was executed. Measurement is a separate explicit command.')

def capture_host(directory, label):
    write_json(directory / f'host-{label}.json', {
        'utc': datetime.datetime.now(datetime.timezone.utc).isoformat(),
        'loadavg': os.getloadavg() if hasattr(os, 'getloadavg') else None,
    })
    result = subprocess.run(['ps', '-eo', 'pid,ppid,comm,pcpu,pmem,etime'], capture_output=True, text=True)
    (directory / f'processes-{label}.txt').write_text(result.stdout + result.stderr)

def run_child(command, deadline):
    remaining = deadline - time.monotonic()
    if remaining <= 0:
        raise TimeoutError('Authorized whole-suite limit reached.')
    process = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, start_new_session=True)
    try:
        stdout, stderr = process.communicate(timeout=remaining)
    except (subprocess.TimeoutExpired, KeyboardInterrupt) as error:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        process.communicate()
        if isinstance(error, KeyboardInterrupt):
            raise
        raise TimeoutError('Authorized whole-suite limit reached.') from error
    if process.returncode:
        raise RuntimeError(f'Child exit {process.returncode}: {command!r}\n{stdout}\n{stderr}')
    return stdout, stderr

def measure(args):
    if not args.authorized:
        raise ValueError('No measurements started. Obtain explicit quiet-window authorization and pass --authorized.')
    if not 1 <= args.timeout <= 300:
        raise ValueError('--timeout must be 1..300 seconds.')
    if not args.results_name or Path(args.results_name).name != args.results_name or args.results_name in {'.', '..'}:
        raise ValueError('Use a plain new result-directory name.')
    root = args.output.resolve()
    manifest = json.loads((root / 'source-manifest.json').read_text())
    result_dir = root / args.results_name
    if result_dir.exists():
        raise ValueError('Results already exist; choose a new --results-name.')
    # Identity-only checks are not benchmark opens; verify the recorded build before the window.
    identities = check_identities(root, manifest, root / 'binary-identities-current.json')
    saved_identities = json.loads((root / 'binary-identities.json').read_text())
    assert identities == saved_identities, 'Executable identity changed since build.'
    result_dir.mkdir()
    started = time.monotonic()
    deadline = started + args.timeout
    config = {'public_command': sys.argv, 'timeout_seconds': args.timeout, 'results_name': args.results_name,
              'exit_code': None, 'timed_out': False, 'completed_harness_invocations': 0}
    write_json(result_dir / 'run-config.json', config)
    capture_host(result_dir, 'before')
    before_hashes = binary_hashes(root)
    write_json(result_dir / 'binary-sha256-before.json', before_hashes)
    write_json(result_dir / 'metadata.json', {
        'started_utc': datetime.datetime.now(datetime.timezone.utc).isoformat(),
        'platform': platform.platform(), 'machine': platform.machine(), 'cpu_count': os.cpu_count(),
        'affinity': sorted(os.sched_getaffinity(0)) if hasattr(os, 'sched_getaffinity') else None,
        'toolchain': (root / 'toolchain.txt').read_text(), 'source_manifest': manifest,
        'binary_identities': identities, 'warmups': 2, 'repetitions': 7,
        'timeout_seconds': args.timeout,
        'timing_scope': 'Uninstrumented release App::handle_key(Enter); excludes construction, verification, output and popup destruction.',
        'metric_scope': 'Rust global-allocator requests during one popup open; excludes tree/registry/App construction and popup destruction.',
        'rss_scope': 'Fresh ordinary-allocator process, including construction, one popup, output and teardown.',
    })
    records = []
    cases = [(n, pattern, pane) for n in (10_000, 100_000) for pattern in ('ties', 'descending') for pane in ('category', 'extension')]
    try:
        with (result_dir / 'raw.ndjson').open('w') as raw:
            for mode in ('verify', 'time', 'metrics', 'rss'):
                for case_index, (count, pattern, pane) in enumerate(cases):
                    variants = ('before', 'after') if case_index % 2 == 0 else ('after', 'before')
                    for variant in variants:
                        suffix = 'metrics' if mode == 'metrics' else 'time'
                        command = [str(root / 'bin' / f'{variant}-{suffix}'), mode, str(count), pattern, pane, '2', '7']
                        if mode == 'rss':
                            if Path('/usr/bin/time').is_file():
                                command = ['/usr/bin/time', '-f', '{"rss_provider":"gnu-time","max_rss_kib":%M}', '--'] + command
                            elif (root / 'bin/rss_exec').is_file():
                                command = [str(root / 'bin/rss_exec')] + command
                            else:
                                raise RuntimeError('RSS requires GNU /usr/bin/time or the Linux rss_exec fallback.')
                        stdout, stderr = run_child(command, deadline)
                        record = json.loads(stdout)
                        assert record['variant'] == variant and record['mode'] == mode, record
                        assert (record['count'], record['pattern'], record['pane']) == (count, pattern, pane), record
                        assert record['instrumented'] == (mode == 'metrics'), record
                        if mode == 'rss':
                            record['result']['rss'] = json.loads(stderr)
                        elif stderr:
                            raise RuntimeError(f'Unexpected child stderr: {stderr}')
                        record['command'] = command
                        records.append(record)
                        raw.write(json.dumps(record) + '\n')
                        raw.flush()
                        config['completed_harness_invocations'] = len(records)
        expected = {(r['variant'], r['count'], r['pattern'], r['pane']): r['result']['checksum'] for r in records if r['mode'] == 'verify'}
        for record in records:
            key = record['variant'], record['count'], record['pattern'], record['pane']
            assert record['result']['checksum'] == expected[key], record
        summary = []
        for count, pattern, pane in cases:
            row = {'count': count, 'pattern': pattern, 'pane': pane}
            for variant in ('before', 'after'):
                modes = {r['mode']: r['result'] for r in records if (r['variant'], r['count'], r['pattern'], r['pane']) == (variant, count, pattern, pane)}
                samples = modes['time']['samples_ns']
                row[variant] = {'median_ms': statistics.median(samples) / 1e6, 'min_ms': min(samples) / 1e6,
                                'max_ms': max(samples) / 1e6, 'samples_ns': samples,
                                'allocator': modes['metrics']['allocator'], 'rss': modes['rss']['rss'], 'verification': modes['verify']}
            row['after_over_before_median'] = row['after']['median_ms'] / row['before']['median_ms']
            summary.append(row)
        write_json(result_dir / 'summary.json', summary)
        config['exit_code'] = 0
    except TimeoutError as error:
        config.update(exit_code=124, timed_out=True, error=str(error))
    except KeyboardInterrupt:
        config.update(exit_code=130, error='Interrupted; active benchmark process group stopped.')
    except Exception as error:
        config.update(exit_code=1, error=str(error))
    finally:
        capture_host(result_dir, 'after')
        after_hashes = binary_hashes(root)
        write_json(result_dir / 'binary-sha256-after.json', after_hashes)
        config.update(elapsed_seconds=time.monotonic() - started, hashes_unchanged=before_hashes == after_hashes)
        if not config['hashes_unchanged']:
            config.update(exit_code=1, error='Executable hashes changed during measurement.')
        write_json(result_dir / 'run-config.json', config)
    print(f"Finished; no benchmark processes remain. {config['completed_harness_invocations']}/64 harness invocations; exit={config['exit_code']}; {result_dir}")
    if config.get('error'):
        print(config['error'], file=sys.stderr)
    return config['exit_code']

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest='command', required=True)
    b = sub.add_parser('build', help='Freeze sources and compile only; no popup opens.')
    b.add_argument('--before', type=Path, required=True)
    b.add_argument('--after', type=Path, required=True)
    b.add_argument('--output', type=Path, required=True)
    b.add_argument('--before-buggy', action='store_true', help='Expect the original incorrect equal-size cutoff only in the before revision.')
    m = sub.add_parser('measure', help='Run only after explicit quiet-window authorization.')
    m.add_argument('--output', type=Path, required=True)
    m.add_argument('--authorized', action='store_true')
    m.add_argument('--timeout', type=int, default=30)
    m.add_argument('--results-name', default='results')
    args = parser.parse_args()
    try:
        return build(args) if args.command == 'build' else measure(args)
    except (ValueError, AssertionError, subprocess.CalledProcessError, FileNotFoundError) as error:
        parser.exit(2, f'error: {error}\n')

if __name__ == '__main__':
    raise SystemExit(main())
