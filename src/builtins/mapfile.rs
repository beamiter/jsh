//! `mapfile` / `readarray`: read lines (or `-d`-delimited records) from a
//! file descriptor into an indexed array.

use crate::environment::ShellState;
use nix::libc;
use std::os::fd::RawFd;

/// Upper bound on the bytes one `mapfile` call stores. Input such as
/// `/dev/zero` has no line ends at all; failing loudly beats exhausting memory.
pub(super) const MAX_MAPFILE_BYTES: usize = 256 * 1024 * 1024;

/// Bash runs the `-C` callback every 5000 lines unless `-c` says otherwise.
const DEFAULT_CALLBACK_QUANTUM: usize = 5000;

const USAGE: &str = "mapfile: usage: mapfile [-d delim] [-n count] [-O origin] [-s count] [-t] [-u fd] [-C callback] [-c quantum] [array]";

/// Reads delimited records from a descriptor without consuming input that
/// belongs to the next reader. A seekable descriptor is read in blocks and
/// rewound past whatever was not used; anything else (a pipe, a terminal) is
/// read one byte at a time when the caller stops early, exactly as bash does.
struct RecordReader {
    fd: RawFd,
    buffer: Vec<u8>,
    start: usize,
    seekable: bool,
    /// Block reads are safe: the caller reads to end of input.
    whole_input: bool,
}

impl RecordReader {
    fn new(fd: RawFd, whole_input: bool) -> Self {
        // SAFETY: lseek only inspects the descriptor; failure means "not
        // seekable" (ESPIPE) or a bad descriptor, which read reports later.
        let seekable = unsafe { libc::lseek(fd, 0, libc::SEEK_CUR) } >= 0;
        Self {
            fd,
            buffer: Vec::new(),
            start: 0,
            seekable,
            whole_input,
        }
    }

    /// Fill the buffer with more input; `Ok(false)` at end of input.
    fn fill(&mut self) -> Result<bool, i32> {
        if self.start > 0 {
            self.buffer.drain(..self.start);
            self.start = 0;
        }
        let chunk = if self.seekable || self.whole_input {
            64 * 1024
        } else {
            1
        };
        let old_len = self.buffer.len();
        self.buffer.resize(old_len + chunk, 0);
        loop {
            if let Some(status) = crate::signal::pending_status() {
                self.buffer.truncate(old_len);
                return Err(status);
            }
            // SAFETY: the destination is the freshly resized tail of `buffer`.
            let read =
                unsafe { libc::read(self.fd, self.buffer[old_len..].as_mut_ptr().cast(), chunk) };
            if read >= 0 {
                self.buffer.truncate(old_len + read as usize);
                return Ok(read > 0);
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                self.buffer.truncate(old_len);
                eprintln!("jsh: mapfile: read error: {}", super::os_error_text(&error));
                return Err(1);
            }
        }
    }

    /// The next record including its delimiter (absent at end of input), or
    /// `None` once the input is exhausted.
    fn next_record(&mut self, delimiter: u8) -> Result<Option<Vec<u8>>, i32> {
        let mut scanned = self.start;
        loop {
            if let Some(offset) = self.buffer[scanned..].iter().position(|&b| b == delimiter) {
                let end = scanned + offset + 1;
                let record = self.buffer[self.start..end].to_vec();
                self.start = end;
                return Ok(Some(record));
            }
            if self.buffer.len() - self.start > MAX_MAPFILE_BYTES {
                eprintln!("jsh: mapfile: input exceeds {MAX_MAPFILE_BYTES} bytes");
                return Err(1);
            }
            scanned = self.buffer.len();
            let consumed = self.start;
            if !self.fill()? {
                if self.start == self.buffer.len() {
                    return Ok(None);
                }
                let record = self.buffer[self.start..].to_vec();
                self.start = self.buffer.len();
                return Ok(Some(record));
            }
            // `fill` moved the unread bytes to the front of the buffer.
            scanned -= consumed;
        }
    }

    /// Give back buffered input nobody consumed, so the next reader of a
    /// seekable descriptor (a redirected file) starts where we stopped.
    fn finish(self) {
        let unread = self.buffer.len() - self.start;
        if self.seekable && unread > 0 {
            // SAFETY: rewinds the shared file offset by bytes we read but
            // did not use; no memory is involved.
            unsafe { libc::lseek(self.fd, -(unread as libc::off_t), libc::SEEK_CUR) };
        }
    }
}

fn parse_count(value: &str) -> Option<usize> {
    value.trim().parse::<usize>().ok()
}

