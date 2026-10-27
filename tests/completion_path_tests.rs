//! Completion candidates are shell source: inserting one must still name the
//! exact file that was displayed, including quotes and expansion characters.
use jsh::completer::{clear_cache, complete, Completion};
use jsh::environment::{CompletionSpec, ShellState};
use std::fs;
use std::process::{Command, Stdio};

fn candidates(buffer: &str, state: &mut ShellState) -> Vec<Completion> {
    clear_cache();
    complete(buffer, buffer.len(), state).1
}

fn expanded_argument(candidate: &str, cwd: &std::path::Path) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_jsh"))
        .args(["-c", &format!("printf '%s' {candidate}")])
        .current_dir(cwd)
        .env("JSH_COMPLETION_LITERAL", "expanded-in-error")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(output.status.success(), "{candidate}: {output:?}");
    assert!(output.stderr.is_empty(), "{candidate}: {output:?}");
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn completed_filenames_remain_literal_when_executed() {
    let temp = tempfile::tempdir().unwrap();
    let names = [
        "report notes.txt",
        "report$JSH_COMPLETION_LITERAL.txt",
        "report$(touch executed).txt",
        "report`touch executed`.txt",
        "report\\notes.txt",
        "report'notes.txt",
        "report\"notes.txt",
        "report[1]*?.txt",
        "report中文🦀.txt",
    ];
    for name in names {
        fs::write(temp.path().join(name), "").unwrap();
    }
    let mut state = ShellState::new(false);
    for quote in ["", "'", "\""] {
        let buffer = format!("cat {quote}{}/report", temp.path().display());
        let results = candidates(&buffer, &mut state);
        assert_eq!(results.len(), names.len(), "{buffer}: {results:?}");
        for name in names {
            let item = results.iter().find(|item| item.display == name).unwrap();
            let actual = expanded_argument(&item.text, temp.path());
            assert!(
                !temp.path().join("executed").exists(),
                "completion executed filename syntax: {}",
                item.text
            );
            assert_eq!(actual, temp.path().join(name).to_str().unwrap());
        }
    }
}

#[test]
fn quotes_inside_a_path_and_assignment_are_completed_without_changing_the_path() {
    let temp = tempfile::tempdir().unwrap();
    let name = "report's $literal.txt";
    fs::write(temp.path().join(name), "").unwrap();
    let mut state = ShellState::new(false);
    for quote in ['\'', '"'] {
        let buffer = format!("cat {}/{quote}report", temp.path().display());
        let results = candidates(&buffer, &mut state);
        assert_eq!(results.len(), 1, "{buffer}: {results:?}");
        assert_eq!(
            expanded_argument(&results[0].text, temp.path()),
            temp.path().join(name).to_str().unwrap()
        );

        let buffer = format!("export JSH_PATH={quote}{}/report", temp.path().display());
        let results = candidates(&buffer, &mut state);
        assert_eq!(results.len(), 1, "{buffer}: {results:?}");
        let value = results[0].text.strip_prefix("JSH_PATH=").unwrap();
        assert_eq!(
            expanded_argument(value, temp.path()),
            temp.path().join(name).to_str().unwrap()
        );
    }
}

#[test]
fn double_quoted_directory_prefix_preserves_its_backslashes() {
    let temp = tempfile::tempdir().unwrap();
    let directory = temp.path().join("my\\dir");
    fs::create_dir(&directory).unwrap();
    fs::write(directory.join("file.txt"), "").unwrap();
    let buffer = format!("cat \"{}/my\\dir/fi", temp.path().display());
    let mut state = ShellState::new(false);
    let results = candidates(&buffer, &mut state);
    assert_eq!(results.len(), 1, "{results:?}");
    assert_eq!(
        expanded_argument(&results[0].text, temp.path()),
        directory.join("file.txt").to_str().unwrap()
    );
}

