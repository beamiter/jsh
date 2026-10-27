//! The shell's call stack as bash exposes it: `FUNCNAME`, `BASH_SOURCE`,
//! `BASH_LINENO`, `caller`, `$LINENO`, `BASH_COMMAND`, `$_` and `local -`.
//!
//! Bash keeps one frame per executing function and sourced file, plus a
//! `main` frame at the bottom when it runs a script (not for `-c` or an
//! interactive shell). Index 0 of each array is the innermost frame:
//! `${FUNCNAME[i]}` was called from line `${BASH_LINENO[i]}` of the file
//! `${BASH_SOURCE[i+1]}`. `FUNCNAME` exists only while a function runs.
//!
//! Line numbers in an AST count from 1 in the text it was parsed from. A
//! program parsed out of the middle of another one — an `eval` string, a trap
//! action, an alias body, a command substitution — continues the line count
//! of the command that started it, as bash's do, through `line_offset`.

use crate::environment::ShellState;
use std::collections::HashMap;
use std::fmt::Write as _;

/// One executing function, sourced file or script.
#[derive(Debug, Clone)]
pub struct CallFrame {
    /// `FUNCNAME` entry: the function name, `source`, or `main`.
    funcname: String,
    /// `BASH_SOURCE` entry: the file the frame's code was read from.
    source: String,
    /// `BASH_LINENO` entry: the caller's line (0 for `main`).
    call_line: usize,
    is_function: bool,
    /// `local -` in this function: the `set` options to put back on return.
    saved_options: Option<SetOptions>,
}

impl CallFrame {
    pub fn function(name: &str, source: String, call_line: usize) -> Self {
        CallFrame {
            funcname: name.to_string(),
            source,
            call_line,
            is_function: true,
            saved_options: None,
        }
    }

    pub fn sourced_file(path: &str, call_line: usize) -> Self {
        CallFrame {
            funcname: "source".to_string(),
            source: path.to_string(),
            call_line,
            is_function: false,
            saved_options: None,
        }
    }

    pub fn main_script(path: &str) -> Self {
        CallFrame {
            funcname: "main".to_string(),
            source: path.to_string(),
            call_line: 0,
            is_function: false,
            saved_options: None,
        }
    }
}

/// Where a function was defined.
#[derive(Debug, Clone)]
struct FunctionOrigin {
    /// What `BASH_SOURCE` reports for the function's frame.
    source: String,
    /// `line_offset` of the program that defined it, so its body keeps
    /// reporting the lines it was written on.
    line_offset: usize,
}

/// The options `set` controls, which `local -` makes local to a function.
#[derive(Debug, Clone)]
struct SetOptions {
    errexit: bool,
    nounset: bool,
    xtrace: bool,
    noclobber: bool,
    pipefail: bool,
    noglob: bool,
    tracked: HashMap<String, bool>,
}

impl SetOptions {
    fn capture(state: &ShellState) -> Self {
        let opts = &state.shell_opts;
        SetOptions {
            errexit: opts.errexit,
            nounset: opts.nounset,
            xtrace: opts.xtrace,
            noclobber: opts.noclobber,
            pipefail: opts.pipefail,
            noglob: opts.noglob,
            tracked: opts.tracked_opts.clone(),
        }
    }

    fn restore(self, state: &mut ShellState) {
        let opts = &mut state.shell_opts;
        opts.errexit = self.errexit;
        opts.nounset = self.nounset;
        opts.xtrace = self.xtrace;
        opts.noclobber = self.noclobber;
        opts.pipefail = self.pipefail;
        opts.noglob = self.noglob;
        opts.tracked_opts = self.tracked;
    }
}

