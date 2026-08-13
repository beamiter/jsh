//! Unified metadata and discovery for commands implemented by jsh.
//!
//! Execution still lives in the classic and value-builtin routing tables.  The
//! catalog deliberately sits above both tables so help, completion,
//! highlighting, and suggestions all see the same stable set of names.

use crate::signature::{Signature, SIGNATURES};
use once_cell::sync::Lazy;
use std::collections::BTreeMap;

/// How a command participates in the in-process value pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueRoute {
    /// The command has no value-aware handler.
    None,
    /// The value-aware handler is available whenever the command is invoked.
    Always,
    /// The handler is used only in a multi-command value pipeline.
    ///
    /// `ls` and `ps` use this route so their bare forms continue to resolve to
    /// the user's external commands.
    ContextOnly,
}

impl ValueRoute {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Always => "always",
            Self::ContextOnly => "context-only",
        }
    }
}

/// One public command known to jsh.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandInfo {
    pub name: &'static str,
    pub canonical_name: &'static str,
    /// Present in the classic/fork-path routing table.
    pub classic_route: bool,
    pub value_route: ValueRoute,
}

impl CommandInfo {
    /// Whether normal command dispatch may invoke this name as a shell builtin.
    ///
    /// Pipeline-only value routes remain discoverable but intentionally return
    /// false here, preserving external dispatch for a bare `ls` or `ps`.
    pub fn is_builtin(self) -> bool {
        self.classic_route || self.value_route == ValueRoute::Always
    }

    /// Declarative signature, following a compatibility alias to its canonical
    /// command when necessary.
    pub fn signature(self) -> Option<&'static Signature> {
        SIGNATURES
            .get(self.name)
            .copied()
            .or_else(|| SIGNATURES.get(self.canonical_name).copied())
    }

    /// Concise, user-facing description suitable for help and completion.
    pub fn summary(self) -> &'static str {
        self.signature()
            .map(|signature| signature.desc)
            .unwrap_or_else(|| fallback_summary(self.canonical_name))
    }

    /// Usage syntax retained for classic commands that do not yet have a
    /// declarative signature. Signed commands render their typed signature
    /// instead.
    pub fn usage(self) -> Option<&'static str> {
        fallback_usage(self.canonical_name)
    }

    /// Option and behavior notes retained from the classic help table.
    pub fn detail(self) -> Option<&'static str> {
        fallback_detail(self.canonical_name)
    }

    /// One stable machine-readable help schema shared by both execution paths.
    pub fn help_record(self) -> crate::value::Value {
        use crate::value::Value;
        use indexmap::IndexMap;

        let mut record = IndexMap::new();
        record.insert("name".to_string(), Value::String(self.name.to_string()));
        record.insert(
            "canonical_name".to_string(),
            Value::String(self.canonical_name.to_string()),
        );
        record.insert(
            "desc".to_string(),
            Value::String(self.summary().to_string()),
        );
        record.insert("classic_route".to_string(), Value::Bool(self.classic_route));
        record.insert(
            "value_route".to_string(),
            Value::String(self.value_route.as_str().to_string()),
        );
        if let Some(usage) = self.usage() {
            record.insert("usage".to_string(), Value::String(usage.to_string()));
        }
        if let Some(detail) = self.detail() {
            record.insert("detail".to_string(), Value::String(detail.to_string()));
        }
        if let Some(signature) = self.signature() {
            record.insert("signature".to_string(), signature.to_record());
        }
        Value::Record(record)
    }
}

static COMMANDS: Lazy<Vec<CommandInfo>> = Lazy::new(|| {
    let mut commands: BTreeMap<&'static str, CommandInfo> = BTreeMap::new();

    // BUILTIN_NAMES is the low-level classic/fork routing surface.  Discovery
    // must consume this catalog instead of reading that table directly.
    for &name in crate::builtins::BUILTIN_NAMES {
        commands
            .entry(name)
            .or_insert_with(|| command_info(name))
            .classic_route = true;
    }

    for &name in crate::value_builtins::VALUE_BUILTINS.keys() {
        commands
            .entry(name)
            .or_insert_with(|| command_info(name))
            .value_route = if matches!(name, "ls" | "ps") {
            ValueRoute::ContextOnly
        } else {
            ValueRoute::Always
        };
    }

    commands.into_values().collect()
});

static ALL_NAMES: Lazy<Vec<&'static str>> =
    Lazy::new(|| COMMANDS.iter().map(|command| command.name).collect());

static BUILTIN_NAMES: Lazy<Vec<&'static str>> = Lazy::new(|| {
    COMMANDS
        .iter()
        .copied()
        .filter(|command| command.is_builtin())
        .map(|command| command.name)
        .collect()
});

fn command_info(name: &'static str) -> CommandInfo {
    CommandInfo {
        name,
        canonical_name: canonical_name(name),
        classic_route: false,
        value_route: ValueRoute::None,
    }
}

