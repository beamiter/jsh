/// Line editor: raw mode, cursor movement, inline editing, integration with
/// highlighting, suggestions, and completion. Supports multiline editing.
use crate::ai::{AiConfig, AiContext, AiRequest, AiResponse, AiWorker};
use crate::completer::{self, common_prefix, Completion, CompletionKind};
use crate::environment::ShellState;
use crate::highlighter;
use crate::history::History;
use crate::prompt;
use crate::signal::{SIGHUP_RECEIVED, SIGINT_RECEIVED};
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
    ai_pending: bool,
    ai_explain_mode: bool,
    ai_saved_input: Option<AiInputSnapshot>,
    ai_error: Option<String>,
    pub last_error_info: Option<(String, String, i32)>,
    pub last_error_execution_id: Option<String>,
    pub key_bindings: crate::keybindings::KeyBindingManager,
    cached_prompt: String,
    last_buffer_snapshot: String,
    last_cursor_snapshot: usize,
    last_suggestion_snapshot: Option<String>,
    last_menu_snapshot: Option<usize>,
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

struct WorkflowMode {
    query: String,
    results: Vec<workflows::Workflow>,
    selected: usize,
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
            ai_pending: false,
            ai_explain_mode: false,
            ai_saved_input: None,
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
            last_ai_error_snapshot: None,
            pushback_byte: None,
        }
    }

    pub fn read_line(
        &mut self,
        state: &mut ShellState,
        history: &mut History,
    ) -> io::Result<Option<String>> {
        self.buffer.clear();
        self.cursor = 0;
        self.suggestion = None;
        self.saved_buffer.clear();
        self.completion_menu = None;
        self.search_mode = None;
        self.workflow_mode = None;
        self.ai_error = None;
        self.vi_mode = ViMode::Insert;
        self.vi_pending = None;
        history.reset_position();

        // Review-only prefill (agent insert path): the text lands in the
        // editor exactly like typed input and is never submitted here.
        if let Some(prefill) = state.pending_editor_insert.take() {
            self.buffer.push_str(&prefill);
            self.cursor = self.buffer.len();
        }

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

        self.cached_prompt = prompt::render_prompt(state);
        let prompt_lines = self.cached_prompt.matches('\n').count() as u16;
        self.last_rendered_lines = prompt_lines;
        self.last_cursor_row = prompt_lines;
        print!("{}", self.cached_prompt);
        io::stdout().flush()?;

        // OSC 133;B — prompt end / command input start marker
        if state.interactive {
            crate::osc::command_start();
        }

        terminal::enable_raw_mode()?;
        stdout().execute(event::EnableBracketedPaste).ok();
        let result = self.edit_loop(state, history);
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
        if self.suggestion.is_some() || !self.buffer.is_empty() {
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
                        // AI failures are rendered as a transient status line. Any
                        // subsequent key dismisses it while leaving the restored
                        // command buffer intact.
                        self.ai_error = None;
                        if key.code != KeyCode::Tab
                            && key.code != KeyCode::BackTab
                            && key.code != KeyCode::Enter
                        {
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

                        self.update_suggestion(history, state);
                        self.repaint(state)?;
                    }
                    Event::Paste(text) => {
                        self.ai_error = None;
                        if text.chars().all(|ch| {
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
                    // Check for AI response
                    if self.ai_pending {
                        if let Some(ref worker) = self.ai_worker {
                            if let Some(resp) = worker.try_recv() {
                                self.ai_pending = false;
                                self.apply_ai_response(resp);
                                self.repaint(state)?;
                            }
                        }
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
            (KeyCode::Char('g'), KeyModifiers::CONTROL) => {
                let all = state.workflow_registry.search("");
                self.workflow_mode = Some(WorkflowMode {
                    query: String::new(),
                    results: all.into_iter().cloned().collect(),
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
                if let Some(ref mut menu) = self.completion_menu {
                    if menu.selected == 0 {
                        menu.selected = menu.completions.len() - 1;
                    } else {
                        menu.selected -= 1;
                    }
                    let text = menu.completions[menu.selected].text.clone();
                    self.buffer
                        .replace_range(menu.word_start..self.cursor, &text);
                    self.cursor = menu.word_start + text.len();
                }
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
        _state: &mut ShellState,
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

    fn handle_workflow_key(
        &mut self,
        key: KeyEvent,
        state: &mut ShellState,
    ) -> io::Result<KeyAction> {
        let wf_mode = self.workflow_mode.as_mut().unwrap();
        match key.code {
            KeyCode::Esc => {
                self.workflow_mode = None;
            }
            KeyCode::Enter => {
                if let Some(wf) = wf_mode.results.get(wf_mode.selected).cloned() {
                    let rendered = workflows::fill_template(
                        &wf.command,
                        &wf.parameters
                            .iter()
                            .map(|p| {
                                (
                                    p.name.clone(),
                                    p.default
                                        .clone()
                                        .unwrap_or_else(|| format!("{{{{{}}}}}", p.name)),
                                )
                            })
                            .collect::<Vec<_>>(),
                    );
                    self.buffer = rendered;
                    self.cursor = self.buffer.len();
                    self.workflow_mode = None;
                }
            }
            KeyCode::Up => {
                if wf_mode.selected > 0 {
                    wf_mode.selected -= 1;
                }
            }
            KeyCode::Down => {
                if !wf_mode.results.is_empty() {
                    wf_mode.selected = (wf_mode.selected + 1).min(wf_mode.results.len() - 1);
                }
            }
            KeyCode::Backspace => {
                wf_mode.query.pop();
                wf_mode.results = state
                    .workflow_registry
                    .search(&wf_mode.query)
                    .into_iter()
                    .cloned()
                    .collect();
                wf_mode.selected = 0;
            }
            KeyCode::Char(c)
                if key.modifiers == KeyModifiers::NONE || key.modifiers == KeyModifiers::SHIFT =>
            {
                if crate::terminal_text::is_terminal_ambiguous(c) {
                    return Ok(KeyAction::Continue);
                }
                wf_mode.query.push(c);
                wf_mode.results = state
                    .workflow_registry
                    .search(&wf_mode.query)
                    .into_iter()
                    .cloned()
                    .collect();
                wf_mode.selected = 0;
            }
            _ => {
                self.workflow_mode = None;
            }
        }
        Ok(KeyAction::Continue)
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

    fn trigger_ai_generate(
        &mut self,
        prompt_text: &str,
        state: &ShellState,
        history: &History,
    ) -> bool {
        if self.ai_worker.is_none() {
            return false;
        }
        let ctx = self.build_ai_context(state, history);
        if !self.ai_worker.as_ref().unwrap().request(AiRequest {
            prompt: prompt_text.to_string(),
            context: ctx,
        }) {
            return false;
        }
        self.snapshot_ai_input();
        self.ai_pending = true;
        self.buffer.clear();
        self.buffer.push_str("[AI...]");
        self.cursor = self.buffer.len();
        true
    }

    fn trigger_ai_fix(&mut self, state: &ShellState, history: &History) -> bool {
        if self.last_error_info.is_none() || self.ai_worker.is_none() {
            return false;
        }
        let ctx = self.build_ai_context(state, history);
        if !self.ai_worker.as_ref().unwrap().request(AiRequest {
            prompt: String::new(),
            context: ctx,
        }) {
            return false;
        }
        self.snapshot_ai_input();
        self.ai_pending = true;
        self.buffer.clear();
        self.buffer.push_str("[AI fixing...]");
        self.cursor = self.buffer.len();
        true
    }

    fn trigger_ai_explain(&mut self, state: &ShellState, history: &History) -> bool {
        if self.ai_worker.is_none() {
            return false;
        }
        let mut ctx = self.build_ai_context(state, history);
        ctx.last_error = None;
        let prompt = format!(
            "Explain this shell command briefly (one line per flag/component): {}",
            self.buffer
        );
        if !self.ai_worker.as_ref().unwrap().request(AiRequest {
            prompt,
            context: ctx,
        }) {
            return false;
        }
        self.snapshot_ai_input();
        self.ai_pending = true;
        self.ai_explain_mode = true;
        true
    }

    fn apply_ai_response(&mut self, response: AiResponse) {
        self.ai_pending = false;
        self.ai_explain_mode = false;
        match response {
            AiResponse::Suggestion(command) => {
                if !crate::terminal_text::is_safe_inline(&command) {
                    if let Some(saved) = self.ai_saved_input.take() {
                        self.buffer = saved.buffer;
                        self.cursor = saved.cursor.min(self.buffer.len());
                        self.suggestion = saved.suggestion;
                    }
                    self.ai_error =
                        Some("AI error: suggestion contained invisible terminal text".to_string());
                    return;
                }
                self.ai_saved_input = None;
                self.ai_error = None;
                self.buffer.clear();
                self.cursor = 0;
                self.suggestion = Some(command);
            }
            AiResponse::Error(error) => {
                if let Some(saved) = self.ai_saved_input.take() {
                    self.buffer = saved.buffer;
                    self.cursor = saved.cursor.min(self.buffer.len());
                    self.suggestion = saved.suggestion;
                }
                self.ai_error = Some(format_ai_error(&error));
            }
        }
    }

    fn update_suggestion(&mut self, history: &History, state: &ShellState) {
        if self.completion_menu.is_some() || self.search_mode.is_some() {
            self.suggestion = None;
            return;
        }
        let ctx = suggest::SuggestionContext {
            git_branch: state.cached_git_branch.as_deref(),
            git_remote: state.cached_git_remote.as_deref(),
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
            && self.ai_error == self.last_ai_error_snapshot
            && self.cursor != self.last_cursor_snapshot;

        self.last_buffer_snapshot = self.buffer.clone();
        self.last_cursor_snapshot = self.cursor;
        self.last_suggestion_snapshot = self.suggestion.clone();
        self.last_menu_snapshot = menu_sel;
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

        // Fast path: only cursor moved, skip full redraw
        if cursor_only && !self.buffer.contains('\n') {
            let prompt_last = self
                .cached_prompt
                .rsplit('\n')
                .next()
                .unwrap_or(&self.cached_prompt);
            let prompt_width = display_width(prompt_last) as u16;
            let buf_before = &self.buffer[..self.cursor];
            let col = prompt_width + display_width(buf_before) as u16;
            out.queue(MoveToColumn(col))?;
            out.flush()?;
            return Ok(());
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
            // Render prompt (cached — only recomputed at read_line entry)
            let prompt_lines = self.cached_prompt.matches('\n').count() as u16;
            rendered_lines += prompt_lines;
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
                        rendered_lines += 1;
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

            // Compute cursor screen position accounting for continuation prompts
            let prompt_last_line = self
                .cached_prompt
                .rsplit('\n')
                .next()
                .unwrap_or(&self.cached_prompt);
            let prompt_width = display_width(prompt_last_line) as u16;
            let cont_width = display_width(&cont_prompt) as u16;
            let buf_before = &self.buffer[..self.cursor];
            let buf_cursor_lines = buf_before.matches('\n').count() as u16;
            let buf_cursor_last = buf_before.rsplit('\n').next().unwrap_or(buf_before);
            cursor_row = prompt_lines + buf_cursor_lines;
            cursor_col = if buf_cursor_lines > 0 {
                cont_width + display_width(buf_cursor_last) as u16
            } else {
                prompt_width + display_width(buf_cursor_last) as u16
            };

            if let Some(ref error) = self.ai_error {
                out.queue(Print("\r\n"))?;
                out.queue(SetForegroundColor(Color::Red))?;
                out.queue(Print(error))?;
                out.queue(ResetColor)?;
                rendered_lines += 1;
            // Phase 16d — signature hint line below the input. Only when nothing
            // else owns this slot: no completion menu, widget mode, or AI error.
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
                    rendered_lines += 1;
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

                // Display name
                let name_width = 20usize.min(self.terminal_width as usize / 3);
                let (display_name, display_name_width) =
                    history_panel_text(&item.comp.display, name_width);
                out.queue(Print(display_name))?;
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

                // Description (dim, after name) — skip generic kind labels
                if !is_selected {
                    if let Some(ref d) = item.comp.description {
                        if d != "builtin" && d != "alias" && d != "function" {
                            out.queue(SetAttribute(Attribute::Dim))?;
                            out.queue(SetForegroundColor(Color::White))?;
                            let max_desc =
                                (self.terminal_width as usize).saturating_sub(name_width + 5);
                            let (description, _) = history_panel_text(d, max_desc);
                            out.queue(Print(description))?;
                            out.queue(ResetColor)?;
                        }
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

            // Header
            out.queue(SetForegroundColor(Color::Magenta))?;
            out.queue(SetAttribute(Attribute::Bold))?;
            out.queue(Print(" WORKFLOWS "))?;
            out.queue(ResetColor)?;
            out.queue(SetForegroundColor(Color::Yellow))?;
            out.queue(Print(format!(
                "[{}/{}] ",
                wf_mode.results.len(),
                wf_mode.results.len()
            )))?;
            out.queue(ResetColor)?;
            let query_width = (self.terminal_width as usize).saturating_sub(2);
            let (query, _) = history_panel_text(&wf_mode.query, query_width);
            out.queue(Print(format!("❯ {query}")))?;
            out.queue(Print("\r\n"))?;
            rendered_lines += 1;

            let max_show = 10usize.min(self.terminal_height as usize / 3);
            for (i, wf) in wf_mode.results.iter().take(max_show).enumerate() {
                let is_sel = i == wf_mode.selected;

                if is_sel {
                    out.queue(SetForegroundColor(Color::Green))?;
                    out.queue(SetAttribute(Attribute::Bold))?;
                    out.queue(Print("▸ "))?;
                } else {
                    out.queue(Print("  "))?;
                }

                // Workflow name
                out.queue(SetForegroundColor(if is_sel {
                    Color::Green
                } else {
                    Color::Cyan
                }))?;
                out.queue(SetAttribute(Attribute::Bold))?;
                let (name, name_width) = history_panel_text(&wf.name, 20);
                out.queue(Print(name))?;
                out.queue(Print(" ".repeat(20usize.saturating_sub(name_width))))?;
                out.queue(ResetColor)?;

                // Description
                out.queue(SetAttribute(Attribute::Dim))?;
                let max_desc = (self.terminal_width as usize).saturating_sub(25);
                let (desc, _) = history_panel_text(&wf.description, max_desc);
                out.queue(Print(&desc))?;
                out.queue(ResetColor)?;

                out.queue(Print("\r\n"))?;
                rendered_lines += 1;

                // Show command preview for selected item
                if is_sel {
                    out.queue(Print("    "))?;
                    out.queue(SetAttribute(Attribute::Dim))?;
                    out.queue(SetForegroundColor(Color::White))?;
                    let (cmd_preview, _) = history_panel_text(
                        &wf.command,
                        (self.terminal_width as usize).saturating_sub(6),
                    );
                    out.queue(Print(&cmd_preview))?;
                    out.queue(ResetColor)?;
                    out.queue(Print("\r\n"))?;
                    rendered_lines += 1;
                }
            }

            if wf_mode.results.len() > max_show {
                out.queue(SetAttribute(Attribute::Dim))?;
                out.queue(Print(format!(
                    "  ... +{} more",
                    wf_mode.results.len() - max_show
                )))?;
                out.queue(ResetColor)?;
                out.queue(Print("\r\n"))?;
                rendered_lines += 1;
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

fn char_width(c: char) -> usize {
    if c == '\0' {
        return 0;
    }
    UnicodeWidthChar::width(c).unwrap_or(0)
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
        editor.ai_pending = true;

        editor.apply_ai_response(AiResponse::Error(
            "\x1b[31mservice unavailable\x1b[0m\ntry again\u{202e}\u{200b}".to_string(),
        ));

        assert_eq!(editor.buffer, "# list project files");
        assert_eq!(editor.cursor, editor.buffer.len());
        assert_eq!(editor.suggestion.as_deref(), Some(" previous suggestion"));
        assert_eq!(
            editor.ai_error.as_deref(),
            Some("AI error: service unavailable try again\\u{202e}\\u{200b}")
        );
        assert!(!editor.ai_pending);
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
}
