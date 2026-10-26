use indexmap::IndexMap;
use jsh::environment::ShellState;
use jsh::pipeline_data::PipelineData;
use jsh::value::{ClosureData, Value};
use jsh::value_builtins::VALUE_BUILTINS;
use std::collections::HashMap;
use std::ops::Range;
use std::process::Command;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

/// Every generated record owns a probe reference. Observing the peak number
/// of live references detects whole-stream buffering without allocator timing
/// or RSS thresholds, and without storing the generated rows in the test.
struct TrackedRows {
    range: Range<i64>,
    probe: Arc<ClosureData>,
    pulls: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
}

impl Iterator for TrackedRows {
    type Item = Value;

    fn next(&mut self) -> Option<Value> {
        let n = self.range.next()?;
        self.pulls.fetch_add(1, Ordering::Relaxed);
        let mut row = IndexMap::new();
        row.insert("n".to_owned(), Value::Int(n));
        row.insert(
            "payload".to_owned(),
            Value::Closure(Arc::clone(&self.probe)),
        );
        self.peak
            .fetch_max(Arc::strong_count(&self.probe), Ordering::Relaxed);
        Some(Value::Record(row))
    }
}

struct Probe {
    payload: Arc<ClosureData>,
    pulls: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
}

fn tracked(range: Range<i64>) -> (PipelineData, Probe) {
    let payload = Arc::new(ClosureData {
        params: vec![],
        body_src: String::new(),
        captured: HashMap::new(),
    });
    let pulls = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let input = PipelineData::Stream(Box::new(TrackedRows {
        range,
        probe: Arc::clone(&payload),
        pulls: Arc::clone(&pulls),
        peak: Arc::clone(&peak),
    }));
    (
        input,
        Probe {
            payload,
            pulls,
            peak,
        },
    )
}

fn apply(
    name: &str,
    input: PipelineData,
    args: &[&str],
    state: &mut ShellState,
) -> Result<Vec<Value>, i32> {
    VALUE_BUILTINS[name](
        input,
        &args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>(),
        state,
    )?
    .into_values()
}

#[test]
fn count_and_length_consume_rows_without_retaining_payloads() {
    for name in ["count", "length"] {
        let (input, probe) = tracked(0..1000);
        let output = apply(name, input, &[], &mut ShellState::new(false)).unwrap();
        assert_eq!(output, vec![Value::Int(1000)], "{name}");
        assert_eq!(probe.pulls.load(Ordering::Relaxed), 1000);
        assert!(
            probe.peak.load(Ordering::Relaxed) <= 3,
            "{name} retained input rows"
        );
        assert_eq!(Arc::strong_count(&probe.payload), 1);
    }
}

#[test]
fn simple_math_folds_retain_constant_rows_and_preserve_results() {
    for (op, expected) in [
        ("sum", Value::Int(4950)),
        ("avg", Value::Float(49.5)),
        ("mean", Value::Float(49.5)),
        ("min", Value::Int(0)),
        ("max", Value::Int(99)),
        ("product", Value::Int(0)),
    ] {
        let (input, probe) = tracked(0..100);
        let output = apply("math", input, &[op, "n"], &mut ShellState::new(false)).unwrap();
        assert_eq!(output, vec![expected], "{op}");
        assert_eq!(probe.pulls.load(Ordering::Relaxed), 100);
        assert!(
            probe.peak.load(Ordering::Relaxed) <= 3,
            "{op} retained input rows"
        );
        assert_eq!(Arc::strong_count(&probe.payload), 1);
    }
}

#[test]
fn last_keeps_only_requested_tail_in_original_order() {
    for n in [0, 1, 3, 1001] {
        let (input, probe) = tracked(0..1000);
        let output = apply(
            "last",
            input,
            &[&n.to_string()],
            &mut ShellState::new(false),
        )
        .unwrap();
        let expected_len = n.min(1000);
        assert_eq!(output.len(), expected_len);
        assert_eq!(probe.pulls.load(Ordering::Relaxed), 1000);
        assert!(probe.peak.load(Ordering::Relaxed) <= expected_len + 3);
        let indices: Vec<i64> = output
            .iter()
            .map(|row| match row.get("n") {
                Some(Value::Int(n)) => *n,
                _ => panic!("missing n"),
            })
            .collect();
        assert_eq!(
            indices,
            ((1000 - expected_len as i64)..1000).collect::<Vec<_>>()
        );
        drop(output);
        assert_eq!(Arc::strong_count(&probe.payload), 1);
    }
    let (input, _) = tracked(0..2);
    assert_eq!(
        apply(
            "last",
            input,
            &[&usize::MAX.to_string()],
            &mut ShellState::new(false)
        )
        .unwrap()
        .len(),
        2
    );
}

