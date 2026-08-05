//! What this shell knows without asking anything: the flags, operators,
//! option values and file kinds that are fixed properties of a command.
//!
//! Everything here is a table because the alternative is running the command
//! to ask it, which a keystroke must not do. They are deliberately not
//! exhaustive — the aim is the options worth choosing from a list, where
//! remembering which letter means what is the actual friction, and the
//! history fallback already recalls whatever else someone has typed.

/// Extensions a command is normally pointed at. Directories always stay —
/// they are the way to reach the file — and a command not listed here keeps
/// every candidate.
pub(super) fn command_file_extensions(cmd: &str) -> Option<&'static [&'static str]> {
    Some(match cmd {
        "source" | "." | "bash" | "sh" | "zsh" | "jsh" => &["sh", "bash", "zsh", "jsh", "rc"],
        "python" | "python3" => &["py", "pyc", "pyz"],
        "node" | "nodejs" => &["js", "mjs", "cjs", "ts"],
        "ruby" => &["rb"],
        "perl" => &["pl", "pm"],
        "unzip" | "zipinfo" => &["zip", "jar", "war", "aar", "whl", "egg", "apk"],
        "gunzip" | "zcat" => &["gz", "tgz", "z"],
        "bunzip2" | "bzcat" => &["bz2", "tbz", "tbz2"],
        "unxz" | "xzcat" => &["xz", "txz"],
        "gzip" | "bzip2" | "xz" | "zstd" => return None,
        "rustc" => &["rs"],
        "javac" => &["java"],
        "java" => &["jar", "class"],
        "go" => &["go"],
        "docker-compose" => &["yml", "yaml"],
        "psql" | "sqlite3" => &["sql", "db", "sqlite", "sqlite3"],
        _ => return None,
    })
}

/// Value-aware builtins whose first argument names a field of the records
/// flowing through the pipeline.
pub(super) const FIELD_TAKING_BUILTINS: &[&str] = &[
    "where", "select", "sort-by", "group-by", "reject", "get", "uniq-by", "count-by", "flatten",
    "rename",
];

/// The operators of `test`, `[` and `[[`, which are a closed set and the
/// place single letters are hardest to recall.
pub(super) const TEST_OPERATORS: &[(&str, &str)] = &[
    ("-e", "the path exists"),
    ("-f", "a regular file"),
    ("-d", "a directory"),
    ("-L", "a symbolic link"),
    ("-h", "a symbolic link"),
    ("-s", "exists and is not empty"),
    ("-r", "readable"),
    ("-w", "writable"),
    ("-x", "executable"),
    ("-p", "a named pipe"),
    ("-S", "a socket"),
    ("-b", "a block device"),
    ("-c", "a character device"),
    ("-t", "a terminal on this descriptor"),
    ("-z", "the string is empty"),
    ("-n", "the string is not empty"),
    ("-eq", "numbers are equal"),
    ("-ne", "numbers differ"),
    ("-lt", "less than"),
    ("-le", "less than or equal"),
    ("-gt", "greater than"),
    ("-ge", "greater than or equal"),
    ("-nt", "newer than"),
    ("-ot", "older than"),
    ("-ef", "the same file"),
    ("-v", "the variable is set"),
    ("-a", "and"),
    ("-o", "or"),
];

