//! Compare bounded-retention aggregations with the previous materializing
//! algorithms on the same generated input. Both sides are invoked through an
//! opaque ValueBuiltin pointer to preserve the same optimization boundary.
//! Shell initialization is excluded; timed operations include generation,
//! aggregation and result disposal.
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use jsh::environment::ShellState;
use jsh::pipeline_data::PipelineData;
use jsh::value::Value;
use jsh::value_builtins::{ValueBuiltin, VALUE_BUILTINS};
use std::hint::black_box;

fn input(rows: i64) -> PipelineData {
    PipelineData::Stream(Box::new((0..rows).map(Value::Int)))
}

#[inline(never)]
fn materialized_count(
    input: PipelineData,
    _: &[String],
    _: &mut ShellState,
) -> Result<PipelineData, i32> {
    let values = input.into_values()?;
    Ok(PipelineData::Values(vec![Value::Int(values.len() as i64)]))
}

#[inline(never)]
fn materialized_sum(
    input: PipelineData,
    _: &[String],
    _: &mut ShellState,
) -> Result<PipelineData, i32> {
    let values = input.into_values()?;
    let numbers: Vec<f64> = values.iter().filter_map(Value::as_f64).collect();
    if numbers.is_empty() {
        return Err(1);
    }
    let sum: f64 = numbers.iter().sum();
    let result = if sum.fract() == 0.0 && sum.is_finite() && sum.abs() < 1e16 {
        Value::Int(sum as i64)
    } else {
        Value::Float(sum)
    };
    Ok(PipelineData::Values(vec![result]))
}

#[inline(never)]
fn materialized_last(
    input: PipelineData,
    args: &[String],
    _: &mut ShellState,
) -> Result<PipelineData, i32> {
    let n: usize = args
        .first()
        .and_then(|count| count.parse().ok())
        .unwrap_or(1);
    let values = input.into_values()?;
    Ok(PipelineData::Values(
        values[values.len().saturating_sub(n)..].to_vec(),
    ))
}

fn bench_aggregates(c: &mut Criterion) {
    for (command, builtin_name, args, baseline) in [
        ("count", "count", vec![], materialized_count as ValueBuiltin),
        (
            "math_sum",
            "math",
            vec!["sum".to_owned()],
            materialized_sum as ValueBuiltin,
        ),
        (
            "last_10",
            "last",
            vec!["10".to_owned()],
            materialized_last as ValueBuiltin,
        ),
    ] {
        let mut group = c.benchmark_group(format!("streaming/{command}"));
        group.sample_size(30);
        for rows in [1_000, 100_000] {
            group.throughput(Throughput::Elements(rows as u64));
            for (variant, operation) in [
                ("bounded", VALUE_BUILTINS[builtin_name]),
                ("materialized_baseline", baseline),
            ] {
                group.bench_with_input(BenchmarkId::new(variant, rows), &rows, |b, &rows| {
                    let mut state = ShellState::new(false);
                    let operation = black_box(operation);
                    b.iter(|| {
                        let output =
                            operation(input(black_box(rows)), black_box(&args), &mut state)
                                .unwrap();
                        black_box(output);
                    });
                });
            }
        }
        group.finish();
    }
}

criterion_group!(benches, bench_aggregates);
criterion_main!(benches);