#[test]
fn quoted_variable_prefix_stays_expandable_but_generated_names_do_not() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("file$JSH_COMPLETION_LITERAL.txt"), "").unwrap();
    let mut state = ShellState::new(false);
    state.globals.insert(
        "JSH_COMPLETION_BASE".to_string(),
        jsh::variables::Variable::exported(temp.path().to_str().unwrap().to_string()),
    );
    let buffer = "cat \"$JSH_COMPLETION_BASE/fi";
    let results = candidates(buffer, &mut state);
    assert_eq!(results.len(), 1, "{results:?}");
    let text = &results[0].text;
    assert!(text.starts_with("\"$JSH_COMPLETION_BASE/"), "{text}");
    let output = Command::new(env!("CARGO_BIN_EXE_jsh"))
        .args(["-c", &format!("printf '%s' {text}")])
        .env("JSH_COMPLETION_BASE", temp.path())
        .env("JSH_COMPLETION_LITERAL", "wrong")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        temp.path()
            .join("file$JSH_COMPLETION_LITERAL.txt")
            .to_str()
            .unwrap()
    );
}

#[test]
fn quoted_and_escaped_leading_tildes_are_literal_directories() {
    let temp = tempfile::Builder::new()
        .prefix("~jsh-completion-")
        .tempdir_in(".")
        .unwrap();
    fs::write(temp.path().join("file.txt"), "").unwrap();
    let directory = temp.path().file_name().unwrap().to_str().unwrap();
    let mut state = ShellState::new(false);
    for prefix in [format!("'{directory}/fi"), format!("\\{directory}/fi")] {
        let results = candidates(&format!("cat {prefix}"), &mut state);
        assert_eq!(results.len(), 1, "{prefix}: {results:?}");
    }
}

#[cfg(unix)]
#[test]
fn directory_symlinks_are_traversable_completion_candidates() {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("target")).unwrap();
    fs::write(temp.path().join("target/file.txt"), "").unwrap();
    std::os::unix::fs::symlink("target", temp.path().join("linked")).unwrap();
    let mut state = ShellState::new(false);
    let buffer = format!("cd {}/lin", temp.path().display());
    let results = candidates(&buffer, &mut state);
    assert_eq!(results.len(), 1, "{results:?}");
    assert!(results[0].is_dir);
    assert!(results[0].text.ends_with("/linked/"));
    let buffer = format!("cat {}fi", results[0].text);
    let results = candidates(&buffer, &mut state);
    assert_eq!(results.len(), 1);
    assert!(results[0].text.ends_with("/linked/file.txt"));
}

#[test]
fn clobber_redirection_completes_paths_instead_of_pipeline_commands() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("output.txt"), "").unwrap();
    let mut state = ShellState::new(false);
    for command in ["cd", "git add", "echo hello"] {
        let buffer = format!("{command} >| {}/out", temp.path().display());
        let results = candidates(&buffer, &mut state);
        assert_eq!(results.len(), 1, "{buffer}: {results:?}");
        assert!(results[0].text.ends_with("/output.txt"));
    }
}

#[test]
fn directory_descriptions_do_not_count_unbounded_children() {
    let temp = tempfile::tempdir().unwrap();
    let directory = temp.path().join("large");
    fs::create_dir(&directory).unwrap();
    for index in 0..270 {
        fs::write(directory.join(format!("child_{index}")), "").unwrap();
    }
    let mut state = ShellState::new(false);
    let results = candidates(&format!("cd {}/lar", temp.path().display()), &mut state);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].description.as_deref(), Some("256+ items"));
}

#[cfg(unix)]
#[test]
fn non_utf8_filenames_are_not_offered_as_different_lossy_names() {
    use std::os::unix::ffi::OsStringExt;
    let temp = tempfile::tempdir().unwrap();
    fs::write(
        temp.path()
            .join(std::ffi::OsString::from_vec(b"file_\xff".to_vec())),
        "",
    )
    .unwrap();
    fs::write(temp.path().join("file_valid"), "").unwrap();
    let mut state = ShellState::new(false);
    let results = candidates(&format!("cat {}/file_", temp.path().display()), &mut state);
    assert_eq!(results.len(), 1, "{results:?}");
    assert!(results[0].text.ends_with("/file_valid"));
}