/// Values an option accepts, when they are a fixed set worth choosing from.
/// Keyed by command and option name; `None` when the option takes free text,
/// a path, or something only the command itself could enumerate.
pub(super) fn option_value_choices(
    cmd: &str,
    option: &str,
) -> Option<&'static [(&'static str, &'static str)]> {
    let choices: &[(&str, &str)] = match (cmd, option) {
        ("curl", "-X" | "--request") => &[
            ("GET", "read a resource"),
            ("POST", "create or submit"),
            ("PUT", "replace"),
            ("PATCH", "modify"),
            ("DELETE", "remove"),
            ("HEAD", "headers only"),
            ("OPTIONS", "what the resource allows"),
        ],
        ("git", "--pretty" | "--format") => &[
            ("oneline", "one commit per line"),
            ("short", "author and message"),
            ("medium", "the default"),
            ("full", "author and committer"),
            ("fuller", "with dates"),
            ("reference", "hash, subject and date"),
            ("raw", "the commit object"),
        ],
        ("git", "--color" | "--decorate")
        | ("ls" | "grep" | "diff" | "dir", "--color")
        | ("systemctl" | "journalctl", "--legend") => &[
            ("auto", "when the output is a terminal"),
            ("always", "even when piped"),
            ("never", "never"),
        ],
        ("git", "--strategy" | "-s") => &[
            ("ort", "the default merge strategy"),
            ("recursive", "the previous default"),
            ("resolve", "three-way merge"),
            ("ours", "keep this side"),
            ("subtree", "merge a subtree"),
        ],
        ("git", "--cleanup") => &[
            ("strip", "drop comments and blank lines"),
            ("whitespace", "strip trailing whitespace"),
            ("verbatim", "keep the message as written"),
            ("scissors", "cut at the scissor line"),
        ],
        ("systemctl", "--state") => &[
            ("active", "running or otherwise active"),
            ("inactive", "not active"),
            ("failed", "failed units"),
            ("enabled", "enabled unit files"),
            ("disabled", "disabled unit files"),
            ("static", "no enable symlinks"),
            ("masked", "cannot be started"),
        ],
        ("systemctl", "--type" | "-t") => &[
            ("service", "services"),
            ("socket", "sockets"),
            ("timer", "timers"),
            ("target", "targets"),
            ("mount", "mounts"),
            ("path", "path units"),
            ("device", "devices"),
        ],
        ("journalctl", "-p" | "--priority") => &[
            ("emerg", "system is unusable"),
            ("alert", "act immediately"),
            ("crit", "critical"),
            ("err", "errors"),
            ("warning", "warnings"),
            ("notice", "normal but significant"),
            ("info", "informational"),
            ("debug", "debug"),
        ],
        ("journalctl", "-o" | "--output") => &[
            ("short", "the default"),
            ("short-iso", "ISO timestamps"),
            ("json", "one JSON object per entry"),
            ("json-pretty", "indented JSON"),
            ("cat", "message text only"),
            ("verbose", "every field"),
        ],
        ("docker", "--restart") => &[
            ("no", "never restart"),
            ("on-failure", "restart on a non-zero exit"),
            ("always", "always restart"),
            ("unless-stopped", "unless it was stopped by hand"),
        ],
        ("docker" | "docker-compose", "--format") => {
            &[("json", "JSON output"), ("table", "a table")]
        }
        ("kubectl", "-o" | "--output") => &[
            ("wide", "extra columns"),
            ("json", "JSON"),
            ("yaml", "YAML"),
            ("name", "resource names only"),
            ("jsonpath", "a JSONPath expression"),
            ("custom-columns", "columns you name"),
        ],
        ("find", "-type") => &[
            ("f", "regular file"),
            ("d", "directory"),
            ("l", "symbolic link"),
            ("s", "socket"),
            ("p", "named pipe"),
            ("b", "block device"),
            ("c", "character device"),
        ],
        ("ps", "-o" | "--format") => &[
            ("pid", "process id"),
            ("ppid", "parent process id"),
            ("user", "owner"),
            ("comm", "command name"),
            ("args", "full command line"),
            ("pcpu", "CPU percentage"),
            ("pmem", "memory percentage"),
            ("rss", "resident memory"),
            ("etime", "elapsed time"),
            ("stat", "process state"),
        ],
        ("tar", "--format") => &[
            ("gnu", "GNU tar format"),
            ("pax", "POSIX pax format"),
            ("ustar", "POSIX ustar format"),
        ],
        ("cargo", "--message-format") => &[
            ("human", "for reading"),
            ("json", "for tools"),
            ("short", "one line per diagnostic"),
        ],
        ("npm" | "pnpm" | "yarn", "--save") => &[
            ("prod", "into dependencies"),
            ("dev", "into devDependencies"),
            ("optional", "into optionalDependencies"),
        ],
        ("ssh" | "scp" | "sftp", "-o") => &[
            ("StrictHostKeyChecking=", "trust policy for host keys"),
            ("UserKnownHostsFile=", "where host keys are stored"),
            ("ProxyJump=", "connect through another host"),
            ("ConnectTimeout=", "seconds before giving up"),
            ("ServerAliveInterval=", "keepalive interval"),
            ("ForwardAgent=", "forward the agent"),
        ],
        _ => return None,
    };
    Some(choices)
}

