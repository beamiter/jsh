/// AST node definitions for bash-compatible shell grammar.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum WordPart {
    Literal(String),
    SingleQuoted(String),
    DoubleQuoted(Vec<WordPart>),
    Variable(String),
    /// `$name.field[0].other` — variable with dotted/indexed path access into a typed Value.
    VariablePath {
        name: String,
        path: Vec<PathSeg>,
    },
    /// `$"...($expr)..."` — nushell-style interpolated string. Parts concatenate at expand time.
    Interpolated(Vec<InterpPart>),
    /// `{|p1 p2| body}` — closure literal. Body is kept as raw source and re-parsed at apply time.
    Closure {
        params: Vec<String>,
        body_src: String,
    },
    CommandSub(String),
    Glob(String),
    Tilde(String),
    BraceExpansion(Vec<Vec<WordPart>>),
    BraceRange {
        start: String,
        end: String,
        step: Option<String>,
    },
    Arithmetic(String),
    ProcessSub(String, ProcessSubKind),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PathSeg {
    Field(String),
    Index(i64),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum InterpPart {
    Lit(String),
    Expr(Word),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ProcessSubKind {
    Input,  // <(cmd) -- provides input
    Output, // >(cmd) -- accepts output
}

pub type Word = Vec<WordPart>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Assignment {
    pub name: String,
    pub value: Word,
    pub index: Option<String>,          // For arr[idx]=value
    pub append: bool,                   // For var+=value or arr+=(...)
    pub array_value: Option<Vec<Word>>, // For arr=(a b c)
}

// Here-doc options to store multi-line content and modifiers
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HereDocOptions {
    pub delimiter: String, // The delimiter (e.g., "EOF")
    pub content: String,   // The multi-line content
    pub strip_tabs: bool,  // <<- strips leading tabs
    pub expand_vars: bool, // Whether to expand variables ($var, $(cmd), etc)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum RedirectTarget {
    File(Word),              // Regular file redirection target
    HereDoc(HereDocOptions), // Here-doc with content and options
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum RedirectKind {
    Output,
    Clobber, // >| overrides noclobber
    Append,
    Input,
    HereDoc, // << (still used for identification)
    HereString,
    DupOutput,
    DupInput,
    OutputAll, // &> (redirect stdout and stderr)
    AppendAll, // &>> (append stdout and stderr)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Redirect {
    pub fd: Option<i32>,
    pub kind: RedirectKind,
    pub target: Word,                     // For backward compatibility, kept as Word
    pub here_doc: Option<HereDocOptions>, // Here-doc specific data
    /// `{name}>file`: the shell picks a free descriptor (10 or above) and
    /// stores its number in `name` instead of using `fd`. For `{name}>&-` the
    /// descriptor to close is read from `name`. `name` may be an array
    /// element (`{COPROC[1]}>&-`).
    #[serde(default)]
    pub fd_var: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SimpleCommand {
    pub assignments: Vec<Assignment>,
    pub words: Vec<Word>,
    pub redirects: Vec<Redirect>,
    /// Line the command is reported on (`$LINENO`, `BASH_LINENO`, `caller`),
    /// counted from 1 in the text this AST was parsed from. Zero means
    /// unknown (an AST from an older session snapshot).
    #[serde(default)]
    pub line: usize,
    /// The command as `BASH_COMMAND` and the DEBUG trap report it: its tokens
    /// as written, joined by single spaces.
    #[serde(default)]
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CaseTerminator {
    Break,         // ;;   stop after this arm
    FallThrough,   // ;&   run the next arm's body unconditionally
    ContinueMatch, // ;;&  keep testing subsequent arms
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaseArm {
    pub patterns: Vec<Word>,
    pub body: Vec<CompleteCommand>,
    pub terminator: CaseTerminator,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CompoundCommand {
    BraceGroup {
        body: Vec<CompleteCommand>,
        redirects: Vec<Redirect>,
    },
    Subshell {
        body: Vec<CompleteCommand>,
        redirects: Vec<Redirect>,
    },
    If {
        conditions: Vec<(Vec<CompleteCommand>, Vec<CompleteCommand>)>,
        else_branch: Option<Vec<CompleteCommand>>,
        redirects: Vec<Redirect>,
    },
    For {
        var: String,
        words: Option<Vec<Word>>,
        body: Vec<CompleteCommand>,
        redirects: Vec<Redirect>,
        /// Line and `BASH_COMMAND` text of the `for ... in ...` header; see
        /// [`SimpleCommand::line`].
        #[serde(default)]
        line: usize,
        #[serde(default)]
        text: String,
    },
    CStyleFor {
        init: String,
        condition: String,
        update: String,
        body: Vec<CompleteCommand>,
        redirects: Vec<Redirect>,
    },
    While {
        condition: Vec<CompleteCommand>,
        body: Vec<CompleteCommand>,
        redirects: Vec<Redirect>,
    },
    Until {
        condition: Vec<CompleteCommand>,
        body: Vec<CompleteCommand>,
        redirects: Vec<Redirect>,
    },
    Case {
        word: Word,
        arms: Vec<CaseArm>,
        redirects: Vec<Redirect>,
        /// Line and `BASH_COMMAND` text of the `case ... in` header.
        #[serde(default)]
        line: usize,
        #[serde(default)]
        text: String,
    },
    Select {
        var: String,
        words: Option<Vec<Word>>,
        body: Vec<CompleteCommand>,
        redirects: Vec<Redirect>,
        /// Line and `BASH_COMMAND` text of the `select ... in ...` header.
        #[serde(default)]
        line: usize,
        #[serde(default)]
        text: String,
    },
    Arithmetic {
        expr: String,
        redirects: Vec<Redirect>,
    },
    /// `coproc cmd args` (a simple command, named `COPROC`) or
    /// `coproc [NAME] compound-command`.
    Coproc {
        name: Option<String>,
        command: Box<Command>,
        redirects: Vec<Redirect>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Command {
    Simple(SimpleCommand),
    Compound(CompoundCommand),
    FunctionDef {
        name: String,
        body: Box<CompoundCommand>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pipeline {
    pub negated: bool,
    pub commands: Vec<Command>,
    /// `time [-p] pipeline`: report elapsed, user and system time on stderr.
    #[serde(default)]
    pub timed: Option<TimeFormat>,
}

/// Which report the `time` reserved word prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TimeFormat {
    /// `$TIMEFORMAT`, or bash's default when it is unset.
    Shell,
    /// `time -p`: the fixed POSIX format.
    Posix,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Connector {
    And,
    Or,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AndOrList {
    pub first: Pipeline,
    pub rest: Vec<(Connector, Pipeline)>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompleteCommand {
    pub list: AndOrList,
    pub background: bool,
    pub disown: bool,
}