fn install_reducer(state: &mut ShellState, body: &str) {
    state.let_vars.insert(
        "f".to_owned(),
        Value::Closure(Arc::new(ClosureData {
            params: vec!["acc".to_owned(), "it".to_owned()],
            body_src: body.to_owned(),
            captured: HashMap::new(),
        })),
    );
}

#[test]
fn reduce_consumes_stream_incrementally_without_retaining_rows() {
    let (input, probe) = tracked(0..1000);
    let mut state = ShellState::new(false);
    install_reducer(&mut state, "$acc + $it.n");
    let output = apply("reduce", input, &["-i", "0", "f"], &mut state).unwrap();
    assert_eq!(output, vec![Value::Int(499500)]);
    assert_eq!(probe.pulls.load(Ordering::Relaxed), 1000);
    assert!(probe.peak.load(Ordering::Relaxed) <= 4);
    assert_eq!(Arc::strong_count(&probe.payload), 1);
}

#[test]
fn reduce_failure_stops_demand_and_empty_semantics_stay_unchanged() {
    let (input, probe) = tracked(0..1000);
    let mut state = ShellState::new(false);
    install_reducer(&mut state, "$acc / 0");
    assert!(apply("reduce", input, &["-i", "0", "f"], &mut state).is_err());
    assert_eq!(probe.pulls.load(Ordering::Relaxed), 1);
    assert_eq!(Arc::strong_count(&probe.payload), 1);
    assert_eq!(
        apply("reduce", PipelineData::Empty, &["f"], &mut state).unwrap(),
        vec![Value::Null]
    );
    assert_eq!(
        apply("reduce", PipelineData::Empty, &["-i", "7", "f"], &mut state).unwrap(),
        vec![Value::Int(7)]
    );
}

#[test]
fn math_keeps_numeric_coercion_missing_field_and_statistics_behavior() {
    let cases = [
        ("sum", "[1,\"2\",true,null,\"oops\",3.5]", 7.5),
        ("avg", "[1,\"2\",true,null,\"oops\",3.5]", 1.875),
        ("median", "[4,1,3,2]", 2.5),
        ("variance", "[1,2,3]", 2.0 / 3.0),
        ("stddev", "[1,2,3]", (2.0_f64 / 3.0).sqrt()),
    ];
    for (op, input, expected) in cases {
        let output = apply(
            "math",
            PipelineData::Bytes(input.as_bytes().to_vec()),
            &[op],
            &mut ShellState::new(false),
        )
        .unwrap();
        assert_eq!(output[0].as_f64().unwrap(), expected, "{op}");
    }
    for op in [
        "sum", "min", "max", "avg", "product", "median", "variance", "stddev",
    ] {
        assert!(
            apply(
                "math",
                PipelineData::Empty,
                &[op],
                &mut ShellState::new(false)
            )
            .is_err(),
            "{op}"
        );
    }
}

#[test]
fn aggregate_cli_pipelines_keep_their_public_results() {
    for (script, expected) in [
        ("range 1..1000 | count", "1000"),
        ("range 1..1000 | length", "1000"),
        ("range 1..1000 | math sum", "500500"),
        ("range 1..1000 | math avg", "500.5"),
        ("range 1..1000 | last 3 | to-json", "[998,999,1000]"),
        (
            "range 1..1000 | reduce -i 0 {|acc,it| $acc + $it}",
            "500500",
        ),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_jsh"))
            .args(["-c", script])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{script}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            json,
            serde_json::from_str::<serde_json::Value>(expected).unwrap(),
            "{script}"
        );
    }
}

#[test]
fn count_and_length_preserve_eager_input_and_json_error_behavior() {
    for name in ["count", "length"] {
        let mut state = ShellState::new(false);
        for input in [
            PipelineData::Values(vec![Value::Null, Value::Bool(false)]),
            PipelineData::Bytes(b"[null,false]".to_vec()),
        ] {
            assert_eq!(
                apply(name, input, &[], &mut state).unwrap(),
                vec![Value::Int(2)]
            );
        }
        assert_eq!(
            apply(name, PipelineData::Empty, &[], &mut state).unwrap(),
            vec![Value::Int(0)]
        );
        assert!(apply(
            name,
            PipelineData::Bytes(b"not json".to_vec()),
            &[],
            &mut state
        )
        .is_err());
    }
}