/// Flags for the tools people reach for constantly, where remembering which
/// letter means what is the actual friction. Kept to the options worth
/// choosing from a list: exhaustive coverage belongs in a spec file, and the
/// history fallback already recalls whatever else someone has typed before.
#[allow(clippy::type_complexity)]
pub(super) const COMMON_FLAGS: &[(&str, &[(&str, &str)])] = &[
    (
        "ps",
        &[
            ("aux", "every process, user-oriented"),
            ("-e", "every process"),
            ("-f", "full format"),
            ("-o", "choose output columns"),
            ("--sort", "sort by a column"),
            ("-p", "by process id"),
            ("-u", "by user"),
        ],
    ),
    (
        "df",
        &[
            ("-h", "human readable sizes"),
            ("-i", "inodes instead of blocks"),
            ("-T", "show filesystem type"),
            ("-x", "exclude a filesystem type"),
        ],
    ),
    (
        "du",
        &[
            ("-h", "human readable sizes"),
            ("-s", "summary per argument"),
            ("-d", "limit depth"),
            ("-a", "include files"),
            ("-x", "stay on one filesystem"),
            ("--exclude", "skip matching paths"),
        ],
    ),
    (
        "curl",
        &[
            ("-s", "silent"),
            ("-S", "show errors even when silent"),
            ("-L", "follow redirects"),
            ("-o", "write to a file"),
            ("-O", "write to the remote name"),
            ("-X", "request method"),
            ("-H", "add a header"),
            ("-d", "request body"),
            ("-F", "multipart form field"),
            ("-u", "credentials"),
            ("-i", "include response headers"),
            ("-I", "headers only"),
            ("-f", "fail on HTTP errors"),
            ("--json", "JSON body, headers set"),
            ("--retry", "retry a failed transfer"),
        ],
    ),
    (
        "wget",
        &[
            ("-O", "write to a file"),
            ("-c", "continue a partial download"),
            ("-q", "quiet"),
            ("-r", "recursive"),
            ("--no-check-certificate", "skip TLS verification"),
        ],
    ),
    (
        "sed",
        &[
            ("-i", "edit files in place"),
            ("-E", "extended regex"),
            ("-n", "print only what is asked"),
            ("-e", "add a script"),
            ("-f", "read a script file"),
        ],
    ),
    (
        "awk",
        &[
            ("-F", "field separator"),
            ("-v", "assign a variable"),
            ("-f", "read a program file"),
        ],
    ),
    (
        "xargs",
        &[
            ("-n", "arguments per command"),
            ("-P", "run in parallel"),
            ("-I", "replace a placeholder"),
            ("-0", "null separated input"),
            ("-r", "skip when input is empty"),
            ("-t", "print each command"),
        ],
    ),
    (
        "sort",
        &[
            ("-n", "numeric"),
            ("-r", "reverse"),
            ("-k", "sort by field"),
            ("-t", "field separator"),
            ("-u", "drop duplicates"),
            ("-h", "human readable numbers"),
        ],
    ),
    (
        "uniq",
        &[
            ("-c", "count occurrences"),
            ("-d", "duplicates only"),
            ("-u", "unique lines only"),
            ("-i", "case insensitive"),
        ],
    ),
    ("head", &[("-n", "line count"), ("-c", "byte count")]),
    (
        "tail",
        &[
            ("-n", "line count"),
            ("-f", "follow as it grows"),
            ("-F", "follow across renames"),
            ("-c", "byte count"),
        ],
    ),
    (
        "wc",
        &[
            ("-l", "lines"),
            ("-w", "words"),
            ("-c", "bytes"),
            ("-m", "characters"),
        ],
    ),
    (
        "rsync",
        &[
            ("-a", "archive mode"),
            ("-v", "verbose"),
            ("-z", "compress in transit"),
            ("-P", "progress and resume"),
            ("-n", "dry run"),
            ("--delete", "remove what the source dropped"),
            ("--exclude", "skip matching paths"),
            ("-e", "remote shell to use"),
        ],
    ),
    (
        "journalctl",
        &[
            ("-u", "one unit"),
            ("-f", "follow"),
            ("-n", "last N lines"),
            ("-b", "this boot"),
            ("-p", "minimum priority"),
            ("--since", "from a time"),
            ("--until", "to a time"),
            ("--user", "user session log"),
            ("-k", "kernel messages"),
        ],
    ),
    (
        "systemctl",
        &[
            ("--user", "user manager"),
            ("--now", "start or stop as well"),
            ("--no-pager", "plain output"),
            ("-q", "quiet"),
            ("--failed", "only failed units"),
        ],
    ),
    (
        "ssh",
        &[
            ("-p", "port"),
            ("-i", "identity file"),
            ("-l", "login name"),
            ("-J", "jump host"),
            ("-A", "forward the agent"),
            ("-N", "no remote command"),
            ("-L", "local port forward"),
            ("-R", "remote port forward"),
            ("-o", "config option"),
            ("-v", "verbose"),
        ],
    ),
    (
        "scp",
        &[
            ("-r", "recursive"),
            ("-P", "port"),
            ("-i", "identity file"),
            ("-C", "compress"),
            ("-p", "preserve times and modes"),
        ],
    ),
    (
        "jq",
        &[
            ("-r", "raw output"),
            ("-c", "compact output"),
            ("-n", "no input"),
            ("-s", "slurp into an array"),
            ("-e", "exit status from the result"),
            ("--arg", "pass a string variable"),
        ],
    ),
    (
        "ln",
        &[
            ("-s", "symbolic link"),
            ("-f", "replace the target"),
            ("-n", "treat a link as a file"),
            ("-r", "relative symbolic link"),
        ],
    ),
    (
        "mv",
        &[
            ("-i", "ask before replacing"),
            ("-n", "never replace"),
            ("-v", "verbose"),
            ("-f", "replace without asking"),
        ],
    ),
    (
        "ping",
        &[
            ("-c", "stop after N packets"),
            ("-i", "seconds between packets"),
            ("-W", "reply timeout"),
            ("-4", "IPv4"),
            ("-6", "IPv6"),
        ],
    ),
    (
        "diff",
        &[
            ("-u", "unified context"),
            ("-r", "recursive"),
            ("-q", "report only whether they differ"),
            ("-w", "ignore whitespace"),
            ("--color", "colourise"),
        ],
    ),
];

