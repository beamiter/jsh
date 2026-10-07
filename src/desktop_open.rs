//! Open a standalone interactive path with the desktop's default application.
use crate::environment::ShellState;
use crate::parser::ast::{Command, CompleteCommand, Word, WordPart};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};

/// This is a REPL convenience, deliberately outside the script executor. Only
/// one simple command without assignments, redirects, or shell operators can
/// be a path. Recognizing it must never execute an expansion a second time.
pub(crate) fn path_to_open(
    source: &str,
    commands: &[CompleteCommand],
    state: &mut ShellState,
) -> Option<PathBuf> {
    if !state.interactive {
        return None;
    }
    let [command] = commands else { return None };
    if command.background || command.disown || !command.list.rest.is_empty() {
        return None;
    }
    let pipeline = &command.list.first;
    if pipeline.negated {
        return None;
    }
    let [Command::Simple(simple)] = pipeline.commands.as_slice() else {
        return None;
    };
    if !simple.assignments.is_empty() || !simple.redirects.is_empty() {
        return None;
    }
    if simple.words.is_empty() || !simple.words.iter().all(|word| is_path_word(word, state)) {
        return None;
    }

    let head = crate::expand::expand_word_to_string(&simple.words[0], state);
    // Preserve ordinary shell resolution, including ./executables. Prefix a
    // colliding document name with ./ to explicitly select the local file.
    if crate::builtins::is_builtin(&head)
        || state.aliases.contains_key(&head)
        || state.functions.contains_key(&head)
        || state.user_typed_fns.contains_key(&head)
        || crate::builtins::find_in_path(&head).is_some()
    {
        return None;
    }

    let path = if simple.words.len() == 1 {
        // Expand quoting, ~ and scalar variables using the shell's normal word
        // rules; a glob, substitution, or multi-field expansion is not a path.
        let expanded = crate::expand::expand_word(&simple.words[0], state);
        let [path] = expanded.as_slice() else {
            return None;
        };
        PathBuf::from(path)
    } else {
        // Also accept a literally typed filename containing spaces. Do not
        // combine actual argv or reinterpret any shell syntax as a filename.
        if !simple
            .words
            .iter()
            .flatten()
            .all(|part| matches!(part, WordPart::Literal(_)))
        {
            return None;
        }
        PathBuf::from(source.trim())
    };
    let metadata = path.metadata().ok()?;
    if !metadata.is_file() && !metadata.is_dir() {
        return None;
    }
    // An absolute argument cannot become an opener option (e.g. -report.pdf)
    // or a URL, and remains correct if the user changes cwd after launching it.
    if path.is_absolute() {
        Some(path)
    } else {
        Some(std::env::current_dir().ok()?.join(path))
    }
}

fn is_path_word(word: &Word, state: &ShellState) -> bool {
    word.iter().all(|part| match part {
        WordPart::Literal(_) | WordPart::SingleQuoted(_) | WordPart::Tilde(_) => true,
        WordPart::DoubleQuoted(parts) => is_path_word(parts, state),
        // Parameter operators can hide substitutions or assignments. Only
        // ordinary, set scalar names are safe to inspect before execution.
        WordPart::Variable(name) => {
            let mut chars = name.chars();
            chars
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
                && state.get_var(name).is_some()
        }
        _ => false,
    })
}

pub(crate) fn open(path: &Path, state: &ShellState) -> i32 {
    #[cfg(target_os = "macos")]
    let helper_name = "open";
    #[cfg(not(target_os = "macos"))]
    let helper_name = "xdg-open";

    let Some(helper) = crate::io_guard::trusted_helper(helper_name) else {
        eprintln!(
            "jsh: cannot open {}: {helper_name} is unavailable",
            path.display()
        );
        return 127;
    };
    match launch(&helper, path, state) {
        Ok(mut child) => {
            // Some desktop launchers live as long as the application. Keep the
            // prompt usable and reap the launcher without borrowing the PTY's
            // input or keeping it in the shell's foreground process group.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            0
        }
        Err(error) => {
            eprintln!("jsh: cannot open {}: {error}", path.display());
            126
        }
    }
}

