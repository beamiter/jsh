//! Builtins that share a name with a common system program (`sort`, `date`,
//! `uniq`, `sum`, `split`, `last`, `zip`, ...) must behave exactly like that
//! program for its ordinary invocations, on every execution route, while jsh's
//! own structured forms keep running the builtin. See src/shadowing.rs.
//!
//! Expected outputs were verified against GNU coreutils with `LC_ALL=C`. A test
//! that needs a program skips, saying so, when the program is not installed.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The PATH every jsh under test runs with, so `program` sees what jsh sees.
const SEARCH_PATH: &str = "/usr/local/bin:/usr/bin:/bin";

fn program(name: &str) -> Option<PathBuf> {
    SEARCH_PATH
        .split(':')
        .map(|dir| Path::new(dir).join(name))
        .find(|path| path.is_file())
}

/// Return early from a test when a program it exercises is missing.
macro_rules! require {
    ($($name:literal),+ $(,)?) => {
        $(
            if program($name).is_none() {
                eprintln!("skipping: `{}` is not installed in {}", $name, SEARCH_PATH);
                return;
            }
        )+
    };
}

struct Run {
    stdout: String,
    stderr: String,
    status: i32,
}

fn jsh_with_path(dir: &Path, path: &str, script: &str) -> Run {
    let output = Command::new(env!("CARGO_BIN_EXE_jsh"))
        .args(["--norc", "-c", script])
        .current_dir(dir)
        .env("PATH", path)
        .env("HOME", dir)
        .env("LC_ALL", "C")
        .env("TZ", "UTC")
        .stdin(Stdio::null())
        .output()
        .expect("run jsh");
    Run {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        status: output.status.code().unwrap_or(-1),
    }
}

fn jsh_in(dir: &Path, script: &str) -> Run {
    jsh_with_path(dir, SEARCH_PATH, script)
}

/// Run `script` in a fresh temporary directory and require success.
fn ok(script: &str) -> String {
    let dir = tempfile::tempdir().unwrap();
    ok_in(dir.path(), script)
}

fn ok_in(dir: &Path, script: &str) -> String {
    let run = jsh_in(dir, script);
    assert_eq!(
        run.status, 0,
        "script: {script}\nstdout: {}\nstderr: {}",
        run.stdout, run.stderr
    );
    run.stdout
}