/// Call-stack and current-command bookkeeping for one shell.
#[derive(Debug, Default)]
pub struct CallStack {
    /// Outermost first.
    frames: Vec<CallFrame>,
    lineno: usize,
    /// `lineno` in decimal, kept current so `$LINENO` can be borrowed like
    /// any other variable value. Empty until the first command runs.
    lineno_text: String,
    /// Added to an AST line to give `$LINENO` for the program now running.
    pub line_offset: usize,
    /// `BASH_COMMAND`: the command being executed (not updated by commands
    /// that a trap action runs). None until the first command runs.
    bash_command: Option<String>,
    /// `$_`: the last argument of the previous simple command, once one has
    /// run. Before that the inherited environment value shows through.
    last_argument: Option<String>,
    origins: HashMap<String, FunctionOrigin>,
}

impl CallStack {
    /// Values of the call-stack parameters that are computed rather than
    /// stored as ordinary variables. Before the first command has run, the
    /// ordinary (inherited) variable shows through.
    pub fn dynamic_var(&self, name: &str) -> Option<&str> {
        match name {
            "LINENO" if !self.lineno_text.is_empty() => Some(&self.lineno_text),
            "BASH_COMMAND" => self.bash_command.as_deref(),
            "_" => self.last_argument.as_deref(),
            _ => None,
        }
    }

    pub fn lineno(&self) -> usize {
        self.lineno
    }

    /// Make `line` the current `$LINENO` as is (already absolute).
    pub fn set_lineno(&mut self, line: usize) {
        if self.lineno != line || self.lineno_text.is_empty() {
            self.lineno = line;
            self.lineno_text.clear();
            let _ = write!(self.lineno_text, "{line}");
        }
    }

    /// A command parsed on `ast_line` is about to run.
    pub fn enter_line(&mut self, ast_line: usize) {
        // Zero marks an AST without line information (an older snapshot).
        if ast_line != 0 {
            self.set_lineno(ast_line.saturating_add(self.line_offset));
        }
    }

    pub fn set_bash_command(&mut self, text: &str) {
        let slot = self.bash_command.get_or_insert_with(String::new);
        slot.clear();
        slot.push_str(text);
    }

    pub fn set_last_argument(&mut self, argument: &str) {
        let slot = self.last_argument.get_or_insert_with(String::new);
        slot.clear();
        slot.push_str(argument);
    }

    /// Line offset that makes line 1 of a nested program (an `eval` string,
    /// a trap action) report the line of the command that started it.
    pub fn nested_line_offset(&self) -> usize {
        self.lineno.saturating_sub(1)
    }

    pub fn function_depth(&self) -> usize {
        self.frames.iter().filter(|frame| frame.is_function).count()
    }

    /// The source file and line offset a function body runs with.
    fn origin_of(&self, name: &str, interactive: bool) -> (String, usize) {
        match self.origins.get(name) {
            Some(origin) => (origin.source.clone(), origin.line_offset),
            None => (default_function_source(interactive).to_string(), 0),
        }
    }
}

/// `BASH_SOURCE` for a function defined outside any file: bash says `main`
/// for one typed at an interactive prompt and `environment` otherwise.
fn default_function_source(interactive: bool) -> &'static str {
    if interactive {
        "main"
    } else {
        "environment"
    }
}

impl ShellState {
    /// Record where function `name` is being defined.
    pub fn note_function_definition(&mut self, name: &str) {
        let source = match self.call_stack.frames.last() {
            Some(frame) => frame.source.clone(),
            None => default_function_source(self.interactive).to_string(),
        };
        let line_offset = self.call_stack.line_offset;
        self.call_stack.origins.insert(
            name.to_string(),
            FunctionOrigin {
                source,
                line_offset,
            },
        );
    }

    /// Push the frame of a call to function `name` from the current line and
    /// switch line numbering to the function's own text. Returns the
    /// caller's line offset for [`ShellState::leave_function_frame`].
    pub fn enter_function_frame(&mut self, name: &str) -> usize {
        let (source, offset) = self.call_stack.origin_of(name, self.interactive);
        let call_line = self.call_stack.lineno;
        self.push_call_frame(CallFrame::function(name, source, call_line));
        std::mem::replace(&mut self.call_stack.line_offset, offset)
    }

