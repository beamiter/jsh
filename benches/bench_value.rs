/// Phase 14c — value-pipeline benchmarks.
///
/// Exercises the hot path through `apply_closure` (the optimization
/// landed in Phase 14c) and the structured builtins on synthetic
/// tables. The intent is to track regressions to the per-iteration cost
/// of `each` / `where` / `reduce` over large inputs.
use criterion::{criterion_group, criterion_main, Criterion};
use std::hint::black_box;

use indexmap::IndexMap;
use jsh::environment::ShellState;
use jsh::pipeline_data::PipelineData;
use jsh::value::{ClosureData, Value};
use jsh::value_builtins::VALUE_BUILTINS;
use std::collections::HashMap;
use std::sync::Arc;

fn make_int_list(n: i64) -> PipelineData {
    let vs: Vec<Value> = (0..n).map(Value::Int).collect();
    PipelineData::Values(vs)
}

fn make_record_table(n: i64) -> PipelineData {
    let vs: Vec<Value> = (0..n)
        .map(|i| {
            let mut m = IndexMap::new();
            m.insert("id".to_string(), Value::Int(i));
            m.insert("group".to_string(), Value::Int(i % 5));
            m.insert("name".to_string(), Value::String(format!("row-{}", i)));
            Value::Record(m)
        })
        .collect();
    PipelineData::Values(vs)
}

fn install_closure(state: &mut ShellState, name: &str, body: &str, params: &[&str]) {
    let c = ClosureData {
        params: params.iter().map(|s| s.to_string()).collect(),
        body_src: body.to_string(),
        captured: HashMap::new(),
    };
    state
        .let_vars
        .insert(name.to_string(), Value::Closure(Arc::new(c)));
}

fn bench_each(c: &mut Criterion) {
    let mut group = c.benchmark_group("value/each");
    group.sample_size(50);

    for n in [100i64, 1000, 10_000] {
        let bench_name = format!("each_x_plus_1_n{}", n);
        group.bench_function(&bench_name, |b| {
            b.iter_with_setup(
                || {
                    let mut state = ShellState::new(false);
                    install_closure(&mut state, "f", "$x + 1", &["x"]);
                    let data = make_int_list(n);
                    (state, data)
                },
                |(mut state, data)| {
                    let f = VALUE_BUILTINS.get("each").unwrap();
                    let out = f(data, &[String::from("f")], &mut state).unwrap();
                    black_box(out);
                },
            )
        });
    }
    group.finish();
}

fn bench_where(c: &mut Criterion) {
    let mut group = c.benchmark_group("value/where");
    group.sample_size(50);

    for n in [100i64, 1000, 10_000] {
        let bench_name = format!("where_id_mod_5_n{}", n);
        group.bench_function(&bench_name, |b| {
            b.iter_with_setup(
                || {
                    let mut state = ShellState::new(false);
                    install_closure(&mut state, "p", "$r.group == 2", &["r"]);
                    let data = make_record_table(n);
                    (state, data)
                },
                |(mut state, data)| {
                    let f = VALUE_BUILTINS.get("where").unwrap();
                    let out = f(data, &[String::from("p")], &mut state).unwrap();
                    black_box(out);
                },
            )
        });
    }
    group.finish();
}

fn bench_reduce(c: &mut Criterion) {
    let mut group = c.benchmark_group("value/reduce");
    group.sample_size(50);

    for n in [100i64, 1000, 10_000] {
        let bench_name = format!("reduce_sum_n{}", n);
        group.bench_function(&bench_name, |b| {
            b.iter_with_setup(
                || {
                    let mut state = ShellState::new(false);
                    install_closure(&mut state, "f", "$a + $b", &["a", "b"]);
                    let data = make_int_list(n);
                    (state, data)
                },
                |(mut state, data)| {
                    let f = VALUE_BUILTINS.get("reduce").unwrap();
                    let out = f(data, &[String::from("0"), String::from("f")], &mut state).unwrap();
                    black_box(out);
                },
            )
        });
    }
    group.finish();
}

fn bench_chain_where_select(c: &mut Criterion) {
    let mut group = c.benchmark_group("value/chain");
    group.sample_size(30);

    group.bench_function("where_then_select_n1000", |b| {
        b.iter_with_setup(
            || {
                let mut state = ShellState::new(false);
                install_closure(&mut state, "p", "$r.group >= 2", &["r"]);
                let data = make_record_table(1000);
                (state, data)
            },
            |(mut state, data)| {
                let wf = VALUE_BUILTINS.get("where").unwrap();
                let sf = VALUE_BUILTINS.get("select").unwrap();
                let filtered = wf(data, &[String::from("p")], &mut state).unwrap();
                let projected = sf(
                    filtered,
                    &[String::from("id"), String::from("name")],
                    &mut state,
                )
                .unwrap();
                black_box(projected);
            },
        )
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_each,
    bench_where,
    bench_reduce,
    bench_chain_where_select
);
criterion_main!(benches);
