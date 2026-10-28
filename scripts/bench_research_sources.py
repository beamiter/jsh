#!/usr/bin/env python3
"""Offline source-snapshot microbenchmark against an explicit Git revision."""
import argparse
import importlib.util
import json
from pathlib import Path
import statistics
import subprocess
import tempfile
import time
import types
from unittest.mock import patch


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--before-ref', required=True, help='Git revision containing the original controller')
    parser.add_argument('--samples', type=int, default=20)
    args = parser.parse_args()
    if not 1 <= args.samples <= 1000:
        parser.error('--samples must be 1..1000')
    project = Path(__file__).resolve().parents[1]
    spec = importlib.util.spec_from_file_location('jsh_research_auto_bench', project / 'scripts/jsh_research_auto.py')
    current = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(current)
    engine = current.load_module('research_bench_engine', (project / 'scripts/jsh_research.py').read_text())
    original = types.ModuleType('research_bench_original')
    source = subprocess.check_output(['git', '-C', str(project), 'show',
                                      args.before_ref + ':scripts/jsh_research_auto.py'])
    exec(compile(source, '<original-research-controller>', 'exec'), original.__dict__)
    with tempfile.TemporaryDirectory(prefix='jsh-research-bench-') as temporary:
        repo = Path(temporary)
        paths = [f'source-{i}.py' for i in range(8)]
        for i, path in enumerate(paths):
            (repo / path).write_text('# benchmark\n' + f'VALUE = {i}\n' * 256)
        engine.git(repo, 'init', '-q')
        engine.git(repo, 'add', '.')
        engine.git(repo, '-c', 'user.name=Research benchmark', '-c', 'user.email=bench@jsh.invalid',
                   '-c', 'commit.gpgsign=false', 'commit', '-qm', 'source fixture')
        context = {'best': {'commit': engine.commit_id(repo, 'HEAD')}}
        config = {'source_paths': paths}
        manifest = {'repo': str(repo), 'editable_paths': paths, 'protected_paths': []}
        controllers = {}
        for name, module in (('before', original), ('after', current)):
            controller = module.AutoLoop.__new__(module.AutoLoop)
            controller.r = engine
            controller.campaign = types.SimpleNamespace(load=lambda: {'manifest': manifest})
            controllers[name] = controller
        assert controllers['before'].sources(context, config) == controllers['after'].sources(context, config)
        timings, counts = {name: [] for name in controllers}, {}
        for index in range(args.samples + 4):
            order = list(controllers.items())
            if index % 2:
                order.reverse()
            for name, controller in order:
                started = time.perf_counter()
                controller.sources(context, config)
                if index >= 4:
                    timings[name].append((time.perf_counter() - started) * 1000)
        for name, controller in controllers.items():
            with patch.object(subprocess, 'run', wraps=subprocess.run) as calls:
                controller.sources(context, config)
            counts[name] = len(calls.call_args_list)
        medians = {name: round(statistics.median(values), 3) for name, values in timings.items()}
        print(json.dumps({'before_ref': args.before_ref, 'files': len(paths),
                          'source_bytes': sum((repo / path).stat().st_size for path in paths),
                          'warmups': 4, 'samples': args.samples, 'median_ms': medians,
                          'speedup': round(medians['before'] / medians['after'], 2), 'git_processes': counts}))


if __name__ == '__main__':
    main()
