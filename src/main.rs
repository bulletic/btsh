use std::collections::HashMap;
use std::env;
use std::ffi::CStr;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::io::{FromRawFd, IntoRawFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{self, Command, Stdio, Child};
use std::sync::atomic::{AtomicBool, Ordering};

use glob::glob;
use unicode_width::UnicodeWidthStr;

// =============================================================================
// Types
// =============================================================================

#[derive(Debug, Clone)]
struct Redirect {
    fd: i32,
    kind: RedirectKind,
    target: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum RedirectKind {
    In,
    Out,
    Append,
    DupOut,
    DupIn,
    Close,
}

#[derive(Debug, Clone)]
struct Simple {
    args: Vec<String>,
    redirects: Vec<Redirect>,
}

#[derive(Debug)]
enum Node {
    Simple(Simple, bool),
    Pipeline(Vec<Simple>, bool),
    And(Box<Node>, Box<Node>),
    Or(Box<Node>, Box<Node>),
}

struct Shell {
    last_exit: i32,
    last_bg_pid: Option<u32>,
    running_bg: Vec<u32>,
    vars: HashMap<String, String>,
    aliases: HashMap<String, String>,
    config_path: Option<std::path::PathBuf>,
    last_cmd: String,
    prev_cmd: String,
    background_job: bool,
    logging: bool,
    shit: bool,
    history: bool,
    autosuggest: bool,
    fresh: bool,
}

// =============================================================================
// Signal handling
// =============================================================================

static SIGINT_RECEIVED: AtomicBool = AtomicBool::new(false);

extern "C" fn sigint_handler(_sig: i32) {
    SIGINT_RECEIVED.store(true, Ordering::SeqCst);
}

fn setup_signal_handlers() {
    unsafe {
        libc::signal(libc::SIGINT, sigint_handler as *const () as usize);
        libc::signal(libc::SIGQUIT, libc::SIG_IGN);
    }
}

fn sigint_pending() -> bool {
    SIGINT_RECEIVED.swap(false, Ordering::SeqCst)
}

// =============================================================================
// Interactive line editor
// =============================================================================

enum Key {
    Char(char),
    Enter,
    Tab,
    Backspace,
    Delete,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    CtrlC,
    CtrlD,
    CtrlL,
    Unknown,
}

fn enable_raw_mode() -> libc::termios {
    unsafe {
        let mut raw: libc::termios = std::mem::zeroed();
        libc::tcgetattr(libc::STDIN_FILENO, &mut raw);
        let original = raw;
        raw.c_iflag &= !(libc::BRKINT | libc::ICRNL | libc::INPCK | libc::ISTRIP | libc::IXON);
        raw.c_oflag |= libc::OPOST;
        raw.c_cflag |= libc::CS8;
        raw.c_lflag &= !(libc::ECHO | libc::ICANON | libc::IEXTEN | libc::ISIG);
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;
        libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, &raw);
        *RAW_TERMIOS.lock().unwrap() = Some(raw);
        original
    }
}

fn disable_raw_mode(original: &libc::termios) {
    unsafe {
        libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, original);
    }
}

static ORIG_TERMIOS: std::sync::Mutex<Option<libc::termios>> = std::sync::Mutex::new(None);
static RAW_TERMIOS: std::sync::Mutex<Option<libc::termios>> = std::sync::Mutex::new(None);

fn set_cooked_mode() {
    let guard = ORIG_TERMIOS.lock().unwrap();
    if let Some(ref orig) = *guard {
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, orig);
        }
    }
}

fn set_raw_mode() {
    let guard = RAW_TERMIOS.lock().unwrap();
    if let Some(ref raw) = *guard {
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, raw);
        }
    }
}

fn prepare_job_terminal(shell: &Shell) {
    if !shell.background_job {
        set_cooked_mode();
    }
}

fn restore_shell_terminal(shell: &Shell) {
    if !shell.background_job {
        set_raw_mode();
    }
}

fn read_byte() -> Option<u8> {
    let mut buf = [0u8; 1];
    loop {
        if sigint_pending() {
            return Some(3); // Ctrl-C
        }
        let n = unsafe {
            libc::read(libc::STDIN_FILENO, buf.as_mut_ptr() as *mut libc::c_void, 1)
        };
        if n == 1 {
            return Some(buf[0]);
        }
        if n == 0 {
            return None; // EOF
        }
        // EINTR: loop back and check sigint_pending
    }
}

fn read_key() -> Option<Key> {
    let b = read_byte()?;
    Some(match b {
        0x03 => Key::CtrlC,
        0x04 => Key::CtrlD,
        0x0c => Key::CtrlL,
        0x09 => Key::Tab,
        0x0a | 0x0d => Key::Enter,
        0x7f => Key::Backspace,
        0x1b => {
            let b2 = read_byte()?;
            if b2 != 0x5b {
                Key::Unknown
            } else {
                let b3 = read_byte()?;
                match b3 {
                    0x41 => Key::Up,
                    0x42 => Key::Down,
                    0x43 => Key::Right,
                    0x44 => Key::Left,
                    0x48 => Key::Home,
                    0x46 => Key::End,
                    0x33 => {
                        let b4 = read_byte().unwrap_or(0);
                        if b4 == 0x7e { Key::Delete } else { Key::Unknown }
                    }
                    0x31 => {
                        let b4 = read_byte().unwrap_or(0);
                        if b4 == 0x7e { Key::Home } else { Key::Unknown }
                    }
                    0x34 => {
                        let b4 = read_byte().unwrap_or(0);
                        if b4 == 0x7e { Key::End } else { Key::Unknown }
                    }
                    _ => Key::Unknown,
                }
            }
        }
        0x20..=0x7e => Key::Char(b as char),
        _ => {
            if b >= 0x80 {
                let len = match b {
                    0xc0..=0xdf => 2,
                    0xe0..=0xef => 3,
                    0xf0..=0xf7 => 4,
                    _ => 1,
                };
                let mut buf = [0u8; 4];
                buf[0] = b;
                for i in 1..len {
                    buf[i] = read_byte()?;
                }
                if let Ok(s) = std::str::from_utf8(&buf[..len]) {
                    Key::Char(s.chars().next().unwrap())
                } else {
                    Key::Unknown
                }
            } else {
                Key::Char(b as char)
            }
        }
    })
}

fn is_csi_terminator(c: char) -> bool {
    matches!(c, '@'..='~')
}

fn prompt_last_line_width(s: &str) -> usize {
    let last_line = s.lines().next_back().unwrap_or(s);
    let mut width = 0;
    let mut chars = last_line.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if let Some(next) = chars.next() {
                if next == '[' {
                    for c in chars.by_ref() {
                        if is_csi_terminator(c) {
                            break;
                        }
                    }
                } else if next.is_ascii_alphabetic() {
                    // two-char escape, consumed
                } else {
                    for c in chars.by_ref() {
                        if c.is_ascii_alphabetic() {
                            break;
                        }
                    }
                }
            }
        } else {
            width += 1;
        }
    }
    width
}


struct History {
    entries: Vec<String>,
    index: usize,
    saved: String,
    path: Option<std::path::PathBuf>,
}

fn history_file() -> std::path::PathBuf {
    env::var("HOME").ok()
        .map(|h| Path::new(&h).join(".local").join("share").join("bsh").join("history.txt"))
        .unwrap_or_else(|| Path::new("/dev/null").to_path_buf())
}

impl History {
    fn empty() -> Self {
        History { entries: Vec::new(), index: 0, saved: String::new(), path: None }
    }

    fn new() -> Self {
        let path = Some(history_file());

        let mut entries = Vec::new();
        if let Some(ref p) = path {
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            if let Ok(text) = std::fs::read_to_string(p) {
                for line in text.lines() {
                    let trimmed = line.trim();
                    if !trimmed.is_empty() {
                        entries.push(trimmed.to_string());
                    }
                }
            }
        }

        History { entries, index: 0, saved: String::new(), path }
    }

    fn add(&mut self, entry: &str) {
        let entry = entry.trim().to_string();
        if entry.is_empty() {
            return;
        }
        if self.entries.last().map_or(false, |last| *last == entry) {
            return;
        }
        if let Some(ref p) = self.path {
            if let Ok(f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)
            {
                use std::io::Write;
                writeln!(&f, "{entry}").ok();
            }
        }
        self.entries.push(entry);
        self.index = self.entries.len();
    }

    fn up(&mut self, current: &str) -> Option<String> {
        if self.entries.is_empty() {
            return None;
        }
        if self.index == self.entries.len() {
            self.saved = current.to_string();
        }
        if self.index > 0 {
            self.index -= 1;
            Some(self.entries[self.index].clone())
        } else {
            None
        }
    }

    fn down(&mut self) -> Option<String> {
        if self.index < self.entries.len() {
            self.index += 1;
            if self.index == self.entries.len() {
                Some(std::mem::take(&mut self.saved))
            } else {
                Some(self.entries[self.index].clone())
            }
        } else {
            None
        }
    }

    fn suggestion(&self, prefix: &str) -> Option<String> {
        self.entries.iter().rev().find_map(|entry| {
            if entry.starts_with(prefix) && entry.len() > prefix.len() {
                let suffix = &entry[prefix.len()..];
                let file_like: Vec<&str> = suffix.split_whitespace()
                    .filter(|w| !w.starts_with('-') && (w.contains('/') || w.contains('.')))
                    .collect();
                if file_like.is_empty() || file_like.iter().any(|w| Path::new(w).exists()) {
                    Some(suffix.to_string())
                } else {
                    None
                }
            } else {
                None
            }
        })
    }
}

struct CommandCache {
    path: String,
    names: Vec<String>,
}

static COMMAND_CACHE: std::sync::Mutex<Option<CommandCache>> = std::sync::Mutex::new(None);

fn get_commands() -> Vec<String> {
    let current_path = env::var("PATH").unwrap_or_default();
    let mut guard = COMMAND_CACHE.lock().unwrap();
    if let Some(cache) = guard.as_ref() {
        if cache.path == current_path {
            return cache.names.clone();
        }
    }
    let mut names = Vec::new();
    for dir in env::split_paths(&current_path) {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                if let Ok(ft) = entry.file_type() {
                    if ft.is_file() || ft.is_symlink() {
                        if let Ok(name) = entry.file_name().into_string() {
                            if !name.starts_with('.') {
                                names.push(name);
                            }
                        }
                    }
                }
            }
        }
    }
    names.sort();
    names.dedup();
    *guard = Some(CommandCache { path: current_path, names: names.clone() });
    names
}

fn find_last_word(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut i = line.len();
    while i > 0 {
        i -= 1;
        if bytes[i] == b' ' || bytes[i] == b'\t' {
            if i > 0 && bytes[i - 1] == b'\\' {
                if i > 1 {
                    i -= 1;
                }
                continue;
            }
            return &line[i + 1..];
        }
    }
    line
}

