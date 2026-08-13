use std::process::Command;

fn run(script: &str) -> (String, String, i32) {
    let output = Command::new(env!("CARGO_BIN_EXE_jsh"))
        .arg("-c")
        .arg(script)
        .output()
        .expect("run jsh");
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
        output.status.code().unwrap_or(-1),
    )
}

#[test]
fn formerly_missing_value_commands_have_reachable_help() {
    let (output, error, status) = run("help def; help reverse");

    assert_eq!(status, 0, "{error}");
    assert!(output.contains("def ::"), "{output}");
    assert!(output.contains("reverse ::"), "{output}");
}

#[test]
fn every_catalog_command_is_reachable_through_help() {
    let commands = jsh::command_catalog::all_names()
        .iter()
        .map(|name| {
            assert!(!name.contains('\''));
            format!("help '{name}' >/dev/null")
        })
        .collect::<Vec<_>>()
        .join("; ");
    let (output, error, status) = run(&format!("set -e; {commands}"));

    assert_eq!(status, 0, "stdout: {output}\nstderr: {error}");
}

#[test]
fn compgen_builtin_discovers_value_commands_without_signatures_gap() {
    let (output, error, status) = run("compgen -A builtin def; compgen -A builtin reve");

    assert_eq!(status, 0, "{error}");
    let candidates: Vec<&str> = output.lines().collect();
    assert!(candidates.contains(&"def"), "{output}");
    assert!(candidates.contains(&"reverse"), "{output}");
}

#[test]
fn catalog_names_are_stable_and_context_routes_remain_external_by_default() {
    let names = jsh::command_catalog::all_names();
    assert!(names.windows(2).all(|pair| pair[0] < pair[1]));
    assert!(names.contains(&"ls"));
    assert!(names.contains(&"ps"));
    assert!(!jsh::command_catalog::is_builtin("ls"));
    assert!(!jsh::command_catalog::is_builtin("ps"));
}

#[test]
fn classic_help_keeps_usage_and_record_schema_is_shared() {
    let (text, error, status) = run("help cd");
    assert_eq!(status, 0, "{error}");
    assert!(text.contains("Usage: cd [-] [dir]"), "{text}");
    assert!(text.contains("previous directory"), "{text}");

    let (classic, error, status) = run("help -r cd");
    assert_eq!(status, 0, "{error}");
    let (pipeline, error, status) = run("help -r cd | to-json");
    assert_eq!(status, 0, "{error}");
    let classic: serde_json::Value = serde_json::from_str(&classic).unwrap();
    let pipeline: serde_json::Value = serde_json::from_str(&pipeline).unwrap();
    assert_eq!(classic, pipeline[0]);
    assert_eq!(classic["canonical_name"], "cd");
    assert_eq!(classic["usage"], "cd [-] [dir]");

    let (signed, error, status) = run("help -r each");
    assert_eq!(status, 0, "{error}");
    let signed: serde_json::Value = serde_json::from_str(&signed).unwrap();
    assert_eq!(signed["canonical_name"], "each");
    assert_eq!(signed["value_route"], "always");
    assert_eq!(signed["signature"]["name"], "each");
}

#[test]
fn help_completion_includes_typed_user_functions() {
    let (output, error, status) = run("def catalog_user_fn [value:string] {|value| $value }; \
         debug-completion help catalog_user");
    assert_eq!(status, 0, "{error}");
    assert!(output.contains("catalog_user_fn"), "{output}");
}

#[test]
fn user_defined_record_help_has_one_schema_on_both_routes() {
    let definition = "def catalog_user_fn [value:string rest...:int] \
                      {|value, rest| $value }";
    let (classic, error, status) = run(&format!("{definition}; help -r catalog_user_fn"));
    assert_eq!(status, 0, "{error}");
    let (pipeline, error, status) =
        run(&format!("{definition}; help -r catalog_user_fn | to-json"));
    assert_eq!(status, 0, "{error}");

    let classic: serde_json::Value = serde_json::from_str(&classic).unwrap();
    let pipeline: serde_json::Value = serde_json::from_str(&pipeline).unwrap();
    assert_eq!(classic, pipeline[0]);
    assert_eq!(classic["user_defined"], true);
    assert!(classic["params"].is_array());
    assert_eq!(classic["params"][0]["name"], "value");
}