pub(super) fn builtin_mapfile(name: &str, args: &[String], state: &mut ShellState) -> i32 {
    let mut delimiter = b'\n';
    let mut max_records: Option<usize> = None;
    let mut origin: Option<usize> = None;
    let mut skip = 0usize;
    let mut trim = false;
    let mut fd: RawFd = 0;
    let mut callback: Option<String> = None;
    let mut quantum = DEFAULT_CALLBACK_QUANTUM;
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        if arg == "--" {
            index += 1;
            break;
        }
        let Some(flags) = arg.strip_prefix('-').filter(|flags| !flags.is_empty()) else {
            break;
        };
        index += 1;
        for (at, flag) in flags.char_indices() {
            if flag == 't' {
                trim = true;
                continue;
            }
            if !matches!(flag, 'd' | 'n' | 'O' | 's' | 'u' | 'C' | 'c') {
                eprintln!("jsh: {name}: -{flag}: invalid option");
                eprintln!("{USAGE}");
                return 2;
            }
            // The option's value is the rest of this word, or the next word.
            let attached = &arg[1 + at + flag.len_utf8()..];
            let value = if !attached.is_empty() {
                attached.to_string()
            } else if let Some(next) = args.get(index) {
                index += 1;
                next.clone()
            } else {
                eprintln!("jsh: {name}: -{flag}: option requires an argument");
                eprintln!("{USAGE}");
                return 2;
            };
            match flag {
                // `-d ''` reads NUL-terminated records.
                'd' => delimiter = value.bytes().next().unwrap_or(0),
                'n' => match parse_count(&value) {
                    Some(0) => max_records = None,
                    Some(count) => max_records = Some(count),
                    None => {
                        eprintln!("jsh: {name}: {value}: invalid line count");
                        return 1;
                    }
                },
                'O' => match parse_count(&value) {
                    Some(start) => origin = Some(start),
                    None => {
                        eprintln!("jsh: {name}: {value}: invalid array origin");
                        return 1;
                    }
                },
                's' => match parse_count(&value) {
                    Some(count) => skip = count,
                    None => {
                        eprintln!("jsh: {name}: {value}: invalid line count");
                        return 1;
                    }
                },
                'u' => match value.trim().parse::<RawFd>() {
                    Ok(number) if number >= 0 => fd = number,
                    _ => {
                        eprintln!("jsh: {name}: {value}: invalid file descriptor specification");
                        return 1;
                    }
                },
                'C' => callback = Some(value),
                _ => match parse_count(&value) {
                    Some(count) if count > 0 => quantum = count,
                    _ => {
                        eprintln!("jsh: {name}: {value}: invalid callback quantum");
                        return 1;
                    }
                },
            }
            break;
        }
    }
    // Bash ignores operands after the array name.
    let array = args.get(index).map_or("MAPFILE", String::as_str);
    if !crate::environment::is_valid_identifier(array) {
        eprintln!("jsh: {name}: `{array}': not a valid identifier");
        return 1;
    }
    // SAFETY: F_GETFD only queries the descriptor's flags.
    if unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 {
        let error = std::io::Error::last_os_error();
        eprintln!(
            "jsh: {name}: {fd}: invalid file descriptor: {}",
            super::os_error_text(&error)
        );
        return 1;
    }

    // Without -O the array is emptied first; with it, elements below the
    // origin survive.
    if origin.is_none() {
        state.set_array(array, Vec::new());
    }
    let mut next_index = origin.unwrap_or(0);
    let mut reader = RecordReader::new(fd, max_records.is_none());
    let mut status = 0;
    for _ in 0..skip {
        match reader.next_record(delimiter) {
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(code) => {
                status = code;
                break;
            }
        }
    }
    let mut stored = 0usize;
    let mut stored_bytes = 0usize;
    while status == 0 && max_records.is_none_or(|max| stored < max) {
        let mut record = match reader.next_record(delimiter) {
            Ok(Some(record)) => record,
            Ok(None) => break,
            Err(code) => {
                status = code;
                break;
            }
        };
        if trim && record.last() == Some(&delimiter) {
            record.pop();
        }
        stored_bytes += record.len();
        if stored_bytes > MAX_MAPFILE_BYTES {
            eprintln!("jsh: {name}: input exceeds {MAX_MAPFILE_BYTES} bytes");
            status = 1;
            break;
        }
        let line = String::from_utf8_lossy(&record).into_owned();
        stored += 1;
        // The callback sees the index about to be assigned and the line,
        // before the assignment, exactly as bash evaluates it.
        if let Some(callback) = &callback {
            if stored.is_multiple_of(quantum) {
                let command = format!(
                    "{callback} {next_index} {}",
                    crate::parser::unparse::single_quote(&line)
                );
                if let Ok(program) = crate::parser::parse(&command) {
                    crate::executor::execute_program(&program, state);
                }
            }
        }
        state.set_array_element(array, &next_index.to_string(), &line);
        next_index += 1;
    }
    reader.finish();
    status
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn records(input: &[u8], delimiter: u8) -> Vec<Vec<u8>> {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(input).unwrap();
        use std::io::Seek;
        file.rewind().unwrap();
        use std::os::fd::AsRawFd;
        let mut reader = RecordReader::new(file.as_raw_fd(), true);
        let mut out = Vec::new();
        while let Some(record) = reader.next_record(delimiter).unwrap() {
            out.push(record);
        }
        out
    }

    #[test]
    fn records_keep_delimiters_and_a_final_partial_line() {
        assert_eq!(
            records(b"a\nb\nc", b'\n'),
            [b"a\n".to_vec(), b"b\n".to_vec(), b"c".to_vec()]
        );
        assert_eq!(records(b"x\0y\0", 0), [b"x\0".to_vec(), b"y\0".to_vec()]);
        assert!(records(b"", b'\n').is_empty());
    }

    #[test]
    fn records_span_block_boundaries() {
        let mut input = vec![b'x'; 100_000];
        input.push(b'\n');
        input.extend_from_slice(b"tail\n");
        let got = records(&input, b'\n');
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].len(), 100_001);
        assert_eq!(got[1], b"tail\n");
    }

    #[test]
    fn stopping_early_rewinds_a_seekable_descriptor() {
        use std::io::{Read, Seek};
        use std::os::fd::AsRawFd;
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"one\ntwo\nthree\n").unwrap();
        file.rewind().unwrap();
        let mut reader = RecordReader::new(file.as_raw_fd(), false);
        assert_eq!(reader.next_record(b'\n').unwrap().unwrap(), b"one\n");
        reader.finish();
        let mut rest = String::new();
        file.read_to_string(&mut rest).unwrap();
        assert_eq!(rest, "two\nthree\n");
    }
}