fn unescape_shell(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some(n) => out.push(n),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn escape_for_shell(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        match c {
            ' ' | '\\' | '|' | ';' | '&' | '<' | '>' | '#' | '$' | '"' | '\'' | '`' | '(' | ')' | '{' | '}' | '*' | '?' | '[' | '!' | '~' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

fn suggest_path(word: &str) -> Option<String> {
    let unescaped = unescape_shell(word);
    let (dir, prefix) = if let Some(pos) = unescaped.rfind('/') {
        (&unescaped[..=pos], &unescaped[pos + 1..])
    } else if unescaped.starts_with('~') {
        return None;
    } else {
        (".", unescaped.as_str())
    };
    let expanded = if dir.starts_with("~/") {
        let home = env::var("HOME").ok()?;
        if dir.len() == 2 {
            format!("{}/", home)
        } else {
            format!("{}/{}", home, &dir[2..])
        }
    } else if dir == "~" {
        let home = env::var("HOME").ok()?;
        format!("{}/", home)
    } else {
        dir.to_string()
    };
    let entries = std::fs::read_dir(&expanded).ok()?;
    let mut names: Vec<String> = entries.flatten().filter_map(|entry| {
        let name = entry.file_name().into_string().ok()?;
        if name.starts_with(prefix) {
            let is_dir = entry.file_type().ok()?.is_dir() || entry.file_type().ok()?.is_symlink();
            Some(if is_dir { format!("{}/", name) } else { name })
        } else {
            None
        }
    }).collect();
    names.sort();
    names.first().map(|s| escape_for_shell(&s[prefix.len()..]))
}

fn suggest_command(prefix: &str, history: &History) -> Option<String> {
    if prefix.is_empty() {
        return None;
    }
    let last = find_last_word(prefix);
    let show_path = last.contains('/') || last.starts_with('~') || last.starts_with('.') || prefix.contains(' ');
    if let Some(s) = history.suggestion(prefix) {
        return Some(s);
    }
    if show_path && !last.is_empty() {
        if let Some(s) = suggest_path(last) {
            return Some(s);
        }
    }
    for cmd in get_commands().iter() {
        if cmd.starts_with(prefix) && cmd.len() > prefix.len() {
            return Some(cmd[prefix.len()..].to_string());
        }
    }
    None
}

fn suggest_command_opt(prefix: &str, history: &History, suggest: bool) -> Option<String> {
    if suggest {
        suggest_command(prefix, history)
    } else {
        None
    }
}

struct Out {
    buf: String,
}

impl Out {
    fn new() -> Self {
        Out { buf: String::new() }
    }
    fn s(&mut self, s: &str) {
        self.buf.push_str(s);
    }
    fn c(&mut self, c: char) {
        self.buf.push(c);
    }
    fn flush(&mut self) {
        if !self.buf.is_empty() {
            unsafe {
                libc::write(libc::STDOUT_FILENO, self.buf.as_ptr() as *const libc::c_void, self.buf.len());
            }
            self.buf.clear();
        }
    }
}

enum ReadLineResult { Line(String), Eof, CtrlC }

fn terminal_width() -> usize {
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(libc::STDIN_FILENO, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 {
            ws.ws_col as usize
        } else {
            80
        }
    }
}

fn input_lines(input_width: usize, term_width: usize) -> usize {
    if input_width == 0 || term_width == 0 {
        1
    } else {
        (input_width + term_width - 1) / term_width
    }
}

fn read_line_interactive(prompt: &str, history: &mut History, suggest: bool) -> ReadLineResult {
    let prompt_width = prompt_last_line_width(prompt);
    let last_prompt_line = prompt.lines().next_back().unwrap_or("");
    let term_width = terminal_width();
    let mut out = Out::new();

    out.s("\r");
    out.s(prompt);
    out.flush();

    let mut line = String::new();
    let mut cursor = 0;
    let mut prev_lines = 1usize;
    let mut suggestion: Option<String> = None;

    let result = loop {
        if sigint_pending() {
            line.clear();
            cursor = 0;
            prev_lines = 1;
            suggestion = None;
            out.s("\r\x1b[J");
            out.s(last_prompt_line);
            out.flush();
        }

        match read_key() {
            None => {
                break ReadLineResult::Eof;
            }
            Some(Key::Enter) => {
                out.s(&line[cursor..]);
                out.s("\x1b[J");
                out.s("\r\n");
                out.flush();
                break ReadLineResult::Line(line);
            }
            Some(Key::Char(c)) => {
                if matches!(c, '"' | '\'' | ')' | ']' | '}') && line[cursor..].chars().next() == Some(c) {
                    cursor += c.len_utf8();
                    prev_lines = refresh_line(&mut out, prompt_width, last_prompt_line, &line, cursor, prev_lines, term_width, suggestion.as_deref());
                    out.flush();
                    continue;
                }
                let close = match c {
                    '{' => Some('}'),
                    '(' => Some(')'),
                    '[' => Some(']'),
                    '"' | '\'' => Some(c),
                    _ => None,
                };
                line.insert(cursor, c);
                cursor += c.len_utf8();
                if let Some(cl) = close {
                    line.insert(cursor, cl);
                }
                suggestion = suggest_command_opt(&line, &history, suggest);
                prev_lines = refresh_line(&mut out, prompt_width, last_prompt_line, &line, cursor, prev_lines, term_width, suggestion.as_deref());
                out.flush();
            }
            Some(Key::Backspace) => {
                if cursor > 0 {
                    let prev = line[..cursor].chars().next_back().unwrap();
                    cursor -= prev.len_utf8();
                    line.remove(cursor);
                    suggestion = suggest_command_opt(&line, &history, suggest);
                    prev_lines = refresh_line(&mut out, prompt_width, last_prompt_line, &line, cursor, prev_lines, term_width, suggestion.as_deref());
                    out.flush();
                }
            }
            Some(Key::Delete) => {
                if cursor < line.len() {
                    line.remove(cursor);
                    suggestion = suggest_command_opt(&line, &history, suggest);
                    prev_lines = refresh_line(&mut out, prompt_width, last_prompt_line, &line, cursor, prev_lines, term_width, suggestion.as_deref());
                    out.flush();
                }
            }
            Some(Key::Left) => {
                if cursor > 0 {
                    let prev = line[..cursor].chars().next_back().unwrap();
                    cursor -= prev.len_utf8();
                    prev_lines = refresh_line(&mut out, prompt_width, last_prompt_line, &line, cursor, prev_lines, term_width, suggestion.as_deref());
                    out.flush();
                }
            }
            Some(Key::Right) => {
                if cursor < line.len() {
                    let next = line[cursor..].chars().next().unwrap();
                    cursor += next.len_utf8();
                    prev_lines = refresh_line(&mut out, prompt_width, last_prompt_line, &line, cursor, prev_lines, term_width, suggestion.as_deref());
                    out.flush();
                } else if let Some(sug) = &suggestion {
                    if !sug.is_empty() {
                        let c = sug.chars().next().unwrap();
                        line.push(c);
                        cursor = line.len();
                        suggestion = suggest_command_opt(&line, &history, suggest);
                        prev_lines = refresh_line(&mut out, prompt_width, last_prompt_line, &line, cursor, prev_lines, term_width, suggestion.as_deref());
                        out.flush();
                    }
                }
            }
            Some(Key::Home) => {
                cursor = 0;
                prev_lines = refresh_line(&mut out, prompt_width, last_prompt_line, &line, cursor, prev_lines, term_width, suggestion.as_deref());
                out.flush();
            }
            Some(Key::End) => {
                cursor = line.len();
                prev_lines = refresh_line(&mut out, prompt_width, last_prompt_line, &line, cursor, prev_lines, term_width, suggestion.as_deref());
                out.flush();
            }
            Some(Key::Up) => {
                if let Some(hist_line) = history.up(&line) {
                    line = hist_line;
                    cursor = line.len();
                    suggestion = suggest_command_opt(&line, &history, suggest);
                    prev_lines = refresh_line(&mut out, prompt_width, last_prompt_line, &line, cursor, prev_lines, term_width, suggestion.as_deref());
                    out.flush();
                }
            }
            Some(Key::Down) => {
                if let Some(hist_line) = history.down() {
                    line = hist_line;
                    cursor = line.len();
                    suggestion = suggest_command_opt(&line, &history, suggest);
                    prev_lines = refresh_line(&mut out, prompt_width, last_prompt_line, &line, cursor, prev_lines, term_width, suggestion.as_deref());
                    out.flush();
                }
            }
            Some(Key::CtrlC) => {
                out.s("\r\x1b[J");
                out.s(last_prompt_line);
                out.flush();
                break ReadLineResult::CtrlC;
            }
            Some(Key::CtrlD) => {
                if line.is_empty() {
                    break ReadLineResult::Eof;
                }
            }
            Some(Key::CtrlL) => {
                out.s("\x1b[2J\x1b[H");
                out.s(prompt);
                prev_lines = refresh_line(&mut out, prompt_width, last_prompt_line, &line, cursor, prev_lines, term_width, suggestion.as_deref());
                out.flush();
            }
            Some(Key::Tab) => {
                if let Some(sug) = &suggestion {
                    if !sug.is_empty() {
                        line.push_str(sug);
                        cursor = line.len();
                        suggestion = suggest_command_opt(&line, &history, suggest);
                        prev_lines = refresh_line(&mut out, prompt_width, last_prompt_line, &line, cursor, prev_lines, term_width, suggestion.as_deref());
                        out.flush();
                    }
                }
            }
            Some(Key::Unknown) => {}
        }
    };

    result
}

fn write_int(out: &mut Out, n: usize) {
    let mut buf = [0u8; 16];
    let mut i = buf.len();
    let mut n = n;
    if n == 0 {
        i -= 1;
        buf[i] = b'0';
    } else {
        while n > 0 {
            i -= 1;
            buf[i] = b'0' + (n % 10) as u8;
            n /= 10;
        }
    }
    out.s(std::str::from_utf8(&buf[i..]).unwrap());
}

fn cursor_to(out: &mut Out, pos: usize) {
    if pos > 0 {
        out.s("\r\x1b[");
        write_int(out, pos);
        out.c('C');
    } else {
        out.c('\r');
    }
}

fn cursor_up(out: &mut Out, n: usize) {
    if n > 0 {
        out.s("\x1b[");
        write_int(out, n);
        out.c('A');
    }
}

fn refresh_line(out: &mut Out, prompt_width: usize, last_line: &str, line: &str, cursor: usize, prev_lines: usize, term_width: usize, suggestion: Option<&str>) -> usize {
    cursor_up(out, prev_lines - 1);
    out.s("\r\x1b[J");
    out.s(last_line);
    out.s(line);
    let sug = suggestion.filter(|_| cursor == line.len());
    if let Some(s) = sug {
        out.s("\x1b[2m");
        out.s(s);
        out.s("\x1b[22m");
    }
    let line_width = UnicodeWidthStr::width(line);
    let total_width = line_width + sug.map(|s| UnicodeWidthStr::width(s)).unwrap_or(0);
    let cursor_col = UnicodeWidthStr::width(&line[..cursor]);
    let new_lines = input_lines(prompt_width + total_width, term_width).max(1);
    cursor_to(out, prompt_width + cursor_col);
    new_lines
}

// =============================================================================
// Tokenizer
// =============================================================================

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Word(String),
    Pipe,
    Semicolon,
    And,
    Or,
    Background,
    Less,
    Great,
    DoubleGreat,
    LessGreat,
    GreatAnd,
    LessAnd,
    BothGreat,
    BothDoubleGreat,
}

struct Tokenizer<'a> {
    input: &'a str,
    pos: usize,
}

impl<'a> Tokenizer<'a> {
    fn new(input: &'a str) -> Self {
        Tokenizer { input, pos: 0 }
    }

    fn peek(&self) -> Option<char> {
        self.input[self.pos..].chars().next()
    }

    fn next(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    fn tokenize(&mut self) -> Result<Vec<Token>, String> {
        let mut tokens = Vec::new();
        loop {
            self.skip_whitespace();
            if self.pos >= self.input.len() {
                break;
            }
            match self.peek() {
                Some('#') => break,
                Some(';') => { self.next(); tokens.push(Token::Semicolon); }
                Some('|') => {
                    self.next();
                    if self.peek() == Some('|') {
                        self.next();
                        tokens.push(Token::Or);
                    } else {
                        tokens.push(Token::Pipe);
                    }
                }
                Some('&') => {
                    self.next();
                    if self.peek() == Some('>') {
                        self.next();
                        if self.peek() == Some('>') {
                            self.next();
                            tokens.push(Token::BothDoubleGreat);
                        } else {
                            tokens.push(Token::BothGreat);
                        }
                    } else if self.peek() == Some('&') {
                        self.next();
                        tokens.push(Token::And);
                    } else {
                        tokens.push(Token::Background);
                    }
                }
                Some('<') => {
                    self.next();
                    if self.peek() == Some('>') {
                        self.next();
                        tokens.push(Token::LessGreat);
                    } else if self.peek() == Some('&') {
                        self.next();
                        tokens.push(Token::LessAnd);
                    } else {
                        tokens.push(Token::Less);
                    }
                }
                Some('>') => {
                    self.next();
                    if self.peek() == Some('>') {
                        self.next();
                        tokens.push(Token::DoubleGreat);
                    } else if self.peek() == Some('&') {
                        self.next();
                        tokens.push(Token::GreatAnd);
                    } else {
                        tokens.push(Token::Great);
                    }
                }
                _ => {
                    let word = self.read_word()?;
                    tokens.push(Token::Word(word));
                }
            }
        }
        Ok(tokens)
    }

    fn skip_whitespace(&mut self) {
        while let Some(c) = self.peek() {
            if c.is_ascii_whitespace() {
                self.next();
            } else {
                break;
            }
        }
    }

    fn read_word(&mut self) -> Result<String, String> {
        let mut word = String::new();
        loop {
            let ch = match self.peek() {
                Some(c) => c,
                None => break,
            };
            if ch.is_ascii_whitespace() || "|;&<>#".contains(ch) {
                break;
            }
            if ch == '&' && word.is_empty() {
                break;
            }
            if ch == '\'' {
                self.next();
                let content = self.read_single_quote()?;
                word.push('\'');
                word.push_str(&content);
                word.push('\'');
            } else if ch == '"' {
                self.next();
                let content = self.read_double_quote()?;
                word.push('"');
                word.push_str(&content);
                word.push('"');
            } else if ch == '\\' {
                self.next();
                word.push('\\');
                match self.next() {
                    Some(c) => word.push(c),
                    None => {}
                }
            } else if ch == '$' {
                self.next();
                word.push('$');
                if self.peek() == Some('(') {
                    self.next();
                    word.push('(');
                    let mut depth = 1usize;
                    loop {
                        match self.next() {
                            Some('(') => { depth += 1; word.push('('); }
                            Some(')') => {
                                depth -= 1;
                                word.push(')');
                                if depth == 0 { break; }
                            }
                            Some(c) => word.push(c),
                            None => break,
                        }
                    }
                }
            } else {
                self.next();
                word.push(ch);
            }
        }
        if word.is_empty() {
            return Err("expected word".into());
        }
        Ok(word)
    }

    fn read_single_quote(&mut self) -> Result<String, String> {
        let mut s = String::new();
        loop {
            match self.next() {
                Some('\'') => return Ok(s),
                Some(c) => s.push(c),
                None => return Err("unterminated single quote".into()),
            }
        }
    }

    fn read_double_quote(&mut self) -> Result<String, String> {
        let mut s = String::new();
        loop {
            match self.next() {
                Some('"') => return Ok(s),
                Some('\\') => {
                    match self.next() {
                        Some('"') => { s.push('\\'); s.push('"'); }
                        Some('\n') => {}
                        Some(c) => { s.push('\\'); s.push(c); }
                        None => s.push('\\'),
                    }
                }
                Some('$') => {
                    s.push('$');
                    if self.peek() == Some('(') {
                        self.next();
                        s.push('(');
                        let mut depth = 1usize;
                        loop {
                            match self.next() {
                                Some('(') => { depth += 1; s.push('('); }
                                Some(')') => {
                                    depth -= 1;
                                    s.push(')');
                                    if depth == 0 { break; }
                                }
                                Some(c) => s.push(c),
                                None => return Err("unterminated command substitution inside double quotes".into()),
                            }
                        }
                    }
                }
                Some(c) => s.push(c),
                None => return Err("unterminated double quote".into()),
            }
        }
    }
}

// =============================================================================
// Expansion
// =============================================================================

fn expand_word(word: &str, shell: &Shell) -> Vec<String> {
    let mut results = Vec::new();
    let mut current = String::new();

    let mut chars = word.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some(next) => current.push(next),
                None => current.push('\\'),
            }
        } else if c == '\'' {
            loop {
                match chars.next() {
                    Some('\'') => break,
                    Some(c) => current.push(c),
                    None => break,
                }
            }
        } else if c == '"' {
            loop {
                match chars.next() {
                    Some('"') => break,
                    Some('\\') => {
                        match chars.next() {
                            Some('$') => current.push('$'),
                            Some('`') => current.push('`'),
                            Some('"') => current.push('"'),
                            Some('\\') => current.push('\\'),
                            Some('\n') => {}
                            Some(c) => {
                                current.push('\\');
                                current.push(c);
                            }
                            None => current.push('\\'),
                        }
                    }
                    Some('$') => {
                        let val = read_var(&mut chars, shell);
                        current.push_str(&val);
                    }
                    Some(c) => current.push(c),
                    None => break,
                }
            }
        } else if c == '$' {
            let val = read_var(&mut chars, shell);
            if val.is_empty() {
                continue;
            }
            let mut quote_pos = false;
            for vc in val.chars() {
                if vc.is_ascii_whitespace() {
                    if !current.is_empty() || quote_pos {
                        results.push(std::mem::take(&mut current));
                    }
                    quote_pos = true;
                } else {
                    current.push(vc);
                    quote_pos = false;
                }
            }
        } else if c == '~' && current.is_empty() && results.is_empty() {
            if chars.peek().map_or(true, |&n| n == '/' || n.is_ascii_whitespace() || n == ':') {
                let home = env::var("HOME").unwrap_or_else(|_| ".".into());
                current.push_str(&home);
            } else {
                current.push('~');
            }
        } else {
            current.push(c);
        }
    }

    if !current.is_empty() {
        results.push(current);
    }
    if results.is_empty() {
        results.push(String::new());
    }
    expand_globs(&mut results);
    results
}