/// ssh-family options whose next word is a value, not the destination.
pub(super) const SSH_VALUE_OPTIONS: &[&str] = &[
    "-i", "-F", "-E", "-o", "-p", "-l", "-J", "-b", "-c", "-e", "-m", "-B", "-L", "-R", "-D", "-W",
    "-S", "-P",
];

pub(super) const KILL_SIGNALS: &[(&str, &str)] = &[
    ("-1", "SIGHUP — hangup"),
    ("-2", "SIGINT — interrupt"),
    ("-3", "SIGQUIT — quit with core"),
    ("-9", "SIGKILL — force kill"),
    ("-15", "SIGTERM — terminate (default)"),
    ("-CONT", "resume a stopped process"),
    ("-HUP", "hangup"),
    ("-INT", "interrupt"),
    ("-KILL", "force kill"),
    ("-QUIT", "quit with core"),
    ("-STOP", "pause a process"),
    ("-TERM", "terminate (default)"),
    ("-USR1", "user-defined signal 1"),
    ("-USR2", "user-defined signal 2"),
];

/// Shell-condition names for `trap`, plus the signals worth trapping.
pub(super) const TRAP_SIGNALS: &[(&str, &str)] = &[
    ("EXIT", "when the shell exits"),
    ("ERR", "when a command fails"),
    ("DEBUG", "before every command"),
    ("HUP", "hangup"),
    ("INT", "on Ctrl-C"),
    ("QUIT", "quit with core"),
    ("ABRT", "abort"),
    ("ALRM", "timer expired"),
    ("TERM", "terminate"),
    ("USR1", "user-defined signal 1"),
    ("USR2", "user-defined signal 2"),
    ("PIPE", "broken pipe"),
    ("CHLD", "child state changed"),
    ("CONT", "continued after stop"),
    ("WINCH", "terminal resized"),
];

/// The compose file names Docker itself looks for, in its own order.
pub(super) const COMPOSE_FILE_NAMES: &[&str] = &[
    "compose.yaml",
    "compose.yml",
    "docker-compose.yaml",
    "docker-compose.yml",
];

/// Keys worth offering even when nothing has set them yet.
pub(super) const GIT_CONFIG_KEYS: &[(&str, &str)] = &[
    ("user.name", "Name recorded on commits"),
    ("user.email", "Email recorded on commits"),
    ("core.editor", "Editor for messages"),
    ("core.excludesfile", "Global ignore file"),
    ("init.defaultBranch", "Branch name for new repositories"),
    ("pull.rebase", "Rebase instead of merge on pull"),
    ("push.default", "What a bare push pushes"),
    ("push.autoSetupRemote", "Create the upstream on first push"),
    ("merge.conflictstyle", "Conflict marker style"),
    ("rebase.autostash", "Stash before rebasing"),
    ("diff.tool", "External diff tool"),
    ("commit.gpgsign", "Sign commits"),
    ("fetch.prune", "Drop deleted remote branches on fetch"),
    ("alias.", "Define a Git alias"),
];

/// Directory names a Python virtual environment conventionally uses.
pub(super) const VENV_DIR_NAMES: &[&str] = &[".venv", "venv", "env", ".env", "virtualenv"];

/// Words that open a shell construct. They are typed where a command is
/// typed, so a command position must offer them; the description is what the
/// construct does, since the word alone rarely says it.
pub(super) const SHELL_KEYWORDS: &[(&str, &str)] = &[
    ("if", "conditional"),
    ("then", "conditional body"),
    ("else", "conditional alternative"),
    ("elif", "further condition"),
    ("fi", "end a conditional"),
    ("for", "loop over words"),
    ("while", "loop while a command succeeds"),
    ("until", "loop until a command succeeds"),
    ("do", "loop body"),
    ("done", "end a loop"),
    ("case", "match a value"),
    ("esac", "end a match"),
    ("in", "the words of a for or case"),
    ("function", "define a function"),
    ("select", "menu loop"),
    ("time", "time a command"),
];