fn launch(helper: &Path, path: &Path, state: &ShellState) -> std::io::Result<Child> {
    use std::os::unix::process::CommandExt;
    let mut command = std::process::Command::new(helper);
    state.configure_command_environment(&mut command);
    command
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .process_group(0)
        .spawn()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn resolve(source: &str, state: &mut ShellState) -> Option<PathBuf> {
        path_to_open(source, &crate::parser::parse(source).unwrap(), state)
    }

    #[test]
    fn standalone_files_folders_and_quoted_paths_are_openable() {
        let temp = tempfile::tempdir().unwrap();
        let mut state = ShellState::new(true);
        state.home_dir = temp.path().to_path_buf();
        for name in [
            "report.txt",
            "中文报告.pdf",
            "two  spaces.txt",
            "-option.txt",
            "literal;name.txt",
        ] {
            let path = temp.path().join(name);
            fs::write(&path, "document").unwrap();
            assert_eq!(
                resolve(&format!("'{}'", path.display()), &mut state),
                Some(path)
            );
        }
        let directory = temp.path().join("documents");
        fs::create_dir(&directory).unwrap();
        assert_eq!(resolve("~/documents", &mut state), Some(directory));
        let path = temp.path().join("two  spaces.txt");
        assert_eq!(
            resolve(path.to_str().unwrap(), &mut state),
            Some(path.clone())
        );
        state.set_var("JSH_TEST_DOCUMENT", path.to_str().unwrap());
        assert_eq!(resolve("\"$JSH_TEST_DOCUMENT\"", &mut state), Some(path));
    }

    #[test]
    fn commands_scripts_and_shell_syntax_keep_normal_execution() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("document.txt");
        fs::write(&file, "document").unwrap();
        let source = file.display().to_string();
        let side_effect = temp.path().join("should-not-run");
        let mut state = ShellState::new(true);
        for source in [
            format!("{source} arg"),
            format!("{source} | cat"),
            format!("{source} && true"),
            format!("{source} || true"),
            format!("{source}; true"),
            format!("{source} &"),
            format!("! {source}"),
            format!("{source} > output"),
            format!("VAR=value {source}"),
            format!("$(touch {})", side_effect.display()),
            format!("${{unset:-$(touch {})}}", side_effect.display()),
            "${unset:=value}".into(),
            "$((counter++))".into(),
            format!("{}/*", temp.path().display()),
        ] {
            assert_eq!(resolve(&source, &mut state), None, "{source}");
        }
        assert!(state.get_var("unset").is_none());
        assert!(!side_effect.exists());
        assert!(state.get_var("counter").is_none());
        state.interactive = false;
        assert_eq!(resolve(&source, &mut state), None);
        state.interactive = true;
        fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(resolve(&source, &mut state), None);
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        state.aliases.insert(source.clone(), "echo alias".into());
        assert_eq!(resolve(&source, &mut state), None);
        state.aliases.clear();
        state.user_typed_fns.insert(
            source.clone(),
            std::sync::Arc::new(crate::value::ClosureData {
                params: vec![],
                body_src: String::new(),
                captured: Default::default(),
            }),
        );
        assert_eq!(resolve(&source, &mut state), None);
    }

    #[test]
    fn launcher_gets_one_literal_argument_and_live_shell_environment() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("space ; $name.txt");
        let marker = temp.path().join("opened");
        fs::write(
            &path,
            "printf '%s' \"$0:$JSH_TEST_VALUE\" > \"$JSH_TEST_MARKER\"\n",
        )
        .unwrap();
        let mut state = ShellState::new(false);
        state.set_var("JSH_TEST_VALUE", "live-state");
        state.set_var("JSH_TEST_MARKER", marker.to_str().unwrap());
        let status = launch(Path::new("/bin/sh"), &path, &state)
            .unwrap()
            .wait()
            .unwrap();
        assert!(status.success());
        assert_eq!(
            fs::read_to_string(marker).unwrap(),
            format!("{}:live-state", path.display())
        );
    }
}