fn expand_globs(results: &mut Vec<String>) {
    let mut i = 0;
    while i < results.len() {
        let word = &results[i];
        if !word.contains('*') && !word.contains('?') && !word.contains('[') {
            i += 1;
            continue;
        }
        let mut matches: Vec<String> = glob(word)
            .ok()
            .into_iter()
            .flat_map(|entries| {
                entries
                    .filter_map(|e| e.ok())
                    .map(|p| p.to_string_lossy().to_string())
                    .collect::<Vec<_>>()
            })
            .collect();
        if !matches.is_empty() {
            matches.sort();
            let n = matches.len();
            results.splice(i..=i, matches);
            i += n;
        } else {
            i += 1;
        }
    }
}

fn read_var(chars: &mut std::iter::Peekable<std::str::Chars>, shell: &Shell) -> String {
    match chars.peek() {
        Some('(') => {
            chars.next();
            let mut cmd = String::new();
            let mut depth = 1usize;
            loop {
                match chars.next() {
                    Some('(') => { depth += 1; cmd.push('('); }
                    Some(')') => {
                        depth -= 1;
                        if depth == 0 { break; }
                        cmd.push(')');
                    }
                    Some(c) => cmd.push(c),
                    None => break,
                }
            }
            execute_subshell(&cmd, shell)
        }
        Some('?') => {
            chars.next();
            shell.last_exit.to_string()
        }
        Some('$') => {
            chars.next();
            process::id().to_string()
        }
        Some('!') => {
            chars.next();
            shell.last_bg_pid.map(|p| p.to_string()).unwrap_or_default()
        }
        Some('{') => {
            chars.next();
            let mut name = String::new();
            loop {
                match chars.next() {
                    Some('}') => break,
                    Some(c) => name.push(c),
                    None => break,
                }
            }
            resolve_var(&name, shell)
        }
        Some(c) if c.is_alphanumeric() || *c == '_' => {
            let mut name = String::new();
            while let Some(&c) = chars.peek() {
                if c.is_alphanumeric() || c == '_' {
                    name.push(c);
                    chars.next();
                } else {
                    break;
                }
            }
            resolve_var(&name, shell)
        }
        _ => String::from("$"),
    }
}

fn resolve_var(name: &str, shell: &Shell) -> String {
    if let Ok(val) = env::var(name) {
        return val;
    }
    if let Some(val) = shell.vars.get(name) {
        return val.clone();
    }
    String::new()
}

fn expand_redirect_target(target: &str, shell: &Shell) -> String {
    let parts = expand_word(target, shell);
    parts.join(" ")
}

// =============================================================================
// Parser
// =============================================================================

fn parse(tokens: &[Token]) -> Result<Vec<Node>, String> {
    let mut nodes = Vec::new();
    let mut i = 0;

    while i < tokens.len() {
        let (node, consumed) = parse_and_or(tokens, i)?;
        if consumed == 0 {
            return Err("parse error: unexpected token".into());
        }
        nodes.push(node);
        i += consumed;

        if i < tokens.len() && tokens[i] == Token::Semicolon {
            i += 1;
        }
    }

    Ok(nodes)
}

fn parse_and_or(tokens: &[Token], start: usize) -> Result<(Node, usize), String> {
    let (mut node, mut consumed) = parse_pipeline(tokens, start)?;

    while start + consumed < tokens.len() {
        let op = &tokens[start + consumed];
        match op {
            Token::And => {
                consumed += 1;
                let (right, right_consumed) = parse_pipeline(tokens, start + consumed)?;
                consumed += right_consumed;
                node = Node::And(Box::new(node), Box::new(right));
            }
            Token::Or => {
                consumed += 1;
                let (right, right_consumed) = parse_pipeline(tokens, start + consumed)?;
                consumed += right_consumed;
                node = Node::Or(Box::new(node), Box::new(right));
            }
            _ => break,
        }
    }

    Ok((node, consumed))
}

fn parse_pipeline(tokens: &[Token], start: usize) -> Result<(Node, usize), String> {
    let mut cmds = Vec::new();
    let mut i = start;

    loop {
        let (simple, consumed) = parse_simple(tokens, i)?;
        cmds.push(simple);
        i += consumed;

        if i < tokens.len() && tokens[i] == Token::Pipe {
            i += 1;
        } else {
            break;
        }
    }

    if cmds.is_empty() {
        return Err("expected command".into());
    }

    let background = if i < tokens.len() && tokens[i] == Token::Background {
        i += 1;
        true
    } else {
        false
    };

    let consumed = i - start;
    if cmds.len() == 1 {
        Ok((Node::Simple(cmds.into_iter().next().unwrap(), background), consumed))
    } else {
        Ok((Node::Pipeline(cmds, background), consumed))
    }
}

fn parse_simple(tokens: &[Token], start: usize) -> Result<(Simple, usize), String> {
    let mut i = start;
    let mut args = Vec::new();
    let mut redirects = Vec::new();

    loop {
        if i >= tokens.len() {
            break;
        }
        match &tokens[i] {
            Token::Word(w) => {
                // Check for numeric fd prefix before a redirect operator
                if i + 1 < tokens.len() && !w.is_empty() && w.chars().all(|c| c.is_ascii_digit()) {
                    let fd: i32 = w.parse().unwrap();
                    let handled = match &tokens[i + 1] {
                        Token::Less => {
                            i += 2;
                            let target = expect_word(tokens, &mut i)?;
                            redirects.push(Redirect { fd, kind: RedirectKind::In, target });
                            true
                        }
                        Token::Great => {
                            i += 2;
                            let target = expect_word(tokens, &mut i)?;
                            redirects.push(Redirect { fd, kind: RedirectKind::Out, target });
                            true
                        }
                        Token::DoubleGreat => {
                            i += 2;
                            let target = expect_word(tokens, &mut i)?;
                            redirects.push(Redirect { fd, kind: RedirectKind::Append, target });
                            true
                        }
                        Token::LessGreat => {
                            i += 2;
                            let target = expect_word(tokens, &mut i)?;
                            redirects.push(Redirect { fd, kind: RedirectKind::In, target: target.clone() });
                            redirects.push(Redirect { fd, kind: RedirectKind::Out, target });
                            true
                        }
                        Token::GreatAnd => {
                            i += 2;
                            let target = expect_word(tokens, &mut i)?;
                            if target == "-" {
                                redirects.push(Redirect { fd, kind: RedirectKind::Close, target: String::new() });
                            } else {
                                redirects.push(Redirect { fd, kind: RedirectKind::DupOut, target });
                            }
                            true
                        }
                        Token::LessAnd => {
                            i += 2;
                            let target = expect_word(tokens, &mut i)?;
                            if target == "-" {
                                redirects.push(Redirect { fd, kind: RedirectKind::Close, target: String::new() });
                            } else {
                                redirects.push(Redirect { fd, kind: RedirectKind::DupIn, target });
                            }
                            true
                        }
                        _ => false,
                    };
                    if handled {
                        continue;
                    }
                }
                args.push(w.clone());
                i += 1;
            }
            Token::Less => {
                i += 1;
                let target = expect_word(tokens, &mut i)?;
                redirects.push(Redirect { fd: 0, kind: RedirectKind::In, target });
            }
            Token::Great => {
                i += 1;
                let target = expect_word(tokens, &mut i)?;
                redirects.push(Redirect { fd: 1, kind: RedirectKind::Out, target });
            }
            Token::DoubleGreat => {
                i += 1;
                let target = expect_word(tokens, &mut i)?;
                redirects.push(Redirect { fd: 1, kind: RedirectKind::Append, target });
            }
            Token::LessGreat => {
                i += 1;
                let target = expect_word(tokens, &mut i)?;
                redirects.push(Redirect { fd: 0, kind: RedirectKind::In, target: target.clone() });
                redirects.push(Redirect { fd: 1, kind: RedirectKind::Out, target });
            }
            Token::GreatAnd => {
                i += 1;
                let target = expect_word(tokens, &mut i)?;
                if target == "-" {
                    redirects.push(Redirect { fd: 1, kind: RedirectKind::Close, target: String::new() });
                } else {
                    redirects.push(Redirect { fd: 1, kind: RedirectKind::DupOut, target });
                }
            }
            Token::LessAnd => {
                i += 1;
                let target = expect_word(tokens, &mut i)?;
                if target == "-" {
                    redirects.push(Redirect { fd: 0, kind: RedirectKind::Close, target: String::new() });
                } else {
                    redirects.push(Redirect { fd: 0, kind: RedirectKind::DupIn, target });
                }
            }
            Token::BothGreat => {
                i += 1;
                let target = expect_word(tokens, &mut i)?;
                redirects.push(Redirect { fd: 1, kind: RedirectKind::Out, target: target.clone() });
                redirects.push(Redirect { fd: 2, kind: RedirectKind::Out, target });
            }
            Token::BothDoubleGreat => {
                i += 1;
                let target = expect_word(tokens, &mut i)?;
                redirects.push(Redirect { fd: 1, kind: RedirectKind::Append, target: target.clone() });
                redirects.push(Redirect { fd: 2, kind: RedirectKind::Append, target });
            }
            Token::Pipe | Token::And | Token::Or | Token::Background | Token::Semicolon => {
                break;
            }
        }
    }

    if args.is_empty() && redirects.is_empty() {
        return Err("expected command".into());
    }

    Ok((Simple { args, redirects }, i - start))
}

fn expect_word(tokens: &[Token], i: &mut usize) -> Result<String, String> {
    if *i < tokens.len() {
        match &tokens[*i] {
            Token::Word(w) => {
                let w = w.clone();
                *i += 1;
                return Ok(w);
            }
            _ => {}
        }
    }
    Err(format!("expected filename after redirection operator"))
}

// =============================================================================
// Builtins
// =============================================================================

fn is_builtin(cmd: &str, shell: &Shell) -> bool {
    if cmd == "shit" && !shell.shit {
        return false;
    }
    matches!(cmd, "exit" | "cd" | "pwd" | "echo" | "type" | "export" | "true" | "false" | "rm" | "source" | "." | "alias" | "unalias" | "add_path" | "path" | "shit" | "bshctl")
}