fn canonical_name(name: &'static str) -> &'static str {
    match name {
        "." => "source",
        "[" => "test",
        "wf" => "workflow",
        _ => name,
    }
}

/// All catalog entries, sorted lexicographically by command name.
pub fn entries() -> &'static [CommandInfo] {
    COMMANDS.as_slice()
}

/// Every discoverable command name, sorted and unique.
///
/// This includes pipeline-only value routes such as `ls` and `ps`.
pub fn all_names() -> &'static [&'static str] {
    ALL_NAMES.as_slice()
}

/// Names that normal command dispatch recognises as builtins, sorted and
/// unique.  Pipeline-only `ls`/`ps` are intentionally absent.
pub fn builtin_names() -> &'static [&'static str] {
    BUILTIN_NAMES.as_slice()
}

/// Look up a discoverable command by its public name.
pub fn get(name: &str) -> Option<&'static CommandInfo> {
    COMMANDS
        .binary_search_by(|command| command.name.cmp(name))
        .ok()
        .map(|index| &COMMANDS[index])
}

/// True for every name in the discovery catalog, including context-only
/// value routes.
pub fn contains(name: &str) -> bool {
    get(name).is_some()
}

/// True when normal dispatch may invoke `name` as a shell builtin.
pub fn is_builtin(name: &str) -> bool {
    get(name).is_some_and(|command| command.is_builtin())
}

/// Return a command's canonical name (for example, `.` resolves to `source`).
pub fn canonical(name: &str) -> Option<&'static str> {
    get(name).map(|command| command.canonical_name)
}

/// Return the static signature for a command or canonical compatibility alias.
pub fn signature(name: &str) -> Option<&'static Signature> {
    get(name).and_then(|command| command.signature())
}

/// Return a concise description for every discoverable command.
pub fn summary(name: &str) -> Option<&'static str> {
    get(name).map(|command| command.summary())
}

fn fallback_summary(name: &str) -> &'static str {
    match name {
        ":" => "Do nothing successfully.",
        "[[" => "Evaluate an extended conditional expression.",
        "agent" => "Run the optional AI assistant.",
        "alias" => "Define or display command aliases.",
        "avg" => "Calculate the average of numeric input.",
        "bg" => "Resume a stopped job in the background.",
        "bookmark" => "Manage named directory bookmarks.",
        "break" => "Exit one or more enclosing loops.",
        "builtin" => "Run a shell builtin by name.",
        "cd" => "Change the current working directory.",
        "command" => "Run or describe a command without alias expansion.",
        "compgen" => "Generate completion candidates.",
        "complete" => "Register or inspect command completion rules.",
        "context" => "Inspect captured command execution context.",
        "continue" => "Resume an enclosing loop at its next iteration.",
        "debug-completion" => "Inspect completion results for an input buffer.",
        "debug-profile" => "Profile repeated command execution.",
        "debug-timing" => "Measure command execution time.",
        "debug-trace" => "Trace command execution.",
        "declare" => "Declare variables and their attributes.",
        "dedupe" => "Remove duplicate input lines.",
        "dirs" => "Display the directory stack.",
        "disown" => "Remove jobs from the shell's job table.",
        "echo" => "Print arguments separated by spaces.",
        "eval" => "Parse and execute arguments as shell code.",
        "exec" => "Replace the shell with another command.",
        "exit" => "Exit the shell with an optional status.",
        "export" => "Set variables and mark them for child processes.",
        "false" => "Return an unsuccessful status.",
        "fg" => "Bring a job to the foreground.",
        "filter" => "Filter input lines by a pattern.",
        "hash" => "Refresh the command lookup cache.",
        "history" => "Display command history.",
        "hook" => "Manage shell lifecycle hooks.",
        "jobs" => "List active jobs.",
        "local" => "Declare variables local to a function.",
        "lower" => "Convert text to lowercase.",
        "map" => "Transform input lines with a replacement expression.",
        "max" => "Find the maximum numeric input.",
        "min" => "Find the minimum numeric input.",
        "popd" => "Pop a directory from the stack and change to it.",
        "printf" => "Print formatted output.",
        "pushd" => "Push a directory onto the stack and change to it.",
        "pwd" => "Print the current working directory.",
        "read" => "Read a line into shell variables.",
        "return" => "Return from a function with an optional status.",
        "set" => "Set shell options or positional parameters.",
        "shift" => "Shift positional parameters to the left.",
        "shopt" => "Set or display optional shell behavior.",
        "source" => "Execute a file in the current shell.",
        "stats" => "Calculate summary statistics for numeric input.",
        "sum" => "Sum numeric input.",
        "test" => "Evaluate a conditional expression.",
        "trap" => "Set signal and shell-exit handlers.",
        "trim" => "Trim surrounding whitespace from text.",
        "true" => "Return a successful status.",
        "type" => "Describe how command names resolve.",
        "unalias" => "Remove command aliases.",
        "uniq" => "Remove adjacent duplicate input lines.",
        "unset" => "Remove variables or functions.",
        "upper" => "Convert text to uppercase.",
        "wait" => "Wait for jobs or processes to finish.",
        "workflow" => "List, inspect, or render local workflow templates.",
        "z" => "Jump to a frecency-ranked directory.",
        _ => "Run a jsh built-in command.",
    }
}