fn squash(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

// ---------------------------------------------------------------------------
// sort
// ---------------------------------------------------------------------------

#[test]
fn sort_orders_text_from_pipes_files_and_redirections() {
    require!("sort");
    assert_eq!(ok("printf 'b\\na\\nc\\n' | sort"), "a\nb\nc\n");
    assert_eq!(ok("printf 'b\\na\\nc\\n' > s.txt; sort s.txt"), "a\nb\nc\n");
    assert_eq!(
        ok("printf 'b\\na\\nc\\n' > s.txt; sort < s.txt"),
        "a\nb\nc\n"
    );
    assert_eq!(ok("sort <<EOF\nb\na\nEOF"), "a\nb\n");
    assert_eq!(ok("sort <<< $'b\\na'"), "a\nb\n");
    assert_eq!(ok("printf 'b\\na\\n' | sort -"), "a\nb\n");
    assert_eq!(
        ok("printf 'b\\n' > f1; printf 'a\\n' > f2; sort f1 f2"),
        "a\nb\n"
    );
    assert_eq!(ok("printf '' | sort; echo st=$?"), "st=0\n");
}

#[test]
fn sort_accepts_every_coreutils_option() {
    require!("sort");
    assert_eq!(ok("printf 'b\\na\\nc\\n' | sort -r"), "c\nb\na\n");
    assert_eq!(ok("printf '10\\n9\\n100\\n' | sort -n"), "9\n10\n100\n");
    assert_eq!(ok("printf '10\\n9\\n100\\n' | sort -rn"), "100\n10\n9\n");
    assert_eq!(ok("printf 'b\\na\\nb\\n' | sort -u"), "a\nb\n");
    assert_eq!(ok("printf 'x:2\\ny:1\\n' | sort -t: -k2"), "y:1\nx:2\n");
    assert_eq!(
        ok("printf '1K\\n2M\\n3G\\n512\\n' | sort -h"),
        "512\n1K\n2M\n3G\n"
    );
    assert_eq!(
        ok("printf 'v1.10\\nv1.2\\nv1.9\\n' | sort -V"),
        "v1.2\nv1.9\nv1.10\n"
    );
    assert_eq!(ok("printf 'b\\nA\\nc\\n' | sort -f"), "A\nb\nc\n");
    assert_eq!(
        ok("printf 'b\\na\\n' > in.txt; sort -o out.txt in.txt; cat out.txt"),
        "a\nb\n"
    );
}

#[test]
fn sort_reports_errors_and_status_like_the_program() {
    require!("sort");
    let dir = tempfile::tempdir().unwrap();
    let run = jsh_in(dir.path(), "printf 'b\\na\\n' | sort -c");
    assert_eq!(
        (run.stdout.as_str(), run.stderr.as_str(), run.status),
        ("", "sort: -:2: disorder: a\n", 1)
    );
    let run = jsh_in(dir.path(), "printf 'a\\nb\\n' | sort -c && echo sorted");
    assert_eq!((run.stdout.as_str(), run.status), ("sorted\n", 0));
    let run = jsh_in(dir.path(), "sort /nonexistent-jsh-fallback-test");
    assert_eq!(run.status, 2);
    assert_eq!(run.stdout, "");
    assert_eq!(
        run.stderr,
        "sort: cannot read: /nonexistent-jsh-fallback-test: No such file or directory\n"
    );
}

#[test]
fn sort_is_the_program_on_every_text_route() {
    require!("sort", "uniq", "awk", "du");
    // Forked pipeline with external commands on both sides.
    assert_eq!(
        ok("printf 'b\\na\\nb\\n' | sort | uniq -c | awk '{print $1 $2}'"),
        "1a\n2b\n"
    );
    assert_eq!(
        ok("mkdir -p d1 d2; printf 'xxxxxxxxxx' > d1/f; du -s d1 d2 | sort -n | awk '{print $2}'"),
        "d2\nd1\n"
    );
    // A shadowing stage downstream of another one that ran its program.
    assert_eq!(ok("printf 'b\\na\\n' | sort | sort -r"), "b\na\n");
    // Command substitution, a brace group inside a stage, and lastpipe.
    assert_eq!(ok("x=$(printf 'b\\na\\n' | sort); echo \"$x\""), "a\nb\n");
    assert_eq!(ok("printf 'b\\na\\n' | { sort; }"), "a\nb\n");
    assert_eq!(ok("shopt -s lastpipe; printf 'b\\na\\n' | sort"), "a\nb\n");
    // Output redirection on the program.
    assert_eq!(ok("printf 'b\\na\\n' | sort > o.txt; cat o.txt"), "a\nb\n");
}

#[test]
fn sort_still_sorts_typed_values_from_jsh_stages() {
    // In-process value pipeline.
    assert_eq!(squash(&ok("range 1..3 | sort -r | to-json")), "[3,2,1]");
    assert_eq!(
        squash(&ok("echo '[\"b\",\"a\"]' | from-json | sort | to-json")),
        r#"["a","b"]"#
    );
    // Forked route: `echo` is not a value stage, `from-json` is.
    assert_eq!(
        squash(&ok("echo '[3,1,2]' | from-json | sort | to-json")),
        "[1,2,3]"
    );
    assert_eq!(
        squash(&ok("echo '[3,1,2]' | from-json | sort -r")),
        "[3,2,1]"
    );
    // Typed input reaches an alias of the command, and the lastpipe stage
    // that runs in the shell itself.
    assert_eq!(
        squash(&ok(
            "alias rsort='sort -r'; echo '[1,3,2]' | from-json | rsort | to-json"
        )),
        "[3,2,1]"
    );
    assert_eq!(
        squash(&ok("shopt -s lastpipe; echo '[2,1]' | from-json | sort")),
        "[1,2]"
    );
    assert_eq!(
        squash(&ok(
            "printf '[3,1,2]' > data.json; open data.json | sort | to-json"
        )),
        "[1,2,3]"
    );
    // `builtin sort` always selects the builtin.
    assert_eq!(squash(&ok("printf '[2,1]' | builtin sort")), "[1,2]");
}

#[test]
fn serializer_and_ls_output_reach_shadowing_commands_as_text() {
    require!("sort", "ls");
    // `to-json` turns values into text; the program sorts its lines.
    assert_eq!(
        ok("echo '[2,1]' | from-json | to-json | sort"),
        "  1\n  2,\n[\n]\n"
    );
    let dir = tempfile::tempdir().unwrap();
    for name in ["a", "b", "c"] {
        std::fs::write(dir.path().join(name), "").unwrap();
    }
    // Outside the in-process value pipeline `ls` is the program, so this is
    // the ordinary text pipeline, as is the long listing that `ls -l` gives.
    assert_eq!(ok_in(dir.path(), "ls | sort -r"), "c\nb\na\n");
    assert_eq!(
        ok_in(dir.path(), "ls -l | sort -k9 | awk 'NF > 2 {print $9}'"),
        "a\nb\nc\n"
    );
    // A value command in between keeps the structured listing.
    assert_eq!(
        squash(&ok_in(dir.path(), "ls | get name | sort -r | to-json")),
        r#"["c","b","a"]"#
    );
}

#[test]
fn commands_nested_in_a_stage_read_ordinary_stdin() {
    require!("sort", "last");
    // Provenance belongs to the command a stage pipes into. Inside a loop the
    // commands are ordinary shell code: neither the login `last` nor `sort
    // FILE` may swallow the loop's input, whatever produced it.
    assert_eq!(
        ok("printf 'a\\nb\\n' | while read x; do last >/dev/null 2>&1; echo $x; done"),
        "a\nb\n"
    );
    assert_eq!(
        ok("echo '[\"x\",\"y\"]' | from-json | while IFS= read -r l; do sort /dev/null; echo \"$l\"; done"),
        "[\n  \"x\",\n  \"y\"\n]\n"
    );
    // A brace group is shell code too, so the system program sorts text.
    assert_eq!(
        ok("echo '[3,1]' | from-json | { sort; }"),
        "  1\n  3,\n[\n]\n"
    );
}

#[test]
fn command_selects_the_program_and_builtin_the_builtin() {
    require!("sort", "date");
    assert_eq!(ok("printf 'b\\na\\n' | command sort"), "a\nb\n");
    let dir = tempfile::tempdir().unwrap();
    // `date now` is jsh syntax, but `command date` is the program, which
    // rejects it as a date to set.
    let run = jsh_in(dir.path(), "command date now");
    assert_eq!((run.stdout.as_str(), run.status), ("", 1));
    assert_eq!(run.stderr, "date: invalid date 'now'\n");
    let run = jsh_in(dir.path(), "builtin date +%Y");
    assert_eq!(run.status, 1);
    assert!(
        run.stderr.contains("unknown subcommand '+%Y'"),
        "{}",
        run.stderr
    );
    // Genuine shell builtins keep their `command` semantics.
    assert_eq!(
        ok("command echo hi; command -V cd"),
        "hi\ncd is a shell builtin\n"
    );
}

#[test]
fn type_and_command_v_report_the_program_and_help_documents_the_rule() {
    require!("sort");
    let sort = program("sort").unwrap();
    let sort = sort.display();
    assert_eq!(ok("command -v sort"), format!("{sort}\n"));
    assert_eq!(ok("command -V sort"), format!("sort is {sort}\n"));
    assert_eq!(ok("type sort"), format!("sort is {sort}\n"));

    let help = ok("help sort");
    assert!(help.starts_with("sort :: list -> list\n"), "{help}");
    assert!(help.contains("Shadows the system `sort`"), "{help}");
    assert!(
        help.contains("`builtin sort` always runs this builtin"),
        "{help}"
    );
    let pipeline_help = ok("help sort | lines | to-json");
    assert!(
        pipeline_help.contains("Shadows the system `sort`"),
        "{pipeline_help}"
    );
    let record: serde_json::Value = serde_json::from_str(&ok("help -r sort")).unwrap();
    assert_eq!(record["shadows"]["program"], "sort");
    assert!(record["shadows"]["builtin_when"]
        .as_str()
        .unwrap()
        .contains("typed values"));
    let record: serde_json::Value = serde_json::from_str(&ok("help -r cd")).unwrap();
    assert!(record.get("shadows").is_none());
}

// ---------------------------------------------------------------------------
// uniq
// ---------------------------------------------------------------------------

#[test]
fn uniq_runs_the_program_for_every_option() {
    require!("uniq");
    assert_eq!(ok("printf 'a\\na\\nb\\n' | uniq -d"), "a\n");
    assert_eq!(ok("printf 'a\\na\\nb\\n' | uniq -u"), "b\n");
    assert_eq!(ok("printf 'a\\nA\\nb\\n' | uniq -i"), "a\nb\n");
    assert_eq!(
        ok("printf 'a\\na\\nb\\n' | uniq -c"),
        "      2 a\n      1 b\n"
    );
    assert_eq!(ok("printf '1 a\\n2 a\\n3 b\\n' | uniq -f 1"), "1 a\n3 b\n");
    assert_eq!(ok("printf 'a\\na\\nb\\n' | uniq -D"), "a\na\n");
    assert_eq!(ok("printf 'a\\na\\nb\\n' > u.txt; uniq u.txt"), "a\nb\n");
    // The line-based builtin stays reachable explicitly.
    assert_eq!(ok("printf 'x\\nx\\n' | builtin uniq -c"), "2 x\n");
}

#[test]
fn shadowed_programs_stream_input_past_the_value_pipeline_bound() {
    require!("yes", "head", "uniq");
    // 70 MB exceeds the 64 MB a value builtin may buffer from a pipe: the
    // program must be reading the pipe itself.
    assert_eq!(ok("yes | head -c 70000000 | uniq -c"), "35000000 y\n");
}

// ---------------------------------------------------------------------------
// date
// ---------------------------------------------------------------------------

#[test]
fn date_runs_the_program_for_coreutils_forms() {
    require!("date", "touch");
    assert_eq!(ok("date -d @0 -u +%Y-%m-%d"), "1970-01-01\n");
    assert_eq!(ok("date -u -d @86400 +%H:%M"), "00:00\n");
    assert_eq!(ok("date -u -d @0 -I"), "1970-01-01\n");
    assert_eq!(ok("date -u -d @0 -R"), "Thu, 01 Jan 1970 00:00:00 +0000\n");
    assert_eq!(ok("date -u -d '2020-01-02 03:04:05' +%s"), "1577934245\n");
    assert_eq!(
        ok("[[ $(date) =~ [0-9]{4} ]] && echo has-year"),
        "has-year\n"
    );
    assert_eq!(ok("[[ $(date +%s) =~ ^[0-9]+$ ]] && echo epoch"), "epoch\n");
    assert_eq!(
        ok("touch -d '2001-02-03 04:05:06' ref; date -r ref +%F"),
        "2001-02-03\n"
    );
    // The program's output feeds a value pipeline as text.
    assert_eq!(ok("date -u -d @0 +%s | into int | describe"), "int\n");
}

#[test]
fn shadowing_commands_never_consume_stdin_they_do_not_read() {
    require!("date", "last");
    // The builtin used to read all of stdin before failing; the program
    // ignores it, leaving it for the next command.
    assert_eq!(
        ok("printf 'hello\\n' | { date -u -d @0 +%Y; cat; }"),
        "1970\nhello\n"
    );
    assert_eq!(
        ok("printf 'hello\\n' | { last -n 1 >/dev/null 2>&1; cat; }"),
        "hello\n"
    );
}

#[test]
fn date_keeps_its_structured_subcommands() {
    let now = ok("date now");
    let now = now.trim();
    assert!(now.ends_with('Z') && now.as_bytes()[10] == b'T', "{now}");
    assert_eq!(
        ok("echo 1700000000 | from-json | date format '%Y-%m-%d %H:%M:%S'"),
        "2023-11-14 22:13:20\n"
    );
    assert_eq!(ok("date format %Y | str length"), "4\n");
    assert_eq!(
        ok("sub=now; date $sub | str length"),
        ok("date now | str length")
    );
}

// ---------------------------------------------------------------------------
// sum, split, last, zip, rename, merge
// ---------------------------------------------------------------------------

#[test]
fn sum_checksums_files_and_still_adds_numbers() {
    require!("sum");
    assert_eq!(
        ok("printf 'hello\\n' > h.txt; sum h.txt"),
        "36979     1 h.txt\n"
    );
    assert_eq!(ok("printf 'hello\\n' | sum"), "36979     1\n");
    assert_eq!(
        ok("printf 'hello\\n' > h.txt; sum -s h.txt"),
        "542 1 h.txt\n"
    );
    assert_eq!(ok("sum 1 2 3.5; sum -1 1"), "6.5\n0\n");
}

#[test]
fn split_writes_files_and_still_splits_values() {
    require!("split", "ls", "tr");
    assert_eq!(
        ok("printf '1\\n2\\n3\\n' > sp.txt; split -l 1 sp.txt part_; ls part_* | tr '\\n' ' '; cat part_ab"),
        "part_aa part_ab part_ac 2\n"
    );
    assert_eq!(
        ok("printf 'abcdef' > b.txt; split -b 2 b.txt bp_; cat bp_ab"),
        "cd"
    );
    // Raw text piped from `echo` keeps its newline, exactly as before.
    assert_eq!(
        squash(&ok("echo a,b | split row , | to-json")),
        r#"["a","b\n"]"#
    );
    assert_eq!(
        squash(&ok(
            "printf 'a,b' > t.txt; open t.txt | split row , | to-json"
        )),
        r#"["a","b"]"#
    );
    assert_eq!(
        squash(&ok(
            "printf 'a,b' > t.txt; open t.txt | split column , x y | to-json"
        )),
        r#"[{"x":"a","y":"b"}]"#
    );
}

#[test]
fn last_is_login_history_unless_counting_piped_rows() {
    require!("last", "seq");
    let dir = tempfile::tempdir().unwrap();
    let direct = Command::new(program("last").unwrap())
        .args(["-n", "1"])
        .env("LC_ALL", "C")
        .env("TZ", "UTC")
        .output()
        .unwrap();
    let run = jsh_in(dir.path(), "last -n 1");
    assert_eq!(run.stdout, String::from_utf8_lossy(&direct.stdout));
    assert_eq!(Some(run.status), direct.status.code());

    assert_eq!(squash(&ok("seq 10 | last 3 | to-json")), "[8,9,10]");
    assert_eq!(squash(&ok("range 1..5 | last 2 | to-json")), "[4,5]");
    let run = jsh_in(dir.path(), "range 1..3 | last nope");
    assert_ne!(run.status, 0);
}

#[test]
fn zip_archives_unless_given_a_json_list() {
    require!("zip");
    let dir = tempfile::tempdir().unwrap();
    let direct = Command::new(program("zip").unwrap())
        .arg("-h")
        .env("LC_ALL", "C")
        .output()
        .unwrap();
    let run = jsh_in(dir.path(), "zip -h");
    assert_eq!(run.stdout, String::from_utf8_lossy(&direct.stdout));
    assert_eq!(Some(run.status), direct.status.code());

    assert_eq!(
        squash(&ok(
            "echo '[\"a\",\"b\"]' | from-json | zip '[1,2]' | to-json"
        )),
        r#"[["a",1],["b",2]]"#
    );
    assert_eq!(
        squash(&ok("echo '[\"a\",\"b\"]' | zip '[1,2]'")),
        r#"[["a",1],["b",2]]"#
    );
}

#[test]
fn rename_and_merge_keep_their_table_forms() {
    assert_eq!(
        squash(&ok("echo '[{\"a\":1}]' | from-json | rename a b | to-json")),
        r#"[{"b":1}]"#
    );
    assert_eq!(
        squash(&ok(
            "echo '[{\"a\":1}]' | from-json | merge '{\"b\":2}' | to-json"
        )),
        r#"[{"a":1,"b":2}]"#
    );
}

// ---------------------------------------------------------------------------
// No program installed
// ---------------------------------------------------------------------------

#[test]
fn builtins_keep_their_behavior_when_no_program_is_installed() {
    let dir = tempfile::tempdir().unwrap();
    let empty = dir.path().join("empty-bin");
    std::fs::create_dir(&empty).unwrap();
    let path = empty.to_str().unwrap();
    let run = |script: &str| jsh_with_path(dir.path(), path, script);

    let sorted = run("printf '[2,1]' | sort");
    assert_eq!(
        (squash(&sorted.stdout).as_str(), sorted.status),
        ("[1,2]", 0)
    );
    let sorted = run("printf '[2,1]' | command sort");
    assert_eq!(squash(&sorted.stdout), "[1,2]");
    let date = run("date +%Y");
    assert_eq!(date.status, 1);
    assert!(
        date.stderr.contains("unknown subcommand '+%Y'"),
        "{}",
        date.stderr
    );
    assert_eq!(run("printf 'x\\nx\\n' | uniq -c").stdout, "2 x\n");
    assert_eq!(run("sum 1 2").stdout, "3\n");
    assert_eq!(run("type sort").stdout, "sort is a shell builtin\n");
    assert_eq!(run("command -v sort").stdout, "sort\n");
}