fn exec_builtin(simple: &Simple, shell: &mut Shell) -> i32 {
    let cmd = &simple.args[0];
    match cmd.as_str() {
        "exit" => {
            let code = simple.args.get(1).and_then(|s| s.parse::<i32>().ok()).unwrap_or(shell.last_exit);
            process::exit(code);
        }
        "cd" => {
            let target = simple.args.get(1).map(|s| s.as_str()).unwrap_or("~");
            if target == "-" {
                match env::var("OLDPWD") {
                    Ok(old) => {
                        let old_cwd = env::current_dir().unwrap_or_default();
                        env::set_current_dir(&old).ok();
                        unsafe { env::set_var("OLDPWD", old_cwd.to_string_lossy().as_ref()); }
                        let cwd = env::current_dir().unwrap_or_default();
                        println!("{}", cwd.display());
                        0
                    }
                    Err(_) => { eprintln!("bsh: cd: OLDPWD not set"); 1 }
                }
            } else {
                expand_tilde_cd(target, shell)
            }
        }
        "pwd" => {
            match env::current_dir() {
                Ok(dir) => { println!("{}", dir.display()); 0 }
                Err(e) => { eprintln!("bsh: pwd: {e}"); 1 }
            }
        }
        "echo" => {
            let msg = simple.args[1..].join(" ");
            println!("{msg}");
            0
        }
        "type" => {
            let mut code = 0;
            for name in &simple.args[1..] {
                if is_builtin(name, shell) {
                    println!("{name} is a shell builtin");
                } else if let Some(path) = find_in_path(name) {
                    println!("{name} is {}", path.display());
                } else {
                    eprintln!("bsh: type: {name}: not found");
                    code = 1;
                }
            }
            code
        }
        "export" => {
            for arg in &simple.args[1..] {
                if let Some((name, val)) = arg.split_once('=') {
                    unsafe { env::set_var(name, val); }
                }
            }
            0
        }
        "source" | "." => exec_builtin_source(simple, shell),
        "shit" => exec_builtin_shit(simple, shell),
        "bshctl" => exec_builtin_bshctl(simple, shell),
        "rm" => exec_builtin_rm(simple, shell),
        "alias" => exec_builtin_alias(simple, shell),
        "unalias" => exec_builtin_unalias(simple, shell),
        "add_path" | "path" => {
            for arg in &simple.args[1..] {
                let dir = if arg.starts_with('~') {
                    let home = env::var("HOME").unwrap_or_default();
                    format!("{}{}", home, &arg[1..])
                } else {
                    arg.clone()
                };
                let dir_path = std::path::Path::new(&dir);
                let current = env::var("PATH").unwrap_or_default();
                let already = env::split_paths(&current).any(|p| p == *dir_path);
                if !already {
                    let sep = if current.is_empty() { "" } else { ":" };
                    let new_path = format!("{current}{sep}{dir}");
                    unsafe { env::set_var("PATH", &new_path); }
                }
                persist_path_to_config(shell, arg);
            }
            0
        }
        "true" => 0,
        "false" => 1,
        _ => 1,
    }
}

fn persist_alias(shell: &Shell, name: &str, value: &str) {
    let path = match shell.config_path {
        Some(ref p) => p.clone(),
        None => return,
    };
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            String::new()
        }
    };
    let needle = format!("alias {}=", name);
    let replaced = content.lines()
        .map(|l| {
            let trimmed = l.trim();
            if trimmed.starts_with(&needle) {
                format!("alias {}={}", name, shell_quote(value))
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>();
    let has_existing = content.lines().any(|l| l.trim().starts_with(&needle));
    let mut out = if has_existing {
        replaced.join("\n")
    } else {
        if content.ends_with('\n') {
            format!("{}alias {}={}\n", content, name, shell_quote(value))
        } else if content.is_empty() {
            format!("alias {}={}\n", name, shell_quote(value))
        } else {
            format!("{}\nalias {}={}\n", content, name, shell_quote(value))
        }
    };
    // Ensure trailing newline
    if !out.ends_with('\n') {
        out.push('\n');
    }
    std::fs::write(&path, &out).ok();
}

fn persist_path_to_config(shell: &Shell, dir: &str) {
    let path = match shell.config_path {
        Some(ref p) => p.clone(),
        None => return,
    };
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    let needle = format!("path {dir}");
    let already = content.lines().any(|l| l.trim() == needle);
    if already {
        return;
    }
    let mut out = content;
    if !out.ends_with('\n') && !out.is_empty() {
        out.push('\n');
    }
    out.push_str(&format!("path {dir}\n"));
    std::fs::write(&path, &out).ok();
}

fn persist_bool_to_config(shell: &Shell, key: &str, enabled: bool) {
    let path = match shell.config_path {
        Some(ref p) => p.clone(),
        None => return,
    };
    let line = format!("{key} {}", if enabled { "on" } else { "off" });
    let content = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        String::new()
    });
    let replaced: Vec<String> = content.lines()
        .map(|l| {
            let trimmed = l.trim();
            if trimmed == format!("{key} on") || trimmed == format!("{key} off") {
                line.clone()
            } else {
                l.to_string()
            }
        })
        .collect();
    let has_existing = content.lines().any(|l| {
        let t = l.trim();
        t == format!("{key} on") || t == format!("{key} off")
    });
    let mut out = if has_existing {
        replaced.join("\n")
    } else if content.is_empty() {
        format!("{line}\n")
    } else if content.ends_with('\n') {
        format!("{content}{line}\n")
    } else {
        format!("{content}\n{line}\n")
    };
    if !out.ends_with('\n') {
        out.push('\n');
    }
    std::fs::write(&path, &out).ok();
}

fn persist_logging_to_config(shell: &Shell, enabled: bool) {
    persist_bool_to_config(shell, "log", enabled);
}

fn persist_shit_to_config(shell: &Shell, enabled: bool) {
    persist_bool_to_config(shell, "shit", enabled);
}

fn persist_history_to_config(shell: &Shell, enabled: bool) {
    persist_bool_to_config(shell, "history", enabled);
}

fn persist_autosuggest_to_config(shell: &Shell, enabled: bool) {
    persist_bool_to_config(shell, "auto-suggestion", enabled);
}

fn remove_alias_from_config(shell: &Shell, name: &str) {
    let path = match shell.config_path {
        Some(ref p) => p.clone(),
        None => return,
    };
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return,
    };
    let needle = format!("alias {}=", name);
    let out: String = content.lines()
        .filter(|l| l.trim() != &needle && !l.trim().starts_with(&needle))
        .map(|l| l.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&path, &out).ok();
}

fn exec_builtin_alias(simple: &Simple, shell: &mut Shell) -> i32 {
    if simple.args.len() == 1 {
        let mut aliases: Vec<_> = shell.aliases.iter().collect();
        aliases.sort_by(|a, b| a.0.cmp(b.0));
        for (name, value) in &aliases {
            println!("alias {}={}", name, shell_quote(value));
        }
        return 0;
    }
    for arg in &simple.args[1..] {
        if let Some((name, value)) = arg.split_once('=') {
            shell.aliases.insert(name.to_string(), value.to_string());
            persist_alias(shell, name, value);
        } else {
            match shell.aliases.get(arg) {
                Some(val) => println!("alias {}={}", arg, shell_quote(val)),
                None => eprintln!("bsh: alias: {arg}: not found"),
            }
        }
    }
    0
}

fn exec_builtin_unalias(simple: &Simple, shell: &mut Shell) -> i32 {
    let mut code = 0;
    for arg in &simple.args[1..] {
        if arg == "-a" {
            shell.aliases.clear();
            if let Some(ref path) = shell.config_path {
                let content = std::fs::read_to_string(path).unwrap_or_default();
                let out: String = content.lines()
                    .filter(|l| !l.trim().starts_with("alias "))
                    .map(|l| l.to_string())
                    .collect::<Vec<_>>()
                    .join("\n");
                std::fs::write(path, &out).ok();
            }
            return 0;
        }
        if shell.aliases.remove(arg).is_some() {
            remove_alias_from_config(shell, arg);
        } else {
            eprintln!("bsh: unalias: {arg}: not found");
            code = 1;
        }
    }
    code
}

fn shell_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    let needs_quoting = s.contains(' ')
        || s.contains('\t')
        || s.contains('\'')
        || s.contains('"')
        || s.contains('\\')
        || s.contains('$')
        || s.contains('`')
        || s.contains('!')
        || s.contains('#')
        || s.contains('&')
        || s.contains('|')
        || s.contains(';')
        || s.contains('<')
        || s.contains('>')
        || s.contains('(')
        || s.contains(')')
        || s.contains('{')
        || s.contains('}');
    if !needs_quoting {
        return s.to_string();
    }
    if !s.contains('\'') {
        return format!("'{}'", s);
    }
    let mut out = String::new();
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

fn exec_builtin_rm(simple: &Simple, _shell: &Shell) -> i32 {
    let mut has_f = false;
    let mut has_r = false;
    let mut files = Vec::new();
    let mut i = 1;
    while i < simple.args.len() {
        let a = &simple.args[i];
        if a == "--" {
            i += 1;
            while i < simple.args.len() {
                files.push(simple.args[i].clone());
                i += 1;
            }
            break;
        }
        if a.starts_with('-') && a.len() > 1 {
            for ch in a[1..].chars() {
                match ch {
                    'f' => has_f = true,
                    'r' | 'R' => has_r = true,
                    _ => {}
                }
            }
            i += 1;
            continue;
        }
        files.push(a.clone());
        i += 1;
    }

    if files.is_empty() {
        eprintln!("bsh: rm: missing operand");
        return 1;
    }

    let interactive = unsafe { libc::isatty(libc::STDIN_FILENO) != 0 };

    if has_f {
        for file in &files {
            delete_path(file, has_r);
        }
        return 0;
    }

    'file: for file in &files {
        if !Path::new(file).exists() {
            eprintln!("bsh: rm: {file}: No such file");
            continue;
        }
        if interactive {
            let suffix = if Path::new(file).is_dir() { "directory" } else { "regular file" };
            print!("bsh: rm: remove {suffix} '{file}'? [y/n] ");
            io::stdout().flush().ok();
            loop {
                let mut buf = [0u8; 1];
                let n = unsafe { libc::read(libc::STDIN_FILENO, buf.as_mut_ptr() as *mut libc::c_void, 1) };
                if n != 1 { break; }
                match buf[0] {
                    b'y' | b'Y' | b'\r' | b'\n' => { eprintln!(); break; }
                    _ => { eprintln!("  skipped"); continue 'file; }
                }
            }
        }
        delete_path(file, has_r);
    }
    0
}

fn delete_path(path: &str, recursive: bool) {
    let p = Path::new(path);
    if p.is_dir() {
        if recursive {
            std::fs::remove_dir_all(path).ok();
        } else {
            eprintln!("bsh: rm: {path}: is a directory");
        }
    } else {
        std::fs::remove_file(path).ok();
    }
}

fn exec_builtin_source(simple: &Simple, shell: &mut Shell) -> i32 {
    let file = match simple.args.get(1) {
        Some(f) if f == "-d" || f == "--default" => {
            let home = match env::var("HOME") {
                Ok(h) => h,
                Err(_) => { eprintln!("bsh: source: HOME not set"); return 1; }
            };
            Path::new(&home).join(".config").join("bsh").join("config")
        }
        Some(f) => Path::new(f).to_path_buf(),
        None => {
            eprintln!("bsh: source: missing filename");
            return 1;
        }
    };
    let contents = match std::fs::read_to_string(&file) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("bsh: source: {}: {e}", file.display());
            return 1;
        }
    };
    let mut last_code = 0;
    let mut in_block = 0i32;
    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if in_block > 0 {
            if trimmed == "}" {
                in_block -= 1;
            }
            continue;
        }
        if trimmed.ends_with('{') {
            in_block += 1;
            continue;
        }
        let code = exec_line(line, shell);
        last_code = code;
    }
    last_code
}

fn exec_builtin_with_redirects(simple: &Simple, shell: &mut Shell) -> i32 {
    let mut expanded = Simple {
        args: Vec::new(),
        redirects: simple.redirects.clone(),
    };
    for arg in &simple.args {
        expanded.args.extend(expand_word(arg, shell));
    }

    if simple.redirects.is_empty() {
        return exec_builtin(&expanded, shell);
    }

    let saved = (
        save_fd(0),
        save_fd(1),
        save_fd(2),
    );

    for redir in &simple.redirects {
        let target = expand_redirect_target(&redir.target, shell);
        match redir.kind {
            RedirectKind::In => {
                let fd = match File::open(&target) {
                    Ok(f) => f.into_raw_fd(),
                    Err(e) => { eprintln!("bsh: {target}: {e}"); return 1; }
                };
                unsafe { libc::dup2(fd, redir.fd); libc::close(fd); }
            }
            RedirectKind::Out => {
                let fd = match File::create(&target) {
                    Ok(f) => f.into_raw_fd(),
                    Err(e) => { eprintln!("bsh: {target}: {e}"); return 1; }
                };
                unsafe { libc::dup2(fd, redir.fd); libc::close(fd); }
            }
            RedirectKind::Append => {
                let fd = match OpenOptions::new().append(true).create(true).open(&target) {
                    Ok(f) => f.into_raw_fd(),
                    Err(e) => { eprintln!("bsh: {target}: {e}"); return 1; }
                };
                unsafe { libc::dup2(fd, redir.fd); libc::close(fd); }
            }
            RedirectKind::DupOut | RedirectKind::DupIn => {
                let tfd: i32 = target.parse().unwrap_or(-1);
                if tfd < 0 {
                    eprintln!("bsh: {}: invalid file descriptor", target);
                    return 1;
                }
                unsafe { libc::dup2(tfd, redir.fd); }
            }
            RedirectKind::Close => {
                unsafe { libc::close(redir.fd); }
            }
        }
    }

    let code = exec_builtin(&expanded, shell);

    restore_fd(saved.0, 0);
    restore_fd(saved.1, 1);
    restore_fd(saved.2, 2);

    code
}

fn save_fd(fd: i32) -> i32 {
    unsafe { libc::dup(fd) }
}

fn restore_fd(saved: i32, target: i32) {
    if saved >= 0 {
        unsafe {
            libc::dup2(saved, target);
            libc::close(saved);
        }
    }
}

