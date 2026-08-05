/// Interactive completion benchmarks.
///
/// Completion runs between one keystroke and the next, so what matters here
/// is the whole `complete` call at a realistic buffer, not any single source.
/// Every case below is deliberately free of external probes: Git, Docker and
/// systemd are measured by their own timeouts, not by this harness, and a
/// benchmark that forked them would report the machine's daemons rather than
/// this code. `clear_cache` runs per iteration so no measurement is a cache
/// hit — the interactive worst case is the first Tab after a command.
use criterion::{criterion_group, criterion_main, Criterion};
use std::hint::black_box;

use jsh::completer::{clear_cache, complete, filter_completions, fuzzy_match_score};
use jsh::environment::ShellState;

fn populated_state() -> ShellState {
    let mut state = ShellState::new(false);
    for index in 0..200 {
        state
            .env_vars
            .insert(format!("JSH_BENCH_VAR_{index}"), format!("value {index}"));
        state
            .aliases
            .insert(format!("bench_alias_{index}"), format!("echo {index}"));
    }
    state
}

fn bench_complete(c: &mut Criterion) {
    let mut group = c.benchmark_group("completion");
    let mut state = populated_state();

    // Command position: builtins, signatures, aliases, functions and PATH.
    group.bench_function("command_prefix", |b| {
        b.iter(|| {
            clear_cache();
            let buffer = black_box("ec");
            complete(buffer, buffer.len(), &mut state)
        })
    });

    // The same Tab again within one command line. Command position builds a
    // candidate list from every builtin, alias, function and PATH entry;
    // this is what not rebuilding it is worth.
    clear_cache();
    group.bench_function("command_prefix_warm", |b| {
        b.iter(|| {
            let buffer = black_box("ec");
            complete(buffer, buffer.len(), &mut state)
        })
    });

    // The static subcommand tables, on the prefix path and the fuzzy path.
    group.bench_function("subcommand_prefix", |b| {
        b.iter(|| {
            clear_cache();
            let buffer = black_box("cargo bu");
            complete(buffer, buffer.len(), &mut state)
        })
    });
    group.bench_function("subcommand_fuzzy_fallback", |b| {
        b.iter(|| {
            clear_cache();
            let buffer = black_box("cargo bld");
            complete(buffer, buffer.len(), &mut state)
        })
    });

    // Variable names, where the candidate set grows with the environment.
    group.bench_function("variable_prefix", |b| {
        b.iter(|| {
            clear_cache();
            let buffer = black_box("echo $JSH_BENCH");
            complete(buffer, buffer.len(), &mut state)
        })
    });

    clear_cache();
    group.bench_function("variable_prefix_warm", |b| {
        b.iter(|| {
            let buffer = black_box("echo $JSH_BENCH");
            complete(buffer, buffer.len(), &mut state)
        })
    });

    // Path completion over this repository's own source directory.
    group.bench_function("path_prefix", |b| {
        b.iter(|| {
            clear_cache();
            let buffer = black_box("cat src/comp");
            complete(buffer, buffer.len(), &mut state)
        })
    });

    // Wrong case, so the smartcase tier runs and then nothing matches: this
    // is the fallback to arguments from history, which decodes the history
    // file. Measured cold, as the first Tab after a command pays it.
    group.bench_function("history_argument_fallback_cold", |b| {
        b.iter(|| {
            clear_cache();
            let buffer = black_box("cat SRC/");
            complete(buffer, buffer.len(), &mut state)
        })
    });

    // The same Tab again within one command line, answered from the
    // per-command-line caches — the completion result itself, and behind it
    // the decoded history and every probe. The gap between the two numbers
    // is what those caches buy while a word is being typed.
    clear_cache();
    group.bench_function("history_argument_fallback_warm", |b| {
        b.iter(|| {
            let buffer = black_box("cat SRC/");
            complete(buffer, buffer.len(), &mut state)
        })
    });

    group.finish();
}

fn bench_ranking(c: &mut Criterion) {
    let mut group = c.benchmark_group("completion_ranking");

    group.bench_function("fuzzy_match_score_hit", |b| {
        b.iter(|| fuzzy_match_score(black_box("checkout"), black_box("chk")))
    });
    group.bench_function("fuzzy_match_score_miss", |b| {
        b.iter(|| fuzzy_match_score(black_box("checkout"), black_box("zzz")))
    });

    // Ranking cost at the size a probe-backed source actually reaches:
    // a repository with hundreds of refs, a host with hundreds of units.
    let mut state = ShellState::new(false);
    for index in 0..500 {
        state
            .env_vars
            .insert(format!("candidate_{index:03}"), String::new());
    }
    let candidates = jsh::completer::complete_from_history("");
    group.bench_function("filter_500_candidates", |b| {
        b.iter(|| filter_completions(black_box(candidates.clone()), black_box("ca")))
    });

    group.finish();
}

criterion_group!(benches, bench_complete, bench_ranking);
criterion_main!(benches);
