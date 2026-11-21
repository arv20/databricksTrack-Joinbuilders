#!/usr/bin/env python3
"""Profile the 2.88-second baseline, or compare it with a committed candidate.
Requires native Linux x86-64, Docker, Git, Python 3, >=8 CPUs and >=24 GiB RAM.
No host Python packages or Rust installation are needed.
"""
import argparse
import datetime
import json
import os
from pathlib import Path
import platform
import re
import shutil
import statistics
import subprocess
import tarfile
import tempfile
import time

BASELINE = '5cf2f652a7b658d58fd1bac058e3131eb850a877'
ROOT = Path(__file__).resolve().parents[1]


def parse_stages(log):
    if 'fast engine declined' in log:
        raise ValueError('Fallback engine ran; inspect stderr before comparing timings.')
    stages, previous = [], 0.0
    for elapsed, label in re.findall(r'^\s*([0-9.]+) ms  (.+)$', log, re.M):
        elapsed = float(elapsed)
        if elapsed < previous:
            raise ValueError('Stage timer reset; this is not one uninterrupted fast-engine run.')
        stages.append({'stage': label, 'elapsed_ms': elapsed, 'duration_ms': elapsed - previous})
        previous = elapsed
    return stages


def summarize(rows):
    result = {}
    for label in sorted({row['version'] for row in rows}):
        selected = [r for r in rows if r['version'] == label]
        seconds = [r['wall_seconds'] for r in selected]
        result[label] = {'runs': len(seconds), 'median_seconds': statistics.median(seconds),
                         'min_seconds': min(seconds), 'max_seconds': max(seconds),
                         'peak_process_rss_kib': max(r['max_rss_kib'] for r in selected)}
    if 'candidate' in result:
        result['speedup'] = result['baseline']['median_seconds'] / result['candidate']['median_seconds']
    return result


def validate_host(system, machine, cpus, memory):
    if system != 'Linux' or machine not in ('x86_64', 'amd64'):
        raise ValueError('Use a native Linux x86-64 machine, not this Mac or an emulated container.')
    if cpus < 8 or memory < 24 * 1024**3:
        raise ValueError('The benchmark needs at least 8 CPUs and 24 GiB RAM; 32 GiB host RAM is recommended.')


def command(args, log=None, cwd=None, timeout=None):
    if log is not None:
        with Path(log).open('w') as output:
            subprocess.run([str(a) for a in args], cwd=cwd, stdout=output,
                           stderr=subprocess.STDOUT, check=True, timeout=timeout)
        return ''
    return subprocess.check_output([str(a) for a in args], cwd=cwd, text=True, timeout=timeout)