fn expand_tilde_cd(target: &str, _shell: &mut Shell) -> i32 {
    let resolved = if target == "~" {
        env::var("HOME").unwrap_or_else(|_| ".".into())
    } else if target.starts_with("~/") {
        let home = env::var("HOME").unwrap_or_else(|_| ".".into());
        format!("{}/{}", home, &target[2..])
    } else {
        target.to_string()
    };
    let old_cwd = env::current_dir().unwrap_or_default();
    match env::set_current_dir(&resolved) {
        Ok(()) => {
            unsafe { env::set_var("OLDPWD", old_cwd.to_string_lossy().as_ref()); }
            0
        }
        Err(e) => { eprintln!("bsh: cd: {target}: {e}"); 1 }
    }
}

// =============================================================================
// Shit command - thefuck-style command correction
// =============================================================================

fn levenshtein(a: &str, b: &str) -> usize {
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    let a_len = a_chars.len();
    let b_len = b_chars.len();
    if a_len == 0 { return b_len; }
    if b_len == 0 { return a_len; }
    let mut prev: Vec<usize> = (0..=b_len).collect();
    let mut curr = vec![0usize; b_len + 1];
    for i in 1..=a_len {
        curr[0] = i;
        for j in 1..=b_len {
            let cost = if a_chars[i - 1] == b_chars[j - 1] { 0 } else { 1 };
            curr[j] = (curr[j - 1] + 1)
                .min(prev[j] + 1)
                .min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b_len]
}

fn get_close_matches(word: &str, possibilities: &[String], cutoff: f64) -> Vec<String> {
    let mut scored: Vec<(f64, &String)> = possibilities.iter()
        .map(|p| {
            let max_len = word.len().max(p.len());
            let dist = levenshtein(word, p);
            let score = if max_len == 0 { 1.0 } else { 1.0 - dist as f64 / max_len as f64 };
            (score, p)
        })
        .filter(|(score, _)| *score >= cutoff)
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored.into_iter().take(3).map(|(_, p)| p.clone()).collect()
}

struct ShitCommand {
    script: String,
    output: String,
}

fn run_and_capture_output(script: &str) -> Option<String> {
    let script = script.to_string();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let child = Command::new("sh")
            .arg("-c")
            .arg(&script)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();
        if let Ok(child) = child {
            if let Ok(output) = child.wait_with_output() {
                let mut out = String::new();
                if !output.stdout.is_empty() {
                    out.push_str(&String::from_utf8_lossy(&output.stdout));
                }
                if !output.stderr.is_empty() {
                    out.push_str(&String::from_utf8_lossy(&output.stderr));
                }
                let _ = tx.send(out);
            }
        }
    });
    match rx.recv_timeout(std::time::Duration::from_secs(5)) {
        Ok(out) => Some(out),
        Err(_) => None,
    }
}

fn shit_rule_fix_alt_space(cmd: &ShitCommand, _shell: &Shell) -> Option<String> {
    if cmd.script.contains('\u{a0}') {
        Some(cmd.script.replace('\u{a0}', " "))
    } else {
        None
    }
}

fn shit_rule_sudo(cmd: &ShitCommand, _shell: &Shell) -> Option<String> {
    let lower = cmd.output.to_lowercase();
    let patterns = ["permission denied", "eacces", "operation not permitted",
        "not super-user", "must be root", "must run as root",
        "only root can", "authentication is required",
        "not superuser", "need to be root", "must be superuser",
        "operation not permitted", "edspermissionerror",
        "error: insufficient privileges"];
    for p in &patterns {
        if lower.contains(p) {
            if !cmd.script.starts_with("sudo ") {
                return Some(format!("sudo {}", cmd.script));
            }
            break;
        }
    }
    None
}

fn shit_rule_cd_mkdir(cmd: &ShitCommand, _shell: &Shell) -> Option<String> {
    if !cmd.script.starts_with("cd ") {
        return None;
    }
    let lower = cmd.output.to_lowercase();
    if lower.contains("no such file or directory") || lower.contains("can't cd to") || lower.contains("does not exist") {
        let dir = cmd.script[3..].trim();
        if !dir.is_empty() {
            return Some(format!("mkdir -p {} && cd {}", dir, dir));
        }
    }
    None
}

fn shit_rule_command_not_found(cmd: &ShitCommand, _shell: &Shell) -> Option<String> {
    if !cmd.output.contains("command not found") {
        return None;
    }
    let parts: Vec<&str> = cmd.script.split_whitespace().collect();
    if parts.is_empty() {
        return None;
    }
    let bad_cmd = parts[0];
    let commands = get_commands();
    let closest = get_close_matches(bad_cmd, &commands, 0.5);
    closest.into_iter().next().map(|c| cmd.script.replacen(bad_cmd, &c, 1))
}

fn shit_rule_cat_dir(cmd: &ShitCommand, _shell: &Shell) -> Option<String> {
    if !cmd.script.starts_with("cat ") {
        return None;
    }
    let lower = cmd.output.to_lowercase();
    if lower.contains("cat:") && lower.contains("is a directory") {
        return Some(cmd.script.replacen("cat", "ls", 1));
    }
    None
}

fn shit_rule_rm_dir(cmd: &ShitCommand, _shell: &Shell) -> Option<String> {
    if !cmd.script.starts_with("rm ") {
        return None;
    }
    let lower = cmd.output.to_lowercase();
    if (lower.contains("is a directory") || lower.contains("cannot remove"))
        && !cmd.script.contains(" -r")
    {
        let rest = &cmd.script[3..];
        return Some(format!("rm -rf {}", rest));
    }
    None
}

fn shit_rule_cp_omitting_directory(cmd: &ShitCommand, _shell: &Shell) -> Option<String> {
    if !cmd.script.starts_with("cp ") {
        return None;
    }
    let lower = cmd.output.to_lowercase();
    if (lower.contains("omitting directory") || lower.contains("is a directory"))
        && !cmd.script.contains(" -r")
    {
        let rest = &cmd.script[3..];
        return Some(format!("cp -r {}", rest));
    }
    None
}

fn shit_rule_git_not_command(cmd: &ShitCommand, _shell: &Shell) -> Option<String> {
    if !cmd.output.contains("is not a git command")
        || (!cmd.output.contains("The most similar command") && !cmd.output.contains("Did you mean"))
    {
        return None;
    }
    let prefix = "git: '";
    let bad_start = cmd.output.find(prefix)?;
    let after = &cmd.output[bad_start + prefix.len()..];
    let bad_end = after.find('\'')?;
    let bad_cmd = &after[..bad_end];

    let mut suggestions: Vec<String> = Vec::new();
    let mut capture = false;
    for line in cmd.output.lines() {
        if line.contains("The most similar command") || line.contains("Did you mean") {
            capture = true;
            continue;
        }
        if capture {
            let trimmed = line.trim();
            if !trimmed.is_empty() {
                suggestions.push(trimmed.to_string());
            }
            capture = false;
        }
    }
    if suggestions.is_empty() {
        return None;
    }
    let close = get_close_matches(bad_cmd, &suggestions, 0.1);
    close.into_iter().next().map(|s| cmd.script.replacen(bad_cmd, s.trim(), 1))
}

fn shit_rule_common_typos(cmd: &ShitCommand, _shell: &Shell) -> Option<String> {
    let first_word = cmd.script.split_whitespace().next()?;
    let correction = match first_word {
        "sl" => "ls",
        "dc" => "cd",
        "mv" => "mv", // not a typo
        "grep" if cmd.script.starts_with("grep -") => return None,
        "pyhton" | "pythno" | "pythn" | "pytohn" => "python",
        "pythno3" | "pyhton3" => "python3",
        "gip" | "igt" | "gi" => "git",
        "k8s" => "kubectl",
        "dockr" | "dokcer" => "docker",
        "dockr-compose" | "dokcer-compose" => "docker-compose",
        "node" if first_word == "node" => return None,
        "npm" if first_word == "npm" => return None,
        _ => return None,
    };
    if correction != first_word {
        Some(cmd.script.replacen(first_word, correction, 1))
    } else {
        None
    }
}

fn exec_builtin_shit(_simple: &Simple, shell: &mut Shell) -> i32 {
    if shell.prev_cmd.is_empty() {
        eprintln!("bsh: shit: no previous command");
        return 1;
    }
    let last = &shell.prev_cmd;

    let output = run_and_capture_output(last).unwrap_or_default();

    let cmd = ShitCommand {
        script: last.clone(),
        output,
    };

    let rules: [fn(&ShitCommand, &Shell) -> Option<String>; 9] = [
        shit_rule_fix_alt_space,
        shit_rule_common_typos,
        shit_rule_sudo,
        shit_rule_cd_mkdir,
        shit_rule_command_not_found,
        shit_rule_cat_dir,
        shit_rule_rm_dir,
        shit_rule_cp_omitting_directory,
        shit_rule_git_not_command,
    ];

    let mut correction = None;
    for rule in &rules {
        if let Some(fix) = rule(&cmd, shell) {
            if fix != *last {
                correction = Some(fix);
                break;
            }
        }
    }

    let fix = match correction {
        Some(f) => f,
        None => {
            eprintln!("bsh: shit: no correction found for: {}", last);
            return 1;
        }
    };

    eprint!("bsh: shit: {} ? [y/n] ", fix);
    io::stderr().flush().ok();

    loop {
        let mut buf = [0u8; 1];
        let n = unsafe { libc::read(libc::STDIN_FILENO, buf.as_mut_ptr() as *mut libc::c_void, 1) };
        if n != 1 { break; }
        match buf[0] {
            b'y' | b'Y' | b'\r' | b'\n' => { eprintln!(); break; }
            b'n' | b'N' => return 0,
            _ => {}
        }
    }

    let code = exec_line(&fix, shell);
    if code != 127 && shell.history && !shell.fresh {
        if let Ok(mut h) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(history_file())
        {
            use std::io::Write;
            writeln!(&mut h, "{}", fix).ok();
        }
    }
    code
}

// =============================================================================
// bshctl - shell control suite
// =============================================================================

fn exec_builtin_bshctl(simple: &Simple, shell: &mut Shell) -> i32 {
    if simple.args.len() == 1 {
        println!("  @@@@@@@@@@@@@@@@@@");
        println!(" @                  @@@@");
        println!(" @    $bsh{}         @", env!("CARGO_PKG_VERSION"));
        println!(" @                       @@");
        println!(" @                      @");
        println!("  @@@@@@@@@@@@@@@@@@@@@@");
        0
    } else {
        match simple.args[1].as_str() {
            "enable" => exec_bshctl_enable(&simple.args[2..], shell),
            "disable" => exec_bshctl_disable(&simple.args[2..], shell),
            "status" => exec_bshctl_status(&simple.args[2..], shell),
            "logging" => exec_bshctl_logging(&simple.args[2..], shell),
            "shit" => exec_bshctl_shit(&simple.args[2..], shell),
            "history" => exec_bshctl_history(&simple.args[2..], shell),
            "auto-suggestion" => exec_bshctl_autosuggest(&simple.args[2..], shell),
            "--fresh" => exec_bshctl_shell(&simple.args[2..], shell),
            sub => {
                eprintln!("bsh: bshctl: unknown subcommand: {sub}");
                1
            }
        }
    }
}

fn exec_bshctl_enable(args: &[String], shell: &mut Shell) -> i32 {
    match args {
        [sub] if sub == "logging" => {
            shell.logging = true;
            persist_logging_to_config(shell, true);
            println!("logging enabled");
            0
        }
        [sub] if sub == "shit" => {
            shell.shit = true;
            persist_shit_to_config(shell, true);
            println!("shit enabled");
            0
        }
        [sub] if sub == "history" => {
            shell.history = true;
            persist_history_to_config(shell, true);
            println!("history enabled");
            0
        }
        [sub] if sub == "auto-suggestion" => {
            shell.autosuggest = true;
            persist_autosuggest_to_config(shell, true);
            println!("auto-suggestion enabled");
            0
        }
        _ => {
            eprintln!("usage: bshctl enable logging|shit|history|auto-suggestion");
            1
        }
    }
}

fn exec_bshctl_disable(args: &[String], shell: &mut Shell) -> i32 {
    match args {
        [sub] if sub == "logging" => {
            shell.logging = false;
            persist_logging_to_config(shell, false);
            println!("logging disabled");
            0
        }
        [sub] if sub == "shit" => {
            shell.shit = false;
            persist_shit_to_config(shell, false);
            println!("shit disabled");
            0
        }
        [sub] if sub == "history" => {
            shell.history = false;
            persist_history_to_config(shell, false);
            println!("history disabled");
            0
        }
        [sub] if sub == "auto-suggestion" => {
            shell.autosuggest = false;
            persist_autosuggest_to_config(shell, false);
            println!("auto-suggestion disabled");
            0
        }
        _ => {
            eprintln!("usage: bshctl disable logging|shit|history|auto-suggestion");
            1
        }
    }
}

fn exec_bshctl_status(args: &[String], shell: &Shell) -> i32 {
    match args {
        [sub] if sub == "logging" => {
            let state = if shell.logging { "enabled" } else { "disabled (default)" };
            println!("logging: {state}");
            println!("log file: {DEBUG_LOG}");
            0
        }
        [sub] if sub == "shit" => {
            let state = if shell.shit { "enabled (default)" } else { "disabled" };
            println!("shit: {state}");
            0
        }
        [sub] if sub == "history" => {
            let state = if shell.history { "enabled (default)" } else { "disabled" };
            println!("history: {state}");
            0
        }
        [sub] if sub == "auto-suggestion" => {
            let state = if shell.autosuggest { "enabled (default)" } else { "disabled" };
            println!("auto-suggestion: {state}");
            0
        }
        _ => {
            eprintln!("usage: bshctl status logging|shit|history|auto-suggestion");
            1
        }
    }
}