#[test]
fn a_precise_prefix_can_find_a_file_after_the_first_4096_directory_entries() {
    let temp = tempfile::tempdir().unwrap();
    for index in 0..4100 {
        fs::write(temp.path().join(format!("file_{index:05}")), "").unwrap();
    }
    // Directory enumeration order is filesystem-dependent; choose a name
    // actually beyond the old scan limit instead of assuming lexical order.
    let name = fs::read_dir(temp.path())
        .unwrap()
        .nth(4097)
        .unwrap()
        .unwrap()
        .file_name()
        .into_string()
        .unwrap();
    let mut state = ShellState::new(false);
    let buffer = format!("cat {}/{name}", temp.path().display());
    let results = candidates(&buffer, &mut state);
    assert_eq!(results.len(), 1, "{buffer}: {results:?}");
    assert!(results[0].text.ends_with(&name));
}

fn completion_function(state: &mut ShellState, body: &str) {
    state.functions.insert(
        "_jsh_test_complete".into(),
        jsh::parser::ast::CompoundCommand::BraceGroup {
            body: jsh::parser::parse(body).unwrap(),
            redirects: Vec::new(),
        },
    );
    state.completion_specs.insert(
        "jsh-test-command".into(),
        CompletionSpec {
            command: "jsh-test-command".into(),
            word_list: None,
            function: Some("_jsh_test_complete".into()),
            directory: false,
            file: false,
            actions: Vec::new(),
            glob_pattern: None,
            filter_pattern: None,
            prefix: None,
            suffix: None,
        },
    );
}

#[test]
fn completion_function_receives_quote_aware_words_and_can_return() {
    let mut state = ShellState::new(false);
    completion_function(
        &mut state,
        r#"COMPREPLY=( "words:${#COMP_WORDS[@]}" "cword:$COMP_CWORD" "quoted:${COMP_WORDS[1]}" "prev:$3" ); return 0; COMPREPLY=(incorrect)"#,
    );
    let results = candidates(
        r#"jsh-test-command "staging name" \ leading\ space "#,
        &mut state,
    );
    let texts: Vec<_> = results.iter().map(|item| item.text.as_str()).collect();
    assert!(texts.contains(&"words:4"), "{texts:?}");
    assert!(texts.contains(&"cword:3"), "{texts:?}");
    assert!(texts.contains(&"quoted:\"staging name\""), "{texts:?}");
    assert!(texts.contains(&"prev:\\ leading\\ space"), "{texts:?}");
    assert!(!texts.contains(&"incorrect"), "{texts:?}");
    assert!(!state.return_requested);
}

#[test]
fn completion_function_does_not_clobber_the_previous_command_state() {
    let mut state = ShellState::new(false);
    completion_function(&mut state, "COMPREPLY=(ready); return 0");
    state.last_exit_code = 42;
    state.pipestatus = vec![42, 7];
    state.positional_params = vec!["original argument".into()];
    state.set_array("COMP_WORDS", vec!["saved words".into()]);
    state.set_array("COMPREPLY", vec!["saved replies".into()]);
    let results = candidates("jsh-test-command ", &mut state);
    assert!(results.iter().any(|item| item.text == "ready"));
    assert_eq!(state.last_exit_code, 42);
    assert_eq!(state.pipestatus, vec![42, 7]);
    assert_eq!(state.positional_params, vec!["original argument"]);
    assert_eq!(state.array_values("COMP_WORDS"), vec!["saved words"]);
    assert_eq!(state.array_values("COMPREPLY"), vec!["saved replies"]);
    assert_eq!(state.return_depth, 0);
    assert!(!state.return_requested);
    assert!(state.local_vars_stack.is_empty());
}