def snapshot(ref, destination):
    sha = command(['git', 'rev-parse', '--verify', '--end-of-options', ref + '^{commit}'], cwd=ROOT).strip()
    archive = destination.parent / (destination.name + '.tar')
    command(['git', 'archive', '--format=tar', '--output', archive, sha], cwd=ROOT)
    destination.mkdir()
    with tarfile.open(archive) as tar:
        for member in tar.getmembers():
            name = Path(member.name)
            if name.is_absolute() or '..' in name.parts or member.issym() or member.islnk():
                raise ValueError('Snapshot contains an unsupported archive path/link.')
        tar.extractall(destination)
    archive.unlink()
    return sha


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--candidate', help='Committed Git revision; omit to profile the baseline only.')
    parser.add_argument('--baseline', default=BASELINE)
    parser.add_argument('--rounds', type=int, default=3)
    parser.add_argument('--seed', type=int, default=3)
    parser.add_argument('--events', type=int, default=20_000_000)
    parser.add_argument('--clicks', type=int, default=5_000_000)
    parser.add_argument('--data', type=Path, help='Use existing input instead of generating it.')
    parser.add_argument('--work-dir', type=Path, help='New directory for logs, data and outputs.')
    args = parser.parse_args()
    if args.rounds < 3 or args.events < 1000 or args.clicks < 0:
        parser.error('Use at least 3 rounds, 1000 events and a nonnegative click count.')
    if platform.system() != 'Linux':
        validate_host(platform.system(), platform.machine(), 0, 0)
    cpus = len(os.sched_getaffinity(0))
    meminfo = Path('/proc/meminfo').read_text()
    memory = int(re.search(r'^MemTotal:\s+(\d+)', meminfo, re.M)[1]) * 1024
    validate_host(platform.system(), platform.machine(), cpus, memory)
    docker = json.loads(command(['docker', 'info', '--format', '{{json .}}']))
    validate_host(docker['OSType'].capitalize(), docker['Architecture'], docker['NCPU'], docker['MemTotal'])
    work = (args.work_dir or ROOT / 'work' / ('linux-benchmark-' + datetime.datetime.now(
        datetime.timezone.utc).strftime('%Y%m%dT%H%M%SZ'))).resolve()
    work.mkdir(parents=True, exist_ok=False)
    report = {'status': 'running', 'baseline_ref': args.baseline, 'candidate_ref': args.candidate,
              'host': {'platform': platform.platform(), 'available_cpus': cpus, 'ram_bytes': memory,
                       'cpu_details': command(['lscpu']), 'docker_version': docker['ServerVersion']},
              'limits': {'cpus': 8, 'memory_gib': 24, 'swap': False}, 'trials': [], 'run_order': []}
    def save():
        (work / 'report.json').write_text(json.dumps(report, indent=2) + '\n')
    save()
    try:
        # All versions run on the same selected logical CPUs. Record topology, do not
        # claim these are identical to the scorer's pinned Xeon hyperthreads.
        affinity = ','.join(str(n) for n in sorted(os.sched_getaffinity(0))[:8])
        report['limits']['cpuset_cpus'] = affinity
        limits = ['--platform', 'linux/amd64', '--network', 'none', '--read-only',
                  '--tmpfs', '/tmp:rw,size=64m,mode=1777', '--cap-drop', 'ALL',
                  '--security-opt', 'no-new-privileges', '--pids-limit', '4096',
                  '--cpus', '8', '--cpuset-cpus', affinity, '--memory', '24g', '--memory-swap', '24g',
                  '--user', '1000:1000']
        with tempfile.TemporaryDirectory(prefix='sources-', dir=work) as temp:
            temp = Path(temp)
            sources, images = {}, {}
            refs = [('baseline', args.baseline)] + ([('candidate', args.candidate)] if args.candidate else [])
            for label, ref in refs:
                source = temp / label
                sha = snapshot(ref, source)
                report[label + '_sha'] = sha
                tag = 'sessions-bench-' + sha[:12]
                print(f'Building {label} {sha}; logs in {work}', flush=True)
                command(['docker', 'build', '--platform', 'linux/amd64', '-t', tag, source], work / (label + '-build.log'), timeout=600)
                sources[label], images[label] = source, tag
            context = temp / 'tools'
            context.mkdir()
            for folder in ['generator', 'fixtures', 'scripts']:
                shutil.copytree(sources['baseline'] / folder, context / folder)
            (context / 'Dockerfile').write_text('''FROM rust:1-slim-bookworm AS generator
WORKDIR /src
COPY generator/ ./
RUN cargo build --release --locked
FROM python:3.12-slim-bookworm
RUN apt-get update && apt-get install -y --no-install-recommends time && rm -rf /var/lib/apt/lists/*
RUN pip install --no-cache-dir polars==2.1.0
COPY --from=generator /src/target/release/generate /usr/local/bin/generate
COPY fixtures/ /fixtures/
COPY scripts/compare.py scripts/run_fixtures.py /checks/
''')
            tools_tag = 'sessions-bench-tools-' + report['baseline_sha'][:12]
            command(['docker', 'build', '--platform', 'linux/amd64', '-t', tools_tag, context], work / 'tools-build.log')
            for label, image in list(images.items()):
                # The timed runtime is the submitted image plus GNU time, not a Python image.
                (context / 'Measure.Dockerfile').write_text(
                    f'FROM {tools_tag} AS tools\nFROM {image}\nCOPY --from=tools /usr/bin/time /bench/time\n')
                measured = image + '-measured'
                command(['docker', 'build', '--platform', 'linux/amd64', '-f', context / 'Measure.Dockerfile',
                         '-t', measured, context], work / (label + '-measure-build.log'))
                command(['docker', 'run', '--rm', *limits, '--entrypoint', '/bench/time', measured, '--version'],
                        work / (label + '-timer-check.log'))
                # Fixtures use the very same executable, with the checker available.
                (context / 'Fixtures.Dockerfile').write_text(
                    f'FROM {image} AS pipeline\nFROM {tools_tag}\nCOPY --from=pipeline /app/rproto /app/rproto\n')
                fixture_image = image + '-fixtures'
                command(['docker', 'build', '--platform', 'linux/amd64', '-f', context / 'Fixtures.Dockerfile',
                         '-t', fixture_image, context], work / (label + '-fixture-build.log'))
                command(['docker', 'run', '--rm', *limits, fixture_image, 'python', '/checks/run_fixtures.py',
                         '/fixtures', '--', '/app/rproto'], work / (label + '-fixtures.log'))
                images[label] = measured
                report[label + '_image_id'] = command(['docker', 'image', 'inspect', measured, '--format', '{{.Id}}']).strip()
            data = args.data.resolve() if args.data else work / 'data'
            if not args.data:
                data.mkdir(mode=0o777)
                data.chmod(0o777)
                print('Generating input (not timed)...', flush=True)
                command(['docker', 'run', '--rm', *limits, '-v', f'{data}:/data', tools_tag, 'generate',
                         '--seed', str(args.seed), '--events', str(args.events), '--clicks', str(args.clicks),
                         '--threads', '8', '--output', '/data'], work / 'generate.log')
            if not all((data / p).exists() for p in ['manifest.json', 'events', 'clicks', 'links', 'products.csv', 'campaigns.csv']):
                raise ValueError('Incomplete input dataset.')
            report['data'] = {'path': str(data), 'generated': not bool(args.data),
                              'manifest': json.loads((data / 'manifest.json').read_text()),
                              'seed': None if args.data else args.seed,
                              'events_requested': None if args.data else args.events,
                              'clicks_requested': None if args.data else args.clicks}
            reference = None
            def run(label, name, measured):
                nonlocal reference
                out, scratch = work / name, work / (name + '-scratch')
                for directory in [out, scratch]:
                    directory.mkdir(mode=0o777)
                    directory.chmod(0o777)
                fmt = '{"wall_seconds":%e,"user_cpu_seconds":%U,"system_cpu_seconds":%S,"max_rss_kib":%M,"major_page_faults":%F,"exit_code":%x}'
                start = time.monotonic()
                container_name = 'sessions-bench-' + str(os.getpid()) + '-' + name
                try:
                    command(['docker', 'run', '--rm', '--name', container_name, *limits, '-v', f'{data}:/data:ro', '-v', f'{out}:/output',
                         '-v', f'{scratch}:/scratch', '-e', 'DATA_DIR=/data', '-e', 'OUTPUT_DIR=/output',
                         '-e', 'SCRATCH_DIR=/scratch', '-e', 'HOME=/scratch', '-e', 'NCPU=8', '-e', 'MEM_GB=24',
                         '-e', 'TIMING=1', '--entrypoint', '/bench/time', images[label],
                         '-f', fmt, '-o', '/output/resources.json', '/app/rproto'], work / (name + '.log'), timeout=240)
                finally:
                    subprocess.run(['docker', 'rm', '-f', container_name], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False)
                row = json.loads((out / 'resources.json').read_text())
                row.update(version=label, run=name, docker_command_seconds=time.monotonic() - start,
                           stages=parse_stages((work / (name + '.log')).read_text()))
                if not row['stages'] or row['exit_code'] != 0:
                    raise ValueError('Missing stage timings or unsuccessful pipeline run.')
                if reference is None:
                    reference = out
                else:
                    command(['docker', 'run', '--rm', *limits, '-v', f'{reference}:/a:ro', '-v', f'{out}:/b:ro',
                             tools_tag, 'python', '/checks/compare.py', '/a', '/b'], work / (name + '-compare.log'))
                if measured:
                    report['trials'].append(row)
                    report['run_order'].append(label)
                    save()
                print(f'{name}: {row["wall_seconds"]:.2f}s, max RSS {row["max_rss_kib"] / 1024:.0f} MiB; checked', flush=True)
            for label in images:
                run(label, label + '-warmup', False)
            for index in range(args.rounds):
                order = list(images) if index % 2 == 0 else list(reversed(images))
                for label in order:
                    run(label, f'{label}-{index + 1}', True)
            report['summary'] = summarize(report['trials'])
            report['status'] = 'passed'
            report['limitations'] = ['Generated data and this Linux CPU are not the hidden scorer.',
                                    'Caches are warmed, not flushed; builds/generation/comparisons are untimed.',
                                    'GNU time reports process max RSS, not cgroup peak including file cache.',
                                    '8 selected logical CPUs may have different topology from scorer hyperthreads.']
            save()
            print(json.dumps(report['summary'], indent=2))
            print(f'Send back {work / "report.json"} and the *.log files. Do not submit this tooling branch as a new optimization.')
    except BaseException as error:
        report['status'] = 'failed'
        report['error'] = str(error)
        save()
        raise


if __name__ == '__main__':
    try:
        main()
    except (ValueError, subprocess.CalledProcessError, subprocess.TimeoutExpired) as error:
        raise SystemExit(str(error))
