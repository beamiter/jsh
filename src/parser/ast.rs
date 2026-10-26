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
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SimpleCommand {
    pub assignments: Vec<Assignment>,
    pub words: Vec<Word>,
    pub redirects: Vec<Redirect>,
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
    },
    Select {
        var: String,
        words: Option<Vec<Word>>,
        body: Vec<CompleteCommand>,
        redirects: Vec<Redirect>,
    },
    Arithmetic {
        expr: String,
        redirects: Vec<Redirect>,
    },
    Coproc {
        name: Option<String>,
        command: Box<SimpleCommand>,
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