fn exec_bshctl_logging(_args: &[String], shell: &mut Shell) -> i32 {
    if unsafe { libc::isatty(libc::STDIN_FILENO) == 0 } {
        eprintln!("bsh: bshctl: logging: interactive-only command");
        return 1;
    }
    println!("type q or exit to exit");
    let mut history = History { entries: Vec::new(), index: 0, saved: String::new(), path: None };
    loop {
        match read_line_interactive("   ? ", &mut history, shell.autosuggest) {
            ReadLineResult::Line(line) => {
                let cmd = line.trim();
                if cmd.is_empty() {
                    continue;
                }
                match cmd {
                    "help" => bshctl_logging_help(),
                    "enable" => {
                        shell.logging = true;
                        persist_logging_to_config(shell, true);
                        println!("    ! enabled");
                    }
                    "disable" => {
                        shell.logging = false;
                        persist_logging_to_config(shell, false);
                        println!("    ! disabled");
                    }
                    "status" => {
                        println!("    ! {}", if shell.logging { "on" } else { "off" });
                    }
                    "clear-file" => bshctl_logging_clear(),
                    "q" | "exit" | "quit" => break,
                    other => println!("    ! unknown command: {other}"),
                }
            }
            ReadLineResult::CtrlC | ReadLineResult::Eof => {
                println!();
                break;
            }
        }
    }
    0
}

fn bshctl_logging_help() {
    println!("    ! logging commands:");
    println!("    !   help         show this help");
    println!("    !   enable       enable logging");
    println!("    !   disable      disable logging");
    println!("    !   status       show if on or off");
    println!("    !   clear-file   delete {DEBUG_LOG}");
    println!("    !   q / exit     leave this shell");
}

fn bshctl_logging_clear() {
    print!("    ! delete {DEBUG_LOG}? [y/n] ");
    io::stdout().flush().ok();
    loop {
        let mut buf = [0u8; 1];
        let n = unsafe { libc::read(libc::STDIN_FILENO, buf.as_mut_ptr() as *mut libc::c_void, 1) };
        if n != 1 {
            break;
        }
        match buf[0] {
            b'y' | b'Y' | b'\r' | b'\n' => {
                std::fs::remove_file(DEBUG_LOG).ok();
                println!();
                println!("    ! cleared");
                break;
            }
            b'n' | b'N' => {
                println!();
                println!("    ! cancelled");
                break;
            }
            _ => {}
        }
    }
}

fn exec_bshctl_shit(_args: &[String], shell: &mut Shell) -> i32 {
    if unsafe { libc::isatty(libc::STDIN_FILENO) == 0 } {
        eprintln!("bsh: bshctl: shit: interactive-only command");
        return 1;
    }
    println!("type q or exit to exit");
    let mut history = History { entries: Vec::new(), index: 0, saved: String::new(), path: None };
    loop {
        match read_line_interactive("   ? ", &mut history, shell.autosuggest) {
            ReadLineResult::Line(line) => {
                let cmd = line.trim();
                if cmd.is_empty() {
                    continue;
                }
                match cmd {
                    "help" => bshctl_shit_help(),
                    "enable" => {
                        shell.shit = true;
                        persist_shit_to_config(shell, true);
                        println!("    ! enabled");
                    }
                    "disable" => {
                        shell.shit = false;
                        persist_shit_to_config(shell, false);
                        println!("    ! disabled");
                    }
                    "status" => {
                        println!("    ! {}", if shell.shit { "on" } else { "off" });
                    }
                    "q" | "exit" | "quit" => break,
                    other => println!("    ! unknown command: {other}"),
                }
            }
            ReadLineResult::CtrlC | ReadLineResult::Eof => {
                println!();
                break;
            }
        }
    }
    0
}

fn bshctl_shit_help() {
    println!("    ! shit commands:");
    println!("    !   help         show this help");
    println!("    !   enable       enable shit");
    println!("    !   disable      disable shit");
    println!("    !   status       show if on or off");
    println!("    !   q / exit     leave this shell");
}

fn exec_bshctl_history(_args: &[String], shell: &mut Shell) -> i32 {
    if unsafe { libc::isatty(libc::STDIN_FILENO) == 0 } {
        eprintln!("bsh: bshctl: history: interactive-only command");
        return 1;
    }
    println!("type q or exit to exit");
    let mut history = History { entries: Vec::new(), index: 0, saved: String::new(), path: None };
    loop {
        match read_line_interactive("   ? ", &mut history, shell.autosuggest) {
            ReadLineResult::Line(line) => {
                let cmd = line.trim();
                if cmd.is_empty() {
                    continue;
                }
                match cmd {
                    "help" => bshctl_history_help(),
                    "enable" => {
                        shell.history = true;
                        persist_history_to_config(shell, true);
                        println!("    ! enabled");
                    }
                    "disable" => {
                        shell.history = false;
                        persist_history_to_config(shell, false);
                        println!("    ! disabled");
                    }
                    "status" => {
                        println!("    ! {}", if shell.history { "on" } else { "off" });
                    }
                    "clear-file" => bshctl_history_clear(),
                    "q" | "exit" | "quit" => break,
                    other => println!("    ! unknown command: {other}"),
                }
            }
            ReadLineResult::CtrlC | ReadLineResult::Eof => {
                println!();
                break;
            }
        }
    }
    0
}

fn bshctl_history_help() {
    println!("    ! history commands:");
    println!("    !   help         show this help");
    println!("    !   enable       enable history");
    println!("    !   disable      disable history");
    println!("    !   status       show if on or off");
    println!("    !   clear-file   delete the history file");
    println!("    !   q / exit     leave this shell");
}

fn bshctl_history_clear() {
    let path = history_file();
    print!("    ! delete {}? [y/n] ", path.display());
    io::stdout().flush().ok();
    loop {
        let mut buf = [0u8; 1];
        let n = unsafe { libc::read(libc::STDIN_FILENO, buf.as_mut_ptr() as *mut libc::c_void, 1) };
        if n != 1 {
            break;
        }
        match buf[0] {
            b'y' | b'Y' | b'\r' | b'\n' => {
                std::fs::remove_file(&path).ok();
                println!();
                println!("    ! cleared");
                break;
            }
            b'n' | b'N' => {
                println!();
                println!("    ! cancelled");
                break;
            }
            _ => {}
        }
    }
}

fn exec_bshctl_autosuggest(_args: &[String], shell: &mut Shell) -> i32 {
    if unsafe { libc::isatty(libc::STDIN_FILENO) == 0 } {
        eprintln!("bsh: bshctl: auto-suggestion: interactive-only command");
        return 1;
    }
    println!("type q or exit to exit");
    let mut history = History { entries: Vec::new(), index: 0, saved: String::new(), path: None };
    loop {
        match read_line_interactive("   ? ", &mut history, shell.autosuggest) {
            ReadLineResult::Line(line) => {
                let cmd = line.trim();
                if cmd.is_empty() {
                    continue;
                }
                match cmd {
                    "help" => bshctl_autosuggest_help(),
                    "enable" => {
                        shell.autosuggest = true;
                        persist_autosuggest_to_config(shell, true);
                        println!("    ! enabled");
                    }
                    "disable" => {
                        shell.autosuggest = false;
                        persist_autosuggest_to_config(shell, false);
                        println!("    ! disabled");
                    }
                    "status" => {
                        println!("    ! {}", if shell.autosuggest { "on" } else { "off" });
                    }
                    "q" | "exit" | "quit" => break,
                    other => println!("    ! unknown command: {other}"),
                }
            }
            ReadLineResult::CtrlC | ReadLineResult::Eof => {
                println!();
                break;
            }
        }
    }
    0
}

fn exec_bshctl_shell(_args: &[String], shell: &Shell) -> i32 {
    if unsafe { libc::isatty(libc::STDIN_FILENO) == 0 } {
        eprintln!("bsh: bshctl: --fresh: interactive-only command");
        return 1;
    }
    let flag = "--fresh";
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("bsh: bshctl: {flag}: {e}");
            return 1;
        }
    };
    let mut cmd = Command::new(&exe);
    cmd.arg(flag);
    unsafe {
        cmd.pre_exec(|| {
            libc::signal(libc::SIGINT, libc::SIG_DFL);
            libc::signal(libc::SIGQUIT, libc::SIG_IGN);
            Ok(())
        });
    }
    prepare_job_terminal(shell);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("bsh: bshctl: {flag}: {e}");
            restore_shell_terminal(shell);
            return 1;
        }
    };
    let status = child.wait().unwrap_or_else(|_| process::ExitStatus::default());
    restore_shell_terminal(shell);
    exit_status_code(status)
}

fn bshctl_autosuggest_help() {
    println!("    ! auto-suggestion commands:");
    println!("    !   help         show this help");
    println!("    !   enable       enable auto-suggestion");
    println!("    !   disable      disable auto-suggestion");
    println!("    !   status       show if on or off");
    println!("    !   q / exit     leave this shell");
}

const DEBUG_LOG: &str = "/tmp/bsh_debug.log";

fn log_line(line: &str) {
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(DEBUG_LOG) {
        use std::io::Write;
        writeln!(f, "{line}").ok();
    }
}

fn log_command(shell: &Shell, line: &str) {
    if shell.logging {
        log_line(line);
    }
}

// =============================================================================
// Path resolution
// =============================================================================

fn find_in_path(name: &str) -> Option<std::path::PathBuf> {
    if name.contains('/') {
        return if Path::new(name).is_file() { Some(Path::new(name).to_path_buf()) } else { None };
    }
    let path = env::var("PATH").unwrap_or_default();
    for dir in path.split(':') {
        let full = Path::new(dir).join(name);
        if full.is_file() {
            return Some(full);
        }
    }
    None
}

// =============================================================================
// Execution
// =============================================================================

fn exec_line(line: &str, shell: &mut Shell) -> i32 {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return shell.last_exit;
    }
    let mut tok = Tokenizer::new(trimmed);
    let tokens = match tok.tokenize() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("bsh: parse error: {e}");
            return 2;
        }
    };
    if tokens.is_empty() {
        return shell.last_exit;
    }
    let nodes = match parse(&tokens) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("bsh: {e}");
            return 2;
        }
    };
    let mut last_code = shell.last_exit;
    for node in &nodes {
        let code = exec_node(node, shell);
        last_code = code;
    }
    last_code
}

fn exec_node(node: &Node, shell: &mut Shell) -> i32 {
    if sigint_pending() {
        return 130;
    }
    match node {
        Node::Simple(simple, background) => {
            if *background {
                exec_background(simple, shell)
            } else {
                exec_simple(simple, shell)
            }
        }
        Node::Pipeline(cmds, background) => {
            if *background {
                exec_pipeline_background(cmds, shell)
            } else {
                exec_pipeline(cmds, shell)
            }
        }
        Node::And(left, right) => {
            let code = exec_node(left, shell);
            shell.last_exit = code;
            if code == 0 {
                exec_node(right, shell)
            } else {
                code
            }
        }
        Node::Or(left, right) => {
            let code = exec_node(left, shell);
            shell.last_exit = code;
            if code != 0 {
                exec_node(right, shell)
            } else {
                code
            }
        }
    }
}

fn exec_simple(simple: &Simple, shell: &mut Shell) -> i32 {
    if simple.args.is_empty() && !simple.redirects.is_empty() {
        return exec_redirect_only(simple, shell);
    }

    let simple = expand_aliases(simple, shell);

    let cmd_name = &simple.args[0];
    if is_builtin(cmd_name, shell) {
        return exec_builtin_with_redirects(&simple, shell);
    }
    exec_external(&simple, shell)
}

fn expand_aliases<'a>(simple: &'a Simple, shell: &Shell) -> std::borrow::Cow<'a, Simple> {
    let mut visited = Vec::new();
    let mut current: std::borrow::Cow<'a, Simple> = std::borrow::Cow::Borrowed(simple);
    loop {
        let cmd_name = &current.args[0];
        if visited.contains(cmd_name) {
            break;
        }
        let alias_val = match shell.aliases.get(cmd_name) {
            Some(v) => v.clone(),
            None => break,
        };
        visited.push(cmd_name.clone());
        let new_args = split_alias_words(&alias_val);
        let mut args = Vec::with_capacity(new_args.len() + current.args.len() - 1);
        args.extend(new_args);
        args.extend(current.args[1..].iter().cloned());
        current = std::borrow::Cow::Owned(Simple {
            args,
            redirects: current.redirects.clone(),
        });
    }
    current
}

