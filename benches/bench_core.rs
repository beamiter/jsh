use criterion::{criterion_group, criterion_main, Criterion};
use std::hint::black_box;

use rsh::environment::ShellState;
use rsh::expand::{expand_word, expand_word_to_string};
use rsh::parser::lexer::tokenize;
use rsh::parser::parse::{is_incomplete, parse, parse_word_parts};

// ---------------------------------------------------------------------------
// Lexer benchmarks
// ---------------------------------------------------------------------------

fn bench_lexer(c: &mut Criterion) {
    let mut group = c.benchmark_group("lexer");

    group.bench_function("simple_command", |b| {
        b.iter(|| tokenize(black_box("echo hello world")))
    });

    group.bench_function("pipeline", |b| {
        b.iter(|| {
            tokenize(black_box(
                "cat file.txt | grep pattern | sort | uniq -c | head -10",
            ))
        })
    });

    group.bench_function("redirects_and_vars", |b| {
        b.iter(|| {
            tokenize(black_box(
                "FOO=bar cmd --flag \"$HOME/path\" 2>/dev/null <<< 'input'",
            ))
        })
    });

    group.bench_function("complex_script", |b| {
        let input = r#"
for i in 1 2 3 4 5; do
    if [ "$i" -gt 3 ]; then
        echo "big: $i"
    else
        echo "small: $i"
    fi
done
"#;
        b.iter(|| tokenize(black_box(input)))
    });

    group.bench_function("long_pipeline", |b| {
        let input = "cat /etc/passwd | cut -d: -f1 | sort | uniq | grep -v root | head -20 | tail -10 | wc -l";
        b.iter(|| tokenize(black_box(input)))
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Parser benchmarks
// ---------------------------------------------------------------------------

fn bench_parser(c: &mut Criterion) {
    let mut group = c.benchmark_group("parser");

    group.bench_function("simple_command", |b| {
        b.iter(|| parse(black_box("echo hello world")).unwrap())
    });

    group.bench_function("pipeline", |b| {
        b.iter(|| parse(black_box("cat file | grep pat | sort | uniq -c")).unwrap())
    });

    group.bench_function("and_or_chain", |b| {
        b.iter(|| parse(black_box("cmd1 && cmd2 || cmd3 && cmd4")).unwrap())
    });

    group.bench_function("multiple_commands", |b| {
        let input = "echo hello\necho world\necho foo\necho bar";
        b.iter(|| parse(black_box(input)).unwrap())
    });

    group.bench_function("semicolon_list", |b| {
        let input = "echo a; echo b; echo c; echo d; echo e";
        b.iter(|| parse(black_box(input)).unwrap())
    });

    group.bench_function("function_def", |b| {
        let input = "myfunc() { echo hello; return 0; }";
        b.iter(|| parse(black_box(input)).unwrap())
    });

    group.bench_function("assignments_and_cmd", |b| {
        let input = "FOO=bar BAZ=qux CMD_VAR=1 mycommand --flag arg1 arg2";
        b.iter(|| parse(black_box(input)).unwrap())
    });

    group.bench_function("subshell", |b| {
        let input = "(cd /tmp && ls -la | grep foo)";
        b.iter(|| parse(black_box(input)).unwrap())
    });

    group.bench_function("complex_pipeline_chain", |b| {
        let input = "cat file.txt | grep -i error | sort | uniq -c > /tmp/out.log 2>&1 && echo done || echo failed";
        b.iter(|| parse(black_box(input)).unwrap())
    });

    group.bench_function("many_redirects", |b| {
        let input = "cmd < input.txt > output.txt 2> error.log >> append.txt";
        b.iter(|| parse(black_box(input)).unwrap())
    });

    group.bench_function("brace_group", |b| {
        let input = "{ echo a; echo b; echo c; }";
        b.iter(|| parse(black_box(input)).unwrap())
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// parse_word_parts benchmarks
// ---------------------------------------------------------------------------

fn bench_word_parts(c: &mut Criterion) {
    let mut group = c.benchmark_group("word_parts");

    group.bench_function("literal", |b| {
        b.iter(|| parse_word_parts(black_box("hello")))
    });

    group.bench_function("variable", |b| {
        b.iter(|| parse_word_parts(black_box("$HOME")))
    });

    group.bench_function("double_quoted_var", |b| {
        b.iter(|| parse_word_parts(black_box("\"hello $USER world\"")))
    });

    group.bench_function("single_quoted", |b| {
        b.iter(|| parse_word_parts(black_box("'no expansion here'")))
    });

    group.bench_function("command_sub", |b| {
        b.iter(|| parse_word_parts(black_box("$(date +%Y-%m-%d)")))
    });

    group.bench_function("arithmetic", |b| {
        b.iter(|| parse_word_parts(black_box("$((1 + 2 * 3))")))
    });

    group.bench_function("tilde", |b| {
        b.iter(|| parse_word_parts(black_box("~/projects/rsh")))
    });

    group.bench_function("complex_mixed", |b| {
        b.iter(|| parse_word_parts(black_box("${HOME}/.config/$APP_NAME/\"$VERSION\"")))
    });

    group.bench_function("glob_pattern", |b| {
        b.iter(|| parse_word_parts(black_box("src/**/*.rs")))
    });

    group.bench_function("parameter_expansion", |b| {
        b.iter(|| parse_word_parts(black_box("${filename%.txt}.md")))
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Expand benchmarks (variable/arithmetic expansion, no command sub or glob)
// ---------------------------------------------------------------------------

fn make_expand_state() -> ShellState {
    let mut state = ShellState::new(false);
    state.set_var("HOME", "/home/user");
    state.set_var("USER", "testuser");
    state.set_var("PATH", "/usr/bin:/bin");
    state.set_var("filename", "document.txt");
    state.set_var("greeting", "hello world");
    state.set_var("x", "42");
    state
}

fn bench_expand(c: &mut Criterion) {
    let mut group = c.benchmark_group("expand");

    group.bench_function("literal", |b| {
        let mut state = make_expand_state();
        let word = parse_word_parts("hello");
        b.iter(|| expand_word_to_string(black_box(&word), &mut state))
    });

    group.bench_function("variable", |b| {
        let mut state = make_expand_state();
        let word = parse_word_parts("$HOME");
        b.iter(|| expand_word_to_string(black_box(&word), &mut state))
    });

    group.bench_function("double_quoted_var", |b| {
        let mut state = make_expand_state();
        let word = parse_word_parts("\"welcome $USER to $HOME\"");
        b.iter(|| expand_word_to_string(black_box(&word), &mut state))
    });

    group.bench_function("tilde_expansion", |b| {
        let mut state = make_expand_state();
        let word = parse_word_parts("~/projects");
        b.iter(|| expand_word_to_string(black_box(&word), &mut state))
    });

    group.bench_function("arithmetic_simple", |b| {
        let mut state = make_expand_state();
        let word = parse_word_parts("$((1 + 2 + 3))");
        b.iter(|| expand_word_to_string(black_box(&word), &mut state))
    });

    group.bench_function("arithmetic_complex", |b| {
        let mut state = make_expand_state();
        let word = parse_word_parts("$((10 * 20 + 30 / 5 - 2))");
        b.iter(|| expand_word_to_string(black_box(&word), &mut state))
    });

    group.bench_function("parameter_default", |b| {
        let mut state = make_expand_state();
        let word = parse_word_parts("${UNSET:-default_value}");
        b.iter(|| expand_word_to_string(black_box(&word), &mut state))
    });

    group.bench_function("parameter_length", |b| {
        let mut state = make_expand_state();
        let word = parse_word_parts("${#greeting}");
        b.iter(|| expand_word_to_string(black_box(&word), &mut state))
    });

    group.bench_function("parameter_strip_suffix", |b| {
        let mut state = make_expand_state();
        let word = parse_word_parts("${filename%.txt}");
        b.iter(|| expand_word_to_string(black_box(&word), &mut state))
    });

    group.bench_function("parameter_replace", |b| {
        let mut state = make_expand_state();
        let word = parse_word_parts("${greeting//o/0}");
        b.iter(|| expand_word_to_string(black_box(&word), &mut state))
    });

    group.bench_function("parameter_substring", |b| {
        let mut state = make_expand_state();
        let word = parse_word_parts("${greeting:6:5}");
        b.iter(|| expand_word_to_string(black_box(&word), &mut state))
    });

    group.bench_function("expand_word_with_glob", |b| {
        let mut state = make_expand_state();
        let word = parse_word_parts("/nonexistent/*.xyz");
        b.iter(|| expand_word(black_box(&word), &mut state))
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// is_incomplete benchmarks
// ---------------------------------------------------------------------------

fn bench_is_incomplete(c: &mut Criterion) {
    let mut group = c.benchmark_group("is_incomplete");

    group.bench_function("complete_simple", |b| {
        b.iter(|| is_incomplete(black_box("echo hello")))
    });

    group.bench_function("complete_pipeline", |b| {
        b.iter(|| is_incomplete(black_box("cat foo | grep bar | wc -l")))
    });

    group.bench_function("incomplete_pipe", |b| {
        b.iter(|| is_incomplete(black_box("echo hello |")))
    });

    group.bench_function("incomplete_quote", |b| {
        b.iter(|| is_incomplete(black_box("echo \"hello")))
    });

    group.bench_function("incomplete_backslash", |b| {
        b.iter(|| is_incomplete(black_box("echo hello \\")))
    });

    group.bench_function("incomplete_and", |b| {
        b.iter(|| is_incomplete(black_box("echo hello &&")))
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_lexer,
    bench_parser,
    bench_word_parts,
    bench_expand,
    bench_is_incomplete,
);
criterion_main!(benches);
