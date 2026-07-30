use jsh::parser::ast::*;
use jsh::parser::parse;

#[test]
fn test_simple_command() {
    let cmds = parse("echo hello").unwrap();
    assert_eq!(cmds.len(), 1);
    assert!(!cmds[0].background);
    assert!(!cmds[0].disown);
}

#[test]
fn test_pipeline() {
    let cmds = parse("ls | grep foo").unwrap();
    assert_eq!(cmds.len(), 1);
    let pipeline = &cmds[0].list.first;
    assert_eq!(pipeline.commands.len(), 2);
    assert!(!pipeline.negated);
}

#[test]
fn test_negated_pipeline() {
    let cmds = parse("! ls").unwrap();
    assert!(cmds[0].list.first.negated);
}

#[test]
fn test_and_or() {
    let cmds = parse("true && echo yes || echo no").unwrap();
    assert_eq!(cmds[0].list.rest.len(), 2);
    assert_eq!(cmds[0].list.rest[0].0, Connector::And);
    assert_eq!(cmds[0].list.rest[1].0, Connector::Or);
}

#[test]
fn test_background() {
    let cmds = parse("sleep 10 &").unwrap();
    assert!(cmds[0].background);
    assert!(!cmds[0].disown);
}

#[test]
fn test_background_disown() {
    let cmds = parse("sleep 10 &!").unwrap();
    assert!(cmds[0].background);
    assert!(cmds[0].disown);
}

#[test]
fn test_if_statement() {
    let cmds = parse("if true\nthen echo yes\nfi").unwrap();
    assert_eq!(cmds.len(), 1);
    if let Command::Compound(CompoundCommand::If {
        conditions,
        else_branch,
        ..
    }) = &cmds[0].list.first.commands[0]
    {
        assert_eq!(conditions.len(), 1);
        assert!(else_branch.is_none());
    } else {
        panic!("expected if compound command");
    }
}

#[test]
fn test_for_loop() {
    let cmds = parse("for i in a b c\ndo echo $i\ndone").unwrap();
    if let Command::Compound(CompoundCommand::For { var, words, .. }) =
        &cmds[0].list.first.commands[0]
    {
        assert_eq!(var, "i");
        assert!(words.is_some());
        assert_eq!(words.as_ref().unwrap().len(), 3);
    } else {
        panic!("expected for compound command");
    }
}

#[test]
fn test_while_loop() {
    let cmds = parse("while true\ndo echo loop\ndone").unwrap();
    if let Command::Compound(CompoundCommand::While { .. }) = &cmds[0].list.first.commands[0] {
        // OK
    } else {
        panic!("expected while compound command");
    }
}

#[test]
fn test_case_statement() {
    let cmds = parse("case x in a) echo a;; b) echo b;; esac").unwrap();
    if let Command::Compound(CompoundCommand::Case { arms, .. }) = &cmds[0].list.first.commands[0] {
        assert_eq!(arms.len(), 2);
    } else {
        panic!("expected case compound command");
    }
}

#[test]
fn test_function_def() {
    let cmds = parse("foo() { echo hello; }").unwrap();
    if let Command::FunctionDef { name, .. } = &cmds[0].list.first.commands[0] {
        assert_eq!(name, "foo");
    } else {
        panic!("expected function def");
    }
}

#[test]
fn test_subshell() {
    let cmds = parse("(echo hello)").unwrap();
    if let Command::Compound(CompoundCommand::Subshell { .. }) = &cmds[0].list.first.commands[0] {
        // OK
    } else {
        panic!("expected subshell");
    }
}

#[test]
fn test_brace_group() {
    let cmds = parse("{ echo hello; }").unwrap();
    if let Command::Compound(CompoundCommand::BraceGroup { .. }) = &cmds[0].list.first.commands[0] {
        // OK
    } else {
        panic!("expected brace group");
    }
}

#[test]
fn test_assignment() {
    let cmds = parse("FOO=bar").unwrap();
    if let Command::Simple(ref sc) = cmds[0].list.first.commands[0] {
        assert_eq!(sc.assignments.len(), 1);
        assert_eq!(sc.assignments[0].name, "FOO");
        assert!(!sc.assignments[0].append);
    } else {
        panic!("expected simple command");
    }
}

#[test]
fn test_append_assignment() {
    let cmds = parse("FOO+=bar").unwrap();
    if let Command::Simple(ref sc) = cmds[0].list.first.commands[0] {
        assert_eq!(sc.assignments.len(), 1);
        assert_eq!(sc.assignments[0].name, "FOO");
        assert!(sc.assignments[0].append);
    } else {
        panic!("expected simple command");
    }
}

#[test]
fn test_semicolon_separated() {
    let cmds = parse("echo a; echo b; echo c").unwrap();
    assert_eq!(cmds.len(), 3);
}

#[test]
fn test_redirect() {
    let cmds = parse("echo hello > /tmp/out").unwrap();
    if let Command::Simple(ref sc) = cmds[0].list.first.commands[0] {
        assert_eq!(sc.redirects.len(), 1);
        assert_eq!(sc.redirects[0].kind, RedirectKind::Output);
    } else {
        panic!("expected simple command");
    }
}

#[test]
fn test_is_incomplete() {
    use jsh::parser::is_incomplete;
    assert!(is_incomplete("echo 'hello"));
    assert!(is_incomplete("echo \"hello"));
    assert!(is_incomplete("echo hello |"));
    assert!(is_incomplete("echo hello &&"));
    assert!(is_incomplete("if true; then"));
    assert!(!is_incomplete("echo hello"));
    assert!(!is_incomplete(""));
}

#[test]
fn test_word_parts_variable() {
    use jsh::parser::parse_word_parts;
    let parts = parse_word_parts("$HOME");
    assert!(matches!(&parts[0], WordPart::Variable(v) if v == "HOME"));
}

#[test]
fn test_word_parts_command_sub() {
    use jsh::parser::parse_word_parts;
    let parts = parse_word_parts("$(echo hi)");
    assert!(matches!(&parts[0], WordPart::CommandSub(c) if c == "echo hi"));
}

#[test]
fn test_word_parts_tilde() {
    use jsh::parser::parse_word_parts;
    let parts = parse_word_parts("~/foo");
    assert!(matches!(&parts[0], WordPart::Tilde(u) if u.is_empty()));
}

#[test]
fn test_word_parts_single_quoted() {
    use jsh::parser::parse_word_parts;
    let parts = parse_word_parts("'hello world'");
    assert!(matches!(&parts[0], WordPart::SingleQuoted(s) if s == "hello world"));
}

#[test]
fn test_word_parts_glob() {
    use jsh::parser::parse_word_parts;
    let parts = parse_word_parts("*.txt");
    assert!(matches!(&parts[0], WordPart::Glob(g) if g == "*"));
}

#[test]
fn test_word_parts_process_sub() {
    use jsh::parser::parse_word_parts;
    let parts = parse_word_parts("<(echo hi)");
    assert!(matches!(&parts[0], WordPart::ProcessSub(c, ProcessSubKind::Input) if c == "echo hi"));
}

#[test]
fn test_word_parts_arithmetic() {
    use jsh::parser::parse_word_parts;
    let parts = parse_word_parts("$((1+2))");
    assert!(matches!(&parts[0], WordPart::Arithmetic(e) if e == "1+2"));
}