fn split_alias_words(s: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut chars = s.chars().peekable();
    loop {
        while let Some(&c) = chars.peek() {
            if c.is_ascii_whitespace() {
                chars.next();
            } else {
                break;
            }
        }
        if chars.peek().is_none() {
            break;
        }
        let word = match chars.peek() {
            Some('\'') => {
                chars.next();
                let mut w = String::new();
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => w.push(c),
                        None => break,
                    }
                }
                w
            }
            Some('"') => {
                chars.next();
                let mut w = String::new();
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => {
                            match chars.next() {
                                Some('$') => w.push('$'),
                                Some('`') => w.push('`'),
                                Some('"') => w.push('"'),
                                Some('\\') => w.push('\\'),
                                Some('\n') => {}
                                Some(c) => {
                                    w.push('\\');
                                    w.push(c);
                                }
                                None => w.push('\\'),
                            }
                        }
                        Some(c) => w.push(c),
                        None => break,
                    }
                }
                w
            }
            _ => {
                let mut w = String::new();
                while let Some(&c) = chars.peek() {
                    if c.is_ascii_whitespace() || c == '\'' || c == '"' {
                        break;
                    }
                    w.push(c);
                    chars.next();
                }
                w
            }
        };
        words.push(word);
    }
    words
}

fn exec_external(simple: &Simple, shell: &Shell) -> i32 {
    let program = match resolve_command(&simple.args[0], shell) {
        Ok(p) => p,
        Err(code) => return code,
    };

    let mut cmd = Command::new(&program);

    let expanded = expand_simple(simple, shell);
    if !expanded.args.is_empty() {
        cmd.args(&expanded.args[1..]);
    }

    unsafe {
        cmd.pre_exec(|| {
            libc::signal(libc::SIGINT, libc::SIG_DFL);
            libc::signal(libc::SIGQUIT, libc::SIG_IGN);
            Ok(())
        });
    }

    apply_redirects(&mut cmd, &simple.redirects, shell);

    prepare_job_terminal(shell);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("bsh: {}: {e}", simple.args[0]);
            restore_shell_terminal(shell);
            return 126;
        }
    };

    let status = child.wait().unwrap_or_else(|_| process::ExitStatus::default());
    restore_shell_terminal(shell);
    exit_status_code(status)
}

fn resolve_command(name: &str, shell: &Shell) -> Result<String, i32> {
    let expanded = expand_word(name, shell);
    let cmd_name = if expanded.is_empty() { name } else { &expanded[0] };
    if cmd_name.contains('/') {
        if Path::new(cmd_name).is_file() {
            return Ok(cmd_name.to_string());
        }
        eprintln!("bsh: {}: No such file", cmd_name);
        return Err(127);
    }
    match find_in_path(cmd_name) {
        Some(p) => Ok(p.to_string_lossy().to_string()),
        None => {
            eprintln!("bsh: {}: command not found", cmd_name);
            Err(127)
        }
    }
}

fn apply_redirects(cmd: &mut Command, redirects: &[Redirect], shell: &Shell) {
    for redir in redirects {
        let target = expand_redirect_target(&redir.target, shell);
        match redir.kind {
            RedirectKind::In => {
                match File::open(&target) {
                    Ok(f) => { cmd.stdin(f); }
                    Err(e) => { eprintln!("bsh: {target}: {e}"); return; }
                }
            }
            RedirectKind::Out => {
                match File::create(&target) {
                    Ok(f) => {
                        match redir.fd {
                            2 => { cmd.stderr(f); }
                            _ => { cmd.stdout(f); }
                        }
                    }
                    Err(e) => { eprintln!("bsh: {target}: {e}"); return; }
                }
            }
            RedirectKind::Append => {
                match OpenOptions::new().append(true).create(true).open(&target) {
                    Ok(f) => {
                        match redir.fd {
                            2 => { cmd.stderr(f); }
                            _ => { cmd.stdout(f); }
                        }
                    }
                    Err(e) => { eprintln!("bsh: {target}: {e}"); return; }
                }
            }
            RedirectKind::DupOut => {
                let tfd: i32 = target.parse().unwrap_or(-1);
                if tfd < 0 {
                    eprintln!("bsh: {}: invalid file descriptor", target);
                    return;
                }
                let my_fd = redir.fd;
                unsafe {
                    cmd.pre_exec(move || {
                        libc::dup2(tfd, my_fd);
                        Ok(())
                    });
                }
            }
            RedirectKind::DupIn => {
                let tfd: i32 = target.parse().unwrap_or(-1);
                if tfd < 0 {
                    eprintln!("bsh: {}: invalid file descriptor", target);
                    return;
                }
                let my_fd = redir.fd;
                unsafe {
                    cmd.pre_exec(move || {
                        libc::dup2(tfd, my_fd);
                        Ok(())
                    });
                }
            }
            RedirectKind::Close => {
                match redir.fd {
                    0 => { cmd.stdin(Stdio::null()); }
                    1 => { cmd.stdout(Stdio::null()); }
                    2 => { cmd.stderr(Stdio::null()); }
                    _ => {}
                }
            }
        }
    }
}

fn expand_simple(simple: &Simple, shell: &Shell) -> Simple {
    let mut args = Vec::new();
    for arg in &simple.args {
        args.extend(expand_word(arg, shell));
    }
    Simple {
        args,
        redirects: simple.redirects.clone(),
    }
}

fn exec_redirect_only(simple: &Simple, shell: &Shell) -> i32 {
    for redir in &simple.redirects {
        let target = expand_redirect_target(&redir.target, shell);
        match redir.kind {
            RedirectKind::In => {
                if File::open(&target).is_err() {
                    eprintln!("bsh: {target}: No such file");
                    return 1;
                }
            }
            RedirectKind::Out | RedirectKind::Append => {
                if File::create(&target).is_err() {
                    eprintln!("bsh: {target}: cannot create");
                    return 1;
                }
            }
            RedirectKind::DupOut | RedirectKind::DupIn | RedirectKind::Close => {}
        }
    }
    0
}

fn exec_pipeline(cmds: &[Simple], shell: &Shell) -> i32 {
    if cmds.is_empty() {
        return 0;
    }
    if cmds.len() == 1 {
        return exec_pipeline_simple(&cmds[0], shell);
    }

    let n = cmds.len();
    let mut pipes: Vec<(i32, i32)> = Vec::new();

    for _ in 0..n - 1 {
        let mut fds: [i32; 2] = [0, 0];
        unsafe {
            libc::pipe(fds.as_mut_ptr());
        }
        pipes.push((fds[0], fds[1]));
    }

    let mut children: Vec<Child> = Vec::new();

    prepare_job_terminal(shell);

    for (i, simple) in cmds.iter().enumerate() {
        let program = match resolve_command(&simple.args[0], shell) {
            Ok(p) => p,
            Err(_code) => {
                for &(rfd, wfd) in &pipes {
                    unsafe { libc::close(rfd); libc::close(wfd); }
                }
                restore_shell_terminal(shell);
                return 127;
            }
        };

        let mut cmd = Command::new(&program);
        let expanded = expand_simple(simple, shell);
        if !expanded.args.is_empty() {
            cmd.args(&expanded.args[1..]);
        }

        unsafe {
            cmd.pre_exec(|| {
                libc::signal(libc::SIGINT, libc::SIG_DFL);
                libc::signal(libc::SIGQUIT, libc::SIG_IGN);
                Ok(())
            });
        }

        if i == 0 {
            let wfd = pipes[0].1;
            cmd.stdout(unsafe { Stdio::from_raw_fd(wfd) });
        } else if i == n - 1 {
            let rfd = pipes[i - 1].0;
            cmd.stdin(unsafe { Stdio::from_raw_fd(rfd) });
        } else {
            let rfd = pipes[i - 1].0;
            let wfd = pipes[i].1;
            cmd.stdin(unsafe { Stdio::from_raw_fd(rfd) });
            cmd.stdout(unsafe { Stdio::from_raw_fd(wfd) });
        }

        apply_redirects(&mut cmd, &simple.redirects, shell);

        match cmd.spawn() {
            Ok(c) => children.push(c),
            Err(e) => {
                eprintln!("bsh: {e}");
                for &(rfd, wfd) in &pipes {
                    unsafe { libc::close(rfd); libc::close(wfd); }
                }
                restore_shell_terminal(shell);
                return 126;
            }
        }
    }

    for &(rfd, wfd) in &pipes {
        unsafe { libc::close(rfd); libc::close(wfd); }
    }

    drop(pipes);

    let mut last_code = 0;
    for mut child in children {
        if let Ok(status) = child.wait() {
            last_code = exit_status_code(status);
        }
    }
    restore_shell_terminal(shell);
    last_code
}

fn exec_pipeline_simple(simple: &Simple, shell: &Shell) -> i32 {
    if simple.args.is_empty() {
        return exec_redirect_only(simple, shell);
    }

    let simple = expand_aliases(simple, shell);

    let cmd_name = &simple.args[0];
    if is_builtin(cmd_name, shell) {
        let mut shell_copy = Shell {
            last_exit: shell.last_exit,
            last_bg_pid: shell.last_bg_pid,
            running_bg: shell.running_bg.clone(),
            vars: shell.vars.clone(),
            aliases: shell.aliases.clone(),
            config_path: shell.config_path.clone(),
            last_cmd: shell.last_cmd.clone(),
            prev_cmd: shell.prev_cmd.clone(),
            background_job: shell.background_job,
            logging: shell.logging,
            shit: shell.shit,
            history: shell.history,
            autosuggest: shell.autosuggest,
            fresh: shell.fresh,
        };
        let mut expanded = Simple {
            args: Vec::new(),
            redirects: simple.redirects.clone(),
        };
        for arg in &simple.args {
            expanded.args.extend(expand_word(arg, &shell_copy));
        }
        return exec_builtin(&expanded, &mut shell_copy);
    }
    exec_external(&simple, shell)
}

fn exec_background(simple: &Simple, shell: &mut Shell) -> i32 {
    match unsafe { libc::fork() } {
        -1 => {
            eprintln!("bsh: fork failed");
            1
        }
        0 => {
            unsafe { libc::signal(libc::SIGINT, libc::SIG_DFL); }
            shell.background_job = true;
            let code = exec_pipeline_simple(simple, shell);
            process::exit(code);
        }
        pid => {
            let pid_u32 = pid as u32;
            println!("[{}] {}", shell.running_bg.len() + 1, pid_u32);
            shell.last_bg_pid = Some(pid_u32);
            shell.running_bg.push(pid_u32);
            0
        }
    }
}

fn exec_pipeline_background(cmds: &[Simple], shell: &mut Shell) -> i32 {
    match unsafe { libc::fork() } {
        -1 => {
            eprintln!("bsh: fork failed");
            1
        }
        0 => {
            unsafe { libc::signal(libc::SIGINT, libc::SIG_DFL); }
            shell.background_job = true;
            let code = exec_pipeline(cmds, shell);
            process::exit(code);
        }
        pid => {
            let pid_u32 = pid as u32;
            println!("[{}] {}", shell.running_bg.len() + 1, pid_u32);
            shell.last_bg_pid = Some(pid_u32);
            shell.running_bg.push(pid_u32);
            0
        }
    }
}

fn exit_status_code(status: process::ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        code
    } else {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            128 + sig
        } else {
            1
        }
    }
}

fn reap_background(shell: &mut Shell) {
    shell.running_bg.retain(|&pid| {
        unsafe {
            let mut status: i32 = 0;
            let ret = libc::waitpid(pid as i32, &mut status as *mut i32, libc::WNOHANG);
            if ret == pid as i32 {
                if libc::WIFEXITED(status) || libc::WIFSIGNALED(status) {
                    return false;
                }
            }
        }
        true
    });
}

// =============================================================================
// Config
// =============================================================================
// Example config (~/.config/bsh/config):
// prompt {
//     echo "Welcome to bsh"
//     echo "Current directory: "
// }
// if-interactive {
//     echo "Running in interactive mode"
//     echo "Use arrow keys for history, TAB for suggestions"
// }
// if-not-interactive {
//     echo "Running in non-interactive mode"
// }
// alias ll="ls -l"
// path /usr/local/bin
// log on
// shit on
// history on
// auto-suggestion on

enum ConfigLine {
    Echo(Vec<String>),
}

struct Config {
    prompt_lines: Vec<ConfigLine>,
    aliases: HashMap<String, String>,
    paths: Vec<String>,
    if_interactive_lines: Vec<String>,
    logging: bool,
    shit: bool,
    history: bool,
    autosuggest: bool,
}

fn load_config() -> Option<Config> {
    let home = env::var("HOME").ok()?;
    let config_path = Path::new(&home).join(".config").join("bsh").join("config");
    if !config_path.exists() {
        return None;
    }
    let text = std::fs::read_to_string(&config_path).ok()?;
    let mut lines: Vec<&str> = text.lines().map(|l| l.trim()).collect();
    lines.retain(|l| !l.is_empty() && !l.starts_with('#'));

    let mut prompt_lines = Vec::new();
    let mut aliases = HashMap::new();
    let mut paths = Vec::new();
    let mut if_interactive_lines = Vec::new();
    let mut logging = false;
    let mut shit = true;
    let mut history = true;
    let mut autosuggest = true;

    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if line == "prompt {" || line == "function prompt {" {
            i += 1;
            while i < lines.len() && lines[i] != "}" {
                let body = lines[i].trim();
                if !body.is_empty() && !body.starts_with('#') {
                    if let Some(cl) = parse_config_line(body) {
                        prompt_lines.push(cl);
                    }
                }
                i += 1;
            }
        } else if line == "if-interactive {" {
            i += 1;
            while i < lines.len() && lines[i] != "}" {
                let body = lines[i].trim();
                if !body.is_empty() && !body.starts_with('#') {
                    if_interactive_lines.push(body.to_string());
                }
                i += 1;
            }
        } else if let Some(rest) = line.strip_prefix("alias ") {
            if let Some((name, value)) = rest.split_once('=') {
                let name = name.trim();
                let mut value = value.trim();
                if (value.starts_with('"') && value.ends_with('"')) || (value.starts_with('\'') && value.ends_with('\'')) {
                    let len = value.len();
                    if len >= 2 {
                        value = &value[1..len-1];
                    }
                }
                if !name.is_empty() {
                    aliases.insert(name.to_string(), value.to_string());
                }
            }
        } else if let Some(dir) = line.strip_prefix("path ") {
            let dir = dir.trim();
            if !dir.is_empty() {
                paths.push(dir.to_string());
            }
        } else if let Some(val) = line.strip_prefix("log ") {
            logging = val.trim() == "on";
        } else if let Some(val) = line.strip_prefix("shit ") {
            shit = val.trim() == "on";
        } else if let Some(val) = line.strip_prefix("history ") {
            history = val.trim() == "on";
        } else if let Some(val) = line.strip_prefix("auto-suggestion ") {
            autosuggest = val.trim() == "on";
        }
        i += 1;
    }

    Some(Config { prompt_lines, aliases, paths, if_interactive_lines, logging, shit, history, autosuggest })
}

