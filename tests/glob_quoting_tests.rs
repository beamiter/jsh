//! Quoting must suppress pathname expansion, and quote characters that arrive as
//! *data* must not suppress it.
//!
//! Both directions were broken because glob eligibility was re-derived from the
//! already-expanded text: `contains_glob` scanned for `'`, `"` and `\` to guess
//! what had been quoted. By that point the parser has consumed the real quotes,
//! so the scan saw only data. The consequences:
//!
//!   * `echo '*'` expanded to the directory listing — quoting a metacharacter,
//!     the documented way to pass one through, did nothing. `"*"` and `\*` too.
//!   * `x="it's"; echo $x*` stopped globbing, because the apostrophe in the
//!     *filename* looked like an opening quote.
//!
//! The first is the load-bearing one: it means a quoted argument can silently
//! turn into many arguments, which is the failure mode
//! `jterm_core::process::shell_quote_argv_for` refuses to risk when it declines
//! to replay a command into an unrecognised shell.
//!
//! Eligibility is now tracked per byte, from the parse tree, through word
//! splitting, into the matcher. These tests pin both directions.

use std::path::Path;
use std::process::{Command, Stdio};

/// Fixture set shared by every case. Deliberately includes names containing a
/// literal `*`, an apostrophe and a double quote.
const FIXTURES: &[&str] = &["aa", "ab", "a b", ".hidden", "it's-f", "q\"f", "star*lit"];

/// Run `script` under jsh in a throwaway directory holding `FIXTURES`, with
/// byte-order collation so the expected orderings are locale-independent.
fn run_in_sandbox(script: &str) -> String {
    let dir = tempfile::tempdir().expect("tempdir");
    for name in FIXTURES {
        std::fs::write(dir.path().join(name), b"x").expect("fixture");
    }
    std::fs::create_dir(dir.path().join("sub")).expect("subdir");
    std::fs::write(dir.path().join("sub").join("x"), b"x").expect("fixture");
    run_in(dir.path(), script)
}