    /// Pop a function frame: undo `local -`, and resume the caller's line
    /// numbering at the line of the call.
    pub fn leave_function_frame(&mut self, caller_offset: usize) {
        let frame = self.call_stack.frames.pop();
        if let Some(options) = frame.as_ref().and_then(|f| f.saved_options.clone()) {
            options.restore(self);
        }
        self.call_stack.line_offset = caller_offset;
        if let Some(frame) = frame {
            self.call_stack.set_lineno(frame.call_line);
        }
        self.sync_call_stack_vars();
    }

    pub fn push_call_frame(&mut self, frame: CallFrame) {
        self.call_stack.frames.push(frame);
        self.sync_call_stack_vars();
    }

    pub fn pop_call_frame(&mut self) {
        self.call_stack.frames.pop();
        self.sync_call_stack_vars();
    }

    /// `local -`: restore the `set` options when the innermost function
    /// returns. Returns false outside a function.
    pub fn save_options_for_function(&mut self) -> bool {
        let options = SetOptions::capture(self);
        match self
            .call_stack
            .frames
            .iter_mut()
            .rev()
            .find(|frame| frame.is_function)
        {
            Some(frame) => {
                // Only the first `local -` in a call matters: later ones must
                // not capture options the function itself already changed.
                frame.saved_options.get_or_insert(options);
                true
            }
            None => false,
        }
    }

    /// Mirror the frames into `FUNCNAME`, `BASH_SOURCE` and `BASH_LINENO`.
    fn sync_call_stack_vars(&mut self) {
        let frames = &self.call_stack.frames;
        let has_function = frames.iter().any(|frame| frame.is_function);
        let funcnames: Vec<String> = frames.iter().rev().map(|f| f.funcname.clone()).collect();
        let sources: Vec<String> = frames.iter().rev().map(|f| f.source.clone()).collect();
        let lines: Vec<String> = frames
            .iter()
            .rev()
            .map(|f| f.call_line.to_string())
            .collect();
        if has_function {
            self.set_shell_array("FUNCNAME", funcnames);
        } else {
            self.unset_shell_array("FUNCNAME");
        }
        if sources.is_empty() {
            self.unset_shell_array("BASH_SOURCE");
            self.unset_shell_array("BASH_LINENO");
        } else {
            self.set_shell_array("BASH_SOURCE", sources);
            self.set_shell_array("BASH_LINENO", lines);
        }
    }
}

/// `caller [N]`: the line and file of the current call, or with N the line,
/// function and file of frame N, exactly as bash derives them from its
/// `BASH_LINENO`, `FUNCNAME` and `BASH_SOURCE` arrays.
pub fn builtin_caller(args: &[String], state: &mut ShellState) -> i32 {
    let frames = &state.call_stack.frames;
    // Innermost first, like the arrays.
    let at = |i: usize| frames.len().checked_sub(i + 1).map(|index| &frames[index]);
    if frames.is_empty() {
        return 1;
    }
    let args: Vec<&String> = match args.first().map(String::as_str) {
        Some("--") => args[1..].iter().collect(),
        _ => args.iter().collect(),
    };
    let Some(arg) = args.first() else {
        // The innermost frame exists (the stack is not empty); its caller's
        // file may not, which bash prints as NULL.
        let line = at(0).map_or(0, |frame| frame.call_line);
        let source = at(1).map_or("NULL", |frame| frame.source.as_str());
        println!("{line} {source}");
        return 0;
    };
    let Ok(index) = arg.trim().parse::<usize>() else {
        if arg.trim().parse::<i64>().is_ok() {
            // A negative frame number names no frame.
            return 1;
        }
        eprintln!("jsh: caller: {arg}: invalid number");
        eprintln!("caller: usage: caller [expr]");
        return 2;
    };
    let (Some(frame), Some(caller)) = (at(index), index.checked_add(1).and_then(at)) else {
        return 1;
    };
    println!("{} {} {}", frame.call_line, caller.funcname, caller.source);
    0
}