fn parse_config_line(line: &str) -> Option<ConfigLine> {
    let line = line.trim();
    if line.starts_with("echo ") || line == "echo" {
        let args = parse_echo_args(&line[4..].trim_start());
        Some(ConfigLine::Echo(args))
    } else {
        None
    }
}

fn parse_echo_args(s: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut i = 0;
    let chars: Vec<char> = s.chars().collect();
    while i < chars.len() {
        if chars[i].is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if chars[i] == '"' {
            i += 1;
            let mut arg = String::new();
            while i < chars.len() && chars[i] != '"' {
                if chars[i] == '\\' && i + 1 < chars.len() {
                    i += 1;
                    match chars[i] {
                        '\\' => arg.push('\\'),
                        '"' => arg.push('"'),
                        c => {
                            arg.push('\\');
                            arg.push(c);
                        }
                    }
                } else {
                    arg.push(chars[i]);
                }
                i += 1;
            }
            if i < chars.len() { i += 1; }
            args.push(arg);
        } else if chars[i] == '\'' {
            i += 1;
            let mut arg = String::new();
            while i < chars.len() && chars[i] != '\'' {
                arg.push(chars[i]);
                i += 1;
            }
            if i < chars.len() { i += 1; }
            args.push(arg);
        } else {
            let mut arg = String::new();
            while i < chars.len() && !chars[i].is_ascii_whitespace() {
                if chars[i] == '\'' || chars[i] == '"' {
                    break;
                }
                arg.push(chars[i]);
                i += 1;
            }
            args.push(arg);
        }
    }
    args
}

fn render_config_prompt(config: &Config, shell: &Shell) -> String {
    let mut out = String::new();
    let mut first = true;
    for line in &config.prompt_lines {
        match line {
            ConfigLine::Echo(args) => {
                if !first {
                    out.push('\n');
                }
                for (j, arg) in args.iter().enumerate() {
                    if j > 0 { out.push(' '); }
                    out.push_str(&expand_subshells(arg, shell));
                }
                first = false;
            }
        }
    }
    render_ps1_format(&out, shell)
}

fn expand_subshells(input: &str, shell: &Shell) -> String {
    let mut out = String::new();
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '$' && chars.peek() == Some(&'(') {
            chars.next();
            let mut cmd = String::new();
            let mut depth = 1;
            while let Some(c) = chars.next() {
                if c == '(' { depth += 1; }
                if c == ')' { depth -= 1; }
                if depth == 0 { break; }
                cmd.push(c);
            }
            let result = execute_subshell(&cmd, shell);
            out.push_str(&result);
        } else {
            out.push(c);
        }
    }
    out
}

fn execute_subshell(cmd: &str, _shell: &Shell) -> String {
    let trimmed = cmd.trim();
    if trimmed.is_empty() { return String::new(); }

    let child = match Command::new("sh").arg("-c").arg(trimmed)
        .stdout(process::Stdio::piped())
        .stderr(process::Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return String::new(),
    };

    let mut output = String::new();
    if let Ok(out) = child.wait_with_output() {
        if let Ok(s) = String::from_utf8(out.stdout) {
            output = s.trim_end_matches('\n').to_string();
        }
    }
    output
}

// =============================================================================
// Prompt
// =============================================================================

fn render_ps1_format(fmt: &str, shell: &Shell) -> String {
    let mut out = String::new();
    let mut chars = fmt.chars();

    let cwd = env::current_dir().unwrap_or_default();
    let cwd_str = cwd.to_string_lossy().to_string();
    let home = env::var("HOME").unwrap_or_default();

    while let Some(c) = chars.next() {
        if c != '\\' && c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('w') => {
                if !home.is_empty() && cwd_str.starts_with(&home) {
                    out.push('~');
                    out.push_str(&cwd_str[home.len()..]);
                } else {
                    out.push_str(&cwd_str);
                }
            }
            Some('W') => {
                if !home.is_empty() && cwd_str.starts_with(&home) {
                    if cwd_str.len() == home.len() {
                        out.push('~');
                    } else {
                        out.push_str(cwd_str.rsplit('/').next().unwrap_or(""));
                    }
                } else {
                    out.push_str(cwd_str.rsplit('/').next().unwrap_or(""));
                }
            }
            Some('u') => out.push_str(&env::var("USER").unwrap_or_else(|_| "user".into())),
            Some('h') => {
                let host = if let Ok(h) = env::var("HOST") {
                    h
                } else {
                    gethostname()
                };
                if let Some(dot) = host.find('.') {
                    out.push_str(&host[..dot]);
                } else {
                    out.push_str(&host);
                }
            }
            Some('$') | Some('#') => {
                let uid = unsafe { libc::geteuid() };
                if uid == 0 { out.push('#'); } else { out.push('$'); }
            }
            Some('t') => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default();
                let secs = now.as_secs() % 86400;
                let h = secs / 3600;
                let m = (secs % 3600) / 60;
                let s = secs % 60;
                out.push_str(&format!("{:02}:{:02}:{:02}", h, m, s));
            }
            Some('n') => out.push('\n'),
            Some('\\') => out.push('\\'),
            Some('%') => out.push('%'),
            Some('[') => {
                let mut ansi = String::new();
                loop {
                    match chars.next() {
                        Some(']') => break,
                        Some(c) => ansi.push(c),
                        None => break,
                    }
                }
                out.push_str(&ansi);
            }
            Some('F') => {
                let mut color = String::new();
                loop {
                    match chars.next() {
                        Some('}') => break,
                        Some(c) => color.push(c),
                        None => break,
                    }
                }
                let code = color_to_ansi(&color);
                if let Some(c) = code {
                    out.push_str(&c);
                }
            }
            Some('f') => {
                out.push_str("\x1b[0m");
            }
            Some(c) => {
                out.push('\\');
                out.push(c);
            }
            None => {}
        }
    }

    if shell.last_exit != 0 {
        format!("\x1b[31m{}\x1b[0m", out)
    } else {
        out
    }
}

fn render_prompt(shell: &Shell) -> String {
    let fmt = env::var("PS1").unwrap_or_else(|_| "\\u@\\h \\$ ".into());
    render_ps1_format(&fmt, shell)
}

fn gethostname() -> String {
    let mut buf = [0i8; 256];
    unsafe {
        if libc::gethostname(buf.as_mut_ptr(), buf.len()) == 0 {
            return CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned();
        }
    }
    "localhost".into()
}

fn color_to_ansi(name: &str) -> Option<String> {
    let code = match name.to_lowercase().as_str() {
        "black" => "0;30",
        "red" => "0;31",
        "green" => "0;32",
        "yellow" => "0;33",
        "blue" => "0;34",
        "magenta" => "0;35",
        "cyan" => "0;36",
        "white" => "0;37",
        "bold" => "1",
        "dim" => "2",
        "italic" => "3",
        "underline" => "4",
        "blink" => "5",
        _ => return None,
    };
    Some(format!("\x1b[{code}m"))
}

// =============================================================================
// Main
// =============================================================================

fn main() {
    let args: Vec<String> = env::args().collect();

    let fresh = args.len() >= 2 && args[1] == "--fresh";

    let (logging_on, shit_on, history_on, autosuggest_on) = if fresh {
        (false, true, true, true)
    } else {
        (
            load_config().map(|c| c.logging).unwrap_or(false),
            load_config().map(|c| c.shit).unwrap_or(true),
            load_config().map(|c| c.history).unwrap_or(true),
            load_config().map(|c| c.autosuggest).unwrap_or(true),
        )
    };

    // Debug: log received args
    if logging_on {
        let args_debug: Vec<String> = args.iter().enumerate().map(|(i, a)| format!("[{}]={}", i, a)).collect();
        log_line(&format!("args: {:?}", args_debug));
    }

    // -c <command>: execute command and exit (non-interactive)
    if args.len() >= 3 && args[1] == "-c" {
        setup_signal_handlers();
        let config_path = env::var("HOME").ok()
            .map(|h| Path::new(&h).join(".config").join("bsh").join("config"));
        let mut shell = Shell {
            last_exit: 0,
            last_bg_pid: None,
            running_bg: Vec::new(),
            vars: HashMap::new(),
            aliases: HashMap::new(),
        config_path: if fresh { None } else { config_path.clone() },
            last_cmd: String::new(),
            prev_cmd: String::new(),
            background_job: false,
            logging: logging_on,
            shit: shit_on,
            history: history_on,
            autosuggest: autosuggest_on,
            fresh: false,
        };
        if let Some(ref cfg) = load_config() {
            for (name, value) in &cfg.aliases {
                shell.aliases.insert(name.clone(), value.clone());
            }
            for dir in &cfg.paths {
                let expanded = if dir.starts_with('~') {
                    let home = env::var("HOME").unwrap_or_default();
                    format!("{}{}", home, &dir[1..])
                } else {
                    dir.clone()
                };
                let dir_path = std::path::Path::new(&expanded);
                let current = env::var("PATH").unwrap_or_default();
                let already = env::split_paths(&current).any(|p| p == *dir_path);
                if !already {
                    let sep = if current.is_empty() { "" } else { ":" };
                    let new_path = format!("{current}{sep}{expanded}");
                    unsafe { env::set_var("PATH", &new_path); }
                }
            }
        }
        let cmd = &args[2];
        shell.last_cmd = cmd.to_string();
        let code = exec_line(cmd, &mut shell);
        log_command(&shell, cmd);
        process::exit(code);
    }

    setup_signal_handlers();

    let interactive = unsafe { libc::isatty(libc::STDIN_FILENO) != 0 };

    let orig_termios = if interactive {
        let orig = enable_raw_mode();
        *ORIG_TERMIOS.lock().unwrap() = Some(orig);
        Some(orig)
    } else {
        None
    };

    let config_path = env::var("HOME").ok()
        .map(|h| Path::new(&h).join(".config").join("bsh").join("config"));

    let mut shell = Shell {
        last_exit: 0,
        last_bg_pid: None,
        running_bg: Vec::new(),
        vars: HashMap::new(),
        aliases: HashMap::new(),
        config_path: config_path.clone(),
        last_cmd: String::new(),
        prev_cmd: String::new(),
        background_job: false,
        logging: logging_on,
        shit: shit_on,
        history: history_on,
        autosuggest: autosuggest_on,
        fresh,
    };

    let config = if fresh { None } else { load_config() };
    if let Some(ref cfg) = config {
        for (name, value) in &cfg.aliases {
            shell.aliases.insert(name.clone(), value.clone());
        }
        for dir in &cfg.paths {
            let expanded = if dir.starts_with('~') {
                let home = env::var("HOME").unwrap_or_default();
                format!("{}{}", home, &dir[1..])
            } else {
                dir.clone()
            };
            let dir_path = std::path::Path::new(&expanded);
            let current = env::var("PATH").unwrap_or_default();
            let already = env::split_paths(&current).any(|p| p == *dir_path);
            if !already {
                let sep = if current.is_empty() { "" } else { ":" };
                let new_path = format!("{current}{sep}{expanded}");
                unsafe { env::set_var("PATH", &new_path); }
            }
        }
    }

    if interactive {
        if let Some(ref cfg) = config {
            for line in &cfg.if_interactive_lines {
                shell.last_exit = exec_line(line, &mut shell);
            }
        }
    }

    let mut history = if fresh { History::empty() } else { History::new() };

    loop {
        if sigint_pending() {
            shell.last_exit = 130;
        }

        reap_background(&mut shell);

        let line = if interactive {
            let prompt = if let Some(ref cfg) = config {
                let p = render_config_prompt(cfg, &shell);
                if p.is_empty() { render_prompt(&shell) } else { p }
            } else {
                render_prompt(&shell)
            };
            match read_line_interactive(&prompt, &mut history, shell.autosuggest) {
                ReadLineResult::Line(l) => l,
                ReadLineResult::CtrlC => {
                    shell.last_exit = 130;
                    continue;
                }
                ReadLineResult::Eof => break,
            }
        } else {
            let mut line = String::new();
            match io::stdin().read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            line
        };

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        shell.prev_cmd = std::mem::replace(&mut shell.last_cmd, trimmed.to_string());
        shell.last_exit = exec_line(trimmed, &mut shell);
        log_command(&shell, trimmed);

        if shell.history && !trimmed.starts_with('#') && shell.last_exit != 127 {
            history.add(trimmed);
        }
    }

    if let Some(orig) = orig_termios {
        disable_raw_mode(&orig);
    }
}