fn fallback_usage(name: &str) -> Option<&'static str> {
    Some(match name {
        "cd" => "cd [-] [dir]",
        "exit" => "exit [N]",
        "export" => "export [-n] name[=value]...",
        "unset" => "unset name...",
        "echo" => "echo [-neE] [args...]",
        "printf" => "printf format [args...]",
        "pwd" => "pwd",
        "alias" => "alias [name[=value]...]",
        "unalias" => "unalias [-a] name...",
        "type" => "type name...",
        "source" => "source file [args...]",
        "eval" => "eval [args...]",
        "read" => "read [-p prompt] [-t timeout] [-r] var...",
        "test" => "test expr",
        "set" => "set [-/+euxo option]",
        "local" => "local name[=value]...",
        "shift" => "shift [N]",
        "jobs" => "jobs",
        "fg" => "fg [%N]",
        "bg" => "bg [%N]",
        "wait" => "wait [pid|%jobspec...]",
        "trap" => "trap [action] signal...",
        "return" => "return [N]",
        "break" => "break [N]",
        "continue" => "continue [N]",
        "declare" => "declare [-aAirx] name[=value]...",
        "history" => "history",
        "context" => "context <list|show|last-failed> [options]",
        "pushd" => "pushd [dir]",
        "popd" => "popd",
        "dirs" => "dirs",
        "complete" => "complete [-W words] [-F func] cmd",
        "compgen" => "compgen [-abcdfv] [-A action] [-W words] [-G glob] [prefix]",
        "disown" => "disown [-a] [%N]",
        "shopt" => "shopt [-su] opt...",
        "exec" => "exec cmd [args...]",
        "hash" => "hash [-r]",
        "z" => "z [query]",
        "bookmark" => "bookmark <add|go|ls|rm> [name]",
        "hook" => "hook <add|remove|list> <precmd|preexec|chpwd> [cmd]",
        "workflow" => "workflow <list|show|render> [name] [parameter=value ...]",
        // Declarative signatures are the detailed usage source for these and
        // future value commands; everything else still has a useful summary.
        _ => return None,
    })
}

fn fallback_detail(name: &str) -> Option<&'static str> {
    Some(match name {
        "cd" => "Use `cd -` to return to the previous directory.",
        "exit" => "Without N, exits with the most recent command status.",
        "export" => "Use -n to remove the export attribute without deleting the value.",
        "echo" => "Use -n to omit the newline and -e to interpret escapes.",
        "unalias" => "Use -a to remove all aliases.",
        "set" => "Use -e to enable errexit; a leading + disables an option.",
        "shift" => "N defaults to 1.",
        "trap" => "An empty action (`trap '' SIGNAL`) ignores the signal.",
        "disown" => "Disowned jobs are removed from the table and do not receive shell SIGHUP.",
        "hash" => "-r refreshes the command lookup cache.",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn catalog_is_the_sorted_unique_union_of_both_execution_tables() {
        let classic: BTreeSet<&str> = crate::builtins::BUILTIN_NAMES.iter().copied().collect();
        let value: BTreeSet<&str> = crate::value_builtins::VALUE_BUILTINS
            .keys()
            .copied()
            .collect();
        assert_eq!(classic.len(), crate::builtins::BUILTIN_NAMES.len());
        assert_eq!(value.len(), crate::value_builtins::VALUE_BUILTINS.len());

        let expected: BTreeSet<&str> = classic.union(&value).copied().collect();
        let actual: Vec<&str> = all_names().to_vec();

        assert_eq!(actual, expected.iter().copied().collect::<Vec<_>>());
        assert!(actual.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(entries().len(), actual.len());
        assert!(builtin_names().windows(2).all(|pair| pair[0] < pair[1]));

        for name in classic {
            assert!(get(name).is_some_and(|command| command.classic_route));
        }
        for name in value {
            assert_ne!(get(name).unwrap().value_route, ValueRoute::None);
        }
    }

    #[test]
    fn every_public_command_has_help_metadata() {
        for &name in all_names() {
            let summary = summary(name).unwrap();
            assert!(!summary.trim().is_empty(), "{name}");
            assert_ne!(summary, "Run a jsh built-in command.", "{name}");
        }
        for &name in SIGNATURES.keys() {
            assert!(
                contains(name),
                "signature without executable command: {name}"
            );
        }
    }

    #[test]
    fn aliases_and_pipeline_only_routes_are_explicit() {
        assert_eq!(canonical("."), Some("source"));
        assert_eq!(summary("."), summary("source"));
        assert_eq!(canonical("["), Some("test"));

        for name in ["ls", "ps"] {
            let command = get(name).unwrap();
            assert_eq!(command.value_route, ValueRoute::ContextOnly);
            assert!(!command.is_builtin());
            assert!(contains(name));
        }
    }
}