fn run_in(dir: &Path, script: &str) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_jsh"))
        .arg("-c")
        .arg(script)
        .current_dir(dir)
        .env("LC_ALL", "C")
        .env("LANG", "C")
        .stdin(Stdio::null())
        .output()
        .expect("spawn jsh");
    assert!(
        out.stderr.is_empty(),
        "unexpected stderr for {script:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Every field, bracketed, so a one-field result is distinguishable from a
/// many-field one. That distinction *is* the bug: quoting failing here does not
/// merely mangle a string, it changes the argument count.
const SHOW: &str = r#"printf "[%s]" "#;

fn fields(argument: &str) -> String {
    run_in_sandbox(&format!("{SHOW}{argument}; echo"))
}

// ---------------------------------------------------------------------------
// Quoting suppresses globbing
// ---------------------------------------------------------------------------

#[test]
fn single_quotes_suppress_every_metacharacter() {
    assert_eq!(fields("'*'"), "[*]\n");
    assert_eq!(fields("'a?'"), "[a?]\n");
    assert_eq!(fields("'a[ab]'"), "[a[ab]]\n");
    // A name that really exists and really contains a star: quoting it must
    // match that one file, not re-expand the star.
    assert_eq!(fields("'star*lit'"), "[star*lit]\n");
}

#[test]
fn double_quotes_suppress_globbing() {
    assert_eq!(fields("\"*\""), "[*]\n");
    assert_eq!(fields("\"a?\""), "[a?]\n");
}

/// The parser folds `\*` into a `Literal`, so this shares its protection with
/// quoted text rather than being a separate rule.
#[test]
fn backslash_escapes_suppress_globbing() {
    assert_eq!(fields("\\*"), "[*]\n");
    assert_eq!(fields("a\\*b"), "[a*b]\n");
}

#[test]
fn quoting_an_expansion_suppresses_globbing() {
    assert_eq!(
        run_in_sandbox("x='*'; printf \"[%s]\" \"$x\"; echo"),
        "[*]\n"
    );
    assert_eq!(
        run_in_sandbox("printf \"[%s]\" \"$(echo '*')\"; echo"),
        "[*]\n"
    );
}

/// `"$@"` and `"${arr[@]}"` take a different code path from ordinary words —
/// they produce fields directly instead of being split — so they need their own
/// coverage. A quoted `*` here previously became the whole directory listing,
/// which silently changed the argument count of every forwarded command
/// (`cmd "$@"`).
#[test]
fn quoted_positional_and_array_fields_are_literal() {
    assert_eq!(
        run_in_sandbox("set -- '*' b; printf \"[%s]\" \"$@\"; echo"),
        "[*][b]\n"
    );
    assert_eq!(
        run_in_sandbox("arr=('*' b); printf \"[%s]\" \"${arr[@]}\"; echo"),
        "[*][b]\n"
    );
    assert_eq!(
        run_in_sandbox("set -- '*'; printf \"[%s]\" \"$*\"; echo"),
        "[*]\n"
    );
}

/// Assignment is where the old bug did the most damage: `arr=('*')` stored the
/// expanded listing, so every later use was already wrong no matter how
/// carefully it was quoted.
#[test]
fn assignment_does_not_glob_a_quoted_value() {
    assert_eq!(
        run_in_sandbox("arr=('*'); printf \"%s\" \"${#arr[@]}\"; echo"),
        "1\n"
    );
    assert_eq!(
        run_in_sandbox("set -- '*'; printf \"%s\" \"$#\"; echo"),
        "1\n"
    );
}

// ---------------------------------------------------------------------------
// Globbing still happens where it must
// ---------------------------------------------------------------------------

#[test]
fn unquoted_metacharacters_still_glob() {
    assert_eq!(fields("a*"), "[a b][aa][ab]\n");
    assert_eq!(fields("a?"), "[aa][ab]\n");
    assert_eq!(fields("a[ab]"), "[aa][ab]\n");
    assert_eq!(fields("sub/*"), "[sub/x]\n");
}

/// Bash globs the *result* of an unquoted expansion, so this must keep working;
/// it is the case that makes "just don't glob expansion output" wrong as a fix.
#[test]
fn unquoted_expansions_are_still_glob_eligible() {
    assert_eq!(
        run_in_sandbox("x='a*'; printf \"[%s]\" $x; echo"),
        "[a b][aa][ab]\n"
    );
    assert_eq!(
        run_in_sandbox("printf \"[%s]\" $(echo 'a*'); echo"),
        "[a b][aa][ab]\n"
    );
    assert_eq!(
        run_in_sandbox("set -- 'a*'; printf \"[%s]\" $@; echo"),
        "[a b][aa][ab]\n"
    );
    assert_eq!(
        run_in_sandbox("arr=('a*'); printf \"[%s]\" ${arr[@]}; echo"),
        "[a b][aa][ab]\n"
    );
}

/// A quoted prefix concatenated with an unquoted metacharacter: the protection
/// is per byte, not per word, so only the `*` may act as a pattern.
#[test]
fn protection_is_per_byte_not_per_word() {
    assert_eq!(fields("'a'*"), "[a b][aa][ab]\n");
    assert_eq!(fields("\"a\"*"), "[a b][aa][ab]\n");
}

// ---------------------------------------------------------------------------
// Quote characters arriving as data must not fake quoting
// ---------------------------------------------------------------------------

#[test]
fn an_apostrophe_in_data_does_not_disable_globbing() {
    // `it's` looked like an unterminated single quote to the old scanner, which
    // then treated the following `*` as quoted and refused to glob.
    assert_eq!(
        run_in_sandbox("x=\"it's\"; printf \"[%s]\" $x*; echo"),
        "[it's-f]\n"
    );
}

#[test]
fn a_double_quote_in_data_does_not_disable_globbing() {
    assert_eq!(
        run_in_sandbox("x='q\"'; printf \"[%s]\" $x*; echo"),
        "[q\"f]\n"
    );
}

// ---------------------------------------------------------------------------
// Interactions
// ---------------------------------------------------------------------------

/// Brace expansion runs before pathname expansion and each alternative keeps its
/// own quoting. This is why alternatives are spliced back as parse-tree parts
/// rather than as expanded text — flattening made `'*'` and `*` indistinguishable.
#[test]
fn brace_alternatives_keep_their_own_quoting() {
    assert_eq!(fields("{a,z}*"), "[a b][aa][ab][z*]\n");
    assert_eq!(fields("{'*',zz}"), "[*][zz]\n");
    assert_eq!(fields("{1..3}"), "[1][2][3]\n");
}

/// No match leaves the word as written *with quotes removed* — the unescaped
/// text, not the internal pattern the matcher was handed.
#[test]
fn an_unmatched_pattern_falls_back_to_unescaped_text() {
    assert_eq!(fields("zz*"), "[zz*]\n");
    assert_eq!(fields("'zz'*"), "[zz*]\n");
    assert_eq!(fields("zz\\*"), "[zz*]\n");
}

#[test]
fn noglob_disables_globbing_entirely() {
    assert_eq!(run_in_sandbox("set -f; printf \"[%s]\" *; echo"), "[*]\n");
}

/// Hidden files stay hidden unless the pattern says otherwise, and that decision
/// must not be confused by protected metacharacters elsewhere in the word.
#[test]
fn dotfiles_are_excluded_unless_named() {
    assert_eq!(fields("*"), "[a b][aa][ab][it's-f][q\"f][star*lit][sub]\n");
    assert_eq!(fields("'.hidden'"), "[.hidden]\n");
}

// ---------------------------------------------------------------------------
// Twin test: the terminals' replay contract
// ---------------------------------------------------------------------------

/// The terminals restore a session by replaying a saved argv as shell text,
/// quoted by `jterm_core::process::shell_quote_argv_for`. That function only
/// quotes for shells whose grammar it has been checked against, and it now lists
/// `jsh` — which is only sound while the string below parses back to exactly the
/// arguments it was built from.
///
/// This is the twin of `jsh_quoting_round_trips` in
/// `jterm_core/src/process.rs`: the literal is that test's expected output,
/// copied verbatim. jsh has no dependency on jterm_core (the two reach jagent
/// through different Cargo sources and would compile incompatible copies), so
/// this pair of tests *is* the seam — change the quoting on either side and both
/// go red.
///
/// Every argument here is hostile in a different way: a space (field splitting),
/// an apostrophe (the close/quote/reopen idiom), `*` `?` `[ab]` (pathname
/// expansion), `$(id)` (command substitution), `~` (tilde expansion) and `;`
/// (command separation).
#[test]
fn a_quoted_argv_from_jterm_core_parses_back_to_the_same_arguments() {
    let quoted = r#"'printf' '[%s]' 'a b' 'it'"'"'s' '*' '$(id)' '~' 'a;b' 'a?' 'x[ab]'"#;
    assert_eq!(
        run_in_sandbox(&format!("{quoted}; echo")),
        "[a b][it's][*][$(id)][~][a;b][a?][x[ab]]\n"
    );
}
