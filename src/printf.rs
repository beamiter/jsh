//! Streaming shell printf formatting. Only validated numeric conversions reach
//! libc's printf family; shell text is always written as data, never as a C
//! printf format. (`%(fmt)T` hands `fmt` to strftime, which has no varargs and
//! so cannot read or write memory it was not given.)
use std::ffi::CString;
use std::io::{self, Write};
use std::iter::Peekable;
use std::str::Chars;

const MAX_FIELD: usize = 1_048_576;

#[derive(Debug)]
pub(crate) enum Error {
    Format(String),
    Io(io::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Format(message) => f.write_str(message),
            Self::Io(error) => error.fmt(f),
        }
    }
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

#[derive(Default)]
struct Spec {
    flags: String,
    width: usize,
    precision: Option<usize>,
    /// The strftime format of a `%(fmt)T` conversion.
    time_format: Option<String>,
}

/// What a printf call needs from the shell beyond its arguments.
#[derive(Clone, Copy)]
pub(crate) struct Context {
    /// Seconds since the epoch when the shell started: `%(fmt)T` with `-2`.
    pub(crate) shell_start: i64,
}

fn argument<'a>(args: &'a [String], index: &mut usize) -> &'a str {
    let value = args.get(*index).map(String::as_str).unwrap_or("");
    *index += 1;
    value
}

fn field(chars: &mut Peekable<Chars<'_>>) -> Result<usize, Error> {
    let mut value = 0usize;
    while let Some(digit) = chars.peek().and_then(|c| c.to_digit(10)) {
        chars.next();
        value = value
            .checked_mul(10)
            .and_then(|value| value.checked_add(digit as usize))
            .filter(|value| *value <= MAX_FIELD)
            .ok_or_else(|| Error::Format("field width or precision exceeds 1048576".into()))?;
    }
    Ok(value)
}

fn star(args: &[String], index: &mut usize) -> Result<i64, Error> {
    let value = argument(args, index);
    match parse_number(value, 'd') {
        (Numeric::Signed(n), NumberStatus::Valid) if n.unsigned_abs() <= MAX_FIELD as u64 => Ok(n),
        _ => Err(Error::Format(format!(
            "invalid field width or precision: {value:?}"
        ))),
    }
}

fn parse_spec(
    chars: &mut Peekable<Chars<'_>>,
    args: &[String],
    index: &mut usize,
) -> Result<(Spec, char), Error> {
    let mut spec = Spec::default();
    while let Some(&flag @ ('-' | '+' | ' ' | '#' | '0')) = chars.peek() {
        chars.next();
        if !spec.flags.contains(flag) {
            spec.flags.push(flag);
        }
    }
    if chars.peek() == Some(&'*') {
        chars.next();
        let width = star(args, index)?;
        if width < 0 && !spec.flags.contains('-') {
            spec.flags.push('-');
        }
        spec.width = width.unsigned_abs() as usize;
    } else {
        spec.width = field(chars)?;
    }
    if chars.peek() == Some(&'.') {
        chars.next();
        spec.precision = if chars.peek() == Some(&'*') {
            chars.next();
            let precision = star(args, index)?;
            (precision >= 0).then_some(precision as usize)
        } else {
            Some(field(chars)?)
        };
    }
    if chars.peek() == Some(&'(') {
        // `%(fmt)T`: the strftime format runs to the first `)`, as in bash.
        chars.next();
        let mut time_format = String::new();
        loop {
            match chars.next() {
                Some(')') => break,
                Some(c) if time_format.len() < MAX_FIELD => time_format.push(c),
                Some(_) => return Err(Error::Format("time format exceeds 1048576 bytes".into())),
                None => return Err(Error::Format("missing `)' in time format".into())),
            }
        }
        if chars.next() != Some('T') {
            return Err(Error::Format(
                "`(' must introduce a %(...)T conversion".into(),
            ));
        }
        spec.time_format = Some(time_format);
        return Ok((spec, 'T'));
    }
    let conversion = chars
        .next()
        .ok_or_else(|| Error::Format("incomplete format conversion".into()))?;
    if !matches!(
        conversion,
        's' | 'b'
            | 'c'
            | 'q'
            | 'd'
            | 'i'
            | 'u'
            | 'o'
            | 'x'
            | 'X'
            | 'f'
            | 'F'
            | 'e'
            | 'E'
            | 'g'
            | 'G'
    ) {
        return Err(Error::Format(format!(
            "invalid format conversion: %{conversion}"
        )));
    }
    Ok((spec, conversion))
}

fn padding(output: &mut impl Write, mut count: usize) -> io::Result<()> {
    const SPACES: [u8; 256] = [b' '; 256];
    while count != 0 {
        let chunk = count.min(SPACES.len());
        output.write_all(&SPACES[..chunk])?;
        count -= chunk;
    }
    Ok(())
}

fn text(output: &mut impl Write, bytes: &[u8], spec: &Spec) -> io::Result<()> {
    let bytes = &bytes[..spec.precision.unwrap_or(bytes.len()).min(bytes.len())];
    let pad = spec.width.saturating_sub(bytes.len());
    if !spec.flags.contains('-') {
        padding(output, pad)?;
    }
    output.write_all(bytes)?;
    if spec.flags.contains('-') {
        padding(output, pad)?;
    }
    Ok(())
}

enum Numeric {
    Signed(i64),
    Unsigned(u64),
    Float(f64),
}

enum NumberStatus {
    Valid,
    Invalid,
    OutOfRange,
}

fn parse_number(value: &str, conversion: char) -> (Numeric, NumberStatus) {
    let float = matches!(conversion, 'f' | 'F' | 'e' | 'E' | 'g' | 'G');
    let signed = matches!(conversion, 'd' | 'i');
    let from_code = |n: u32| {
        if float {
            Numeric::Float(n as f64)
        } else if signed {
            Numeric::Signed(n as i64)
        } else {
            Numeric::Unsigned(n as u64)
        }
    };
    if value.is_empty() {
        return (from_code(0), NumberStatus::Valid);
    }
    if let Some(value) = value.strip_prefix(['\'', '"']) {
        return (
            from_code(value.chars().next().map(u32::from).unwrap_or(0)),
            NumberStatus::Valid,
        );
    }
    let Ok(value) = CString::new(value) else {
        return (from_code(0), NumberStatus::Invalid);
    };
    let start = value.as_ptr();
    let mut end = std::ptr::null_mut();
    nix::errno::Errno::clear();
    // SAFETY: CString supplies a NUL-terminated input. libc sets end to a
    // pointer into that same allocation (possibly its terminating NUL).
    // These parsers accept data, not format strings, and report range errors.
    let result = unsafe {
        if float {
            Numeric::Float(nix::libc::strtod(start, &mut end))
        } else if signed {
            Numeric::Signed(nix::libc::strtoll(start, &mut end, 0))
        } else {
            Numeric::Unsigned(nix::libc::strtoull(start, &mut end, 0))
        }
    };
    let range_error = nix::errno::Errno::last() == nix::errno::Errno::ERANGE;
    // SAFETY: end is within the live CString above, as required by strto*.
    let complete = end != start.cast_mut() && unsafe { *end == 0 };
    let status = if !complete {
        NumberStatus::Invalid
    } else if range_error {
        NumberStatus::OutOfRange
    } else {
        NumberStatus::Valid
    };
    (result, status)
}

fn numeric(
    output: &mut impl Write,
    spec: &Spec,
    conversion: char,
    value: Numeric,
) -> Result<(), Error> {
    let mut format = format!("%{}", spec.flags);
    if spec.width != 0 {
        format.push_str(&spec.width.to_string());
    }
    if let Some(precision) = spec.precision {
        format.push_str(&format!(".{precision}"));
    }
    if !matches!(value, Numeric::Float(_)) {
        format.push_str("ll");
    }
    format.push(conversion);
    let format = CString::new(format).expect("validated printf format is ASCII");
    // SAFETY: format is constructed only from validated flags, bounded decimal
    // fields and the conversion whitelist. Integer conversions carry `ll` and
    // receive the matching C long long type; float conversions receive double.
    // No user-controlled string or `%n` can become a conversion here.
    let render = |buffer: *mut nix::libc::c_char, size: usize| unsafe {
        match value {
            Numeric::Signed(n) => {
                nix::libc::snprintf(buffer, size, format.as_ptr(), n as nix::libc::c_longlong)
            }
            Numeric::Unsigned(n) => {
                nix::libc::snprintf(buffer, size, format.as_ptr(), n as nix::libc::c_ulonglong)
            }
            Numeric::Float(n) => {
                nix::libc::snprintf(buffer, size, format.as_ptr(), n as nix::libc::c_double)
            }
        }
    };
    let size = render(std::ptr::null_mut(), 0);
    if size < 0 || size as usize > MAX_FIELD * 2 {
        return Err(Error::Format("numeric format result is too large".into()));
    }
    let mut bytes = vec![0u8; size as usize + 1];
    if render(bytes.as_mut_ptr().cast(), bytes.len()) != size {
        return Err(Error::Format("numeric formatting failed".into()));
    }
    output.write_all(&bytes[..size as usize])?;
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum EscapeMode {
    Format,
    Argument,
    Echo,
}

/// Write a backslash escape after its initial slash. `%b` and echo recognize
/// `\c`; only echo requires a leading zero for octal escapes.
fn escape(
    output: &mut impl Write,
    chars: &mut Peekable<Chars<'_>>,
    mode: EscapeMode,
) -> io::Result<bool> {
    let Some(ch) = chars.next() else {
        output.write_all(b"\\")?;
        return Ok(false);
    };
    let byte = match ch {
        'a' => Some(7),
        'b' => Some(8),
        'e' | 'E' => Some(27),
        'f' => Some(12),
        'n' => Some(b'\n'),
        'r' => Some(b'\r'),
        't' => Some(b'\t'),
        'v' => Some(11),
        '\\' => Some(b'\\'),
        'c' if mode != EscapeMode::Format => return Ok(true),
        _ => None,
    };
    if let Some(byte) = byte {
        output.write_all(&[byte])?;
    } else if ch == '0' || (mode != EscapeMode::Echo && matches!(ch, '1'..='7')) {
        let mut value = ch.to_digit(8).unwrap();
        let remaining = if ch == '0' && mode != EscapeMode::Format {
            3
        } else {
            2
        };
        for _ in 0..remaining {
            let Some(digit) = chars.peek().and_then(|c| c.to_digit(8)) else {
                break;
            };
            chars.next();
            value = value * 8 + digit;
        }
        output.write_all(&[value as u8])?;
    } else if matches!(ch, 'x' | 'u' | 'U') {
        let count = match ch {
            'x' => 2,
            'u' => 4,
            _ => 8,
        };
        let mut value = 0u32;
        let mut digits = String::new();
        for _ in 0..count {
            let Some(digit) = chars.peek().and_then(|c| c.to_digit(16)) else {
                break;
            };
            digits.push(chars.next().unwrap());
            value = value * 16 + digit;
        }
        if !digits.is_empty() && ch == 'x' {
            output.write_all(&[value as u8])?;
        } else if let Some(value) = (!digits.is_empty())
            .then(|| char::from_u32(value))
            .flatten()
        {
            output.write_all(value.encode_utf8(&mut [0u8; 4]).as_bytes())?;
        } else {
            write!(output, "\\{ch}{digits}")?;
        }
    } else {
        write!(output, "\\{ch}")?;
    }
    Ok(false)
}

pub(crate) fn escaped(output: &mut impl Write, value: &str, mode: EscapeMode) -> io::Result<bool> {
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            if escape(output, &mut chars, mode)? {
                return Ok(true);
            }
        } else {
            output.write_all(ch.encode_utf8(&mut [0u8; 4]).as_bytes())?;
        }
    }
    Ok(false)
}

/// Bash's `printf %q` (and `${var@Q}`-style) quoting: the shortest form that
/// reads back as `text`. Empty is `''`; text with a non-printable character
/// uses `$'...'` with C escapes; anything else gets a backslash before each
/// character the shell treats specially.
pub(crate) fn shell_quote(text: &str) -> String {
    if text.is_empty() {
        return "''".to_string();
    }
    if text.chars().any(char::is_control) {
        let mut out = String::from("$'");
        for c in text.chars() {
            match c {
                '\u{1b}' => out.push_str("\\E"),
                '\u{07}' => out.push_str("\\a"),
                '\u{0b}' => out.push_str("\\v"),
                '\u{08}' => out.push_str("\\b"),
                '\u{0c}' => out.push_str("\\f"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                '\\' => out.push_str("\\\\"),
                '\'' => out.push_str("\\'"),
                c if c.is_control() => {
                    // Each byte of the character as a three-digit octal escape.
                    for byte in c.encode_utf8(&mut [0u8; 4]).bytes() {
                        out.push_str(&format!("\\{byte:03o}"));
                    }
                }
                c => out.push(c),
            }
        }
        out.push('\'');
        return out;
    }
    let mut out = String::with_capacity(text.len() + 8);
    let mut previous: Option<char> = None;
    for c in text.chars() {
        let special = matches!(
            c,
            ' ' | '!'
                | '"'
                | '$'
                | '&'
                | '\''
                | '('
                | ')'
                | '*'
                | ','
                | ';'
                | '<'
                | '>'
                | '?'
                | '['
                | '\\'
                | ']'
                | '^'
                | '`'
                | '{'
                | '|'
                | '}'
        ) || (c == '#' && previous.is_none())
            // A tilde expands at the start of a word and after `:` or `=`.
            || (c == '~' && matches!(previous, None | Some(':' | '=')));
        if special {
            out.push('\\');
        }
        out.push(c);
        previous = Some(c);
    }
    out
}

/// strftime `format` for the local time at `seconds` since the epoch, as bash's
/// `%(fmt)T` does (an empty format means `%X`). TZ is re-read first so a
/// `TZ=... printf` prefix assignment takes effect.
fn format_time(format: &str, seconds: i64) -> Result<Vec<u8>, Error> {
    let format = if format.is_empty() { "%X" } else { format };
    let c_format = CString::new(format)
        .map_err(|_| Error::Format("time format contains a NUL byte".into()))?;
    let time = nix::libc::time_t::try_from(seconds)
        .map_err(|_| Error::Format(format!("time out of range: {seconds}")))?;
    // SAFETY: tm is plain old data; localtime_r fills it in completely.
    let mut broken_down: nix::libc::tm = unsafe { std::mem::zeroed() };
    extern "C" {
        // POSIX; not exported by the libc crate. localtime_r (unlike
        // localtime) need not re-read TZ, so call it explicitly.
        fn tzset();
    }
    // SAFETY: tzset only reads the environment; localtime_r writes to the
    // valid `broken_down` and reports failure (an unrepresentable year) as NULL.
    let converted = unsafe {
        tzset();
        nix::libc::localtime_r(&time, &mut broken_down)
    };
    if converted.is_null() {
        return Err(Error::Format(format!("time out of range: {seconds}")));
    }
    // strftime returns 0 both for an empty result and for a buffer that was
    // too small, so grow the buffer (up to the field limit) before accepting
    // an empty answer.
    let mut size = 256usize.max(format.len().saturating_mul(4));
    loop {
        let mut buffer = vec![0u8; size.min(MAX_FIELD)];
        // SAFETY: strftime writes at most buffer.len() bytes into buffer and
        // reads only the NUL-terminated format and the tm filled in above.
        let written = unsafe {
            nix::libc::strftime(
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                c_format.as_ptr(),
                &broken_down,
            )
        };
        if written > 0 || buffer.len() >= MAX_FIELD {
            buffer.truncate(written);
            return Ok(buffer);
        }
        size = size.saturating_mul(4);
    }
}

pub(crate) fn write_printf(
    output: &mut impl Write,
    format: &str,
    args: &[String],
    context: Context,
    mut diagnostic: impl FnMut(String),
) -> Result<i32, Error> {
    let mut index = 0;
    let mut status = 0;
    loop {
        let start = index;
        let mut chars = format.chars().peekable();
        while let Some(ch) = chars.next() {
            match ch {
                '\\' => {
                    if escape(output, &mut chars, EscapeMode::Format)? {
                        return Ok(status);
                    }
                }
                '%' if chars.peek() == Some(&'%') => {
                    chars.next();
                    output.write_all(b"%")?;
                }
                '%' => {
                    let (spec, conversion) = parse_spec(&mut chars, args, &mut index)?;
                    let missing = index >= args.len();
                    let value = argument(args, &mut index);
                    match conversion {
                        's' => text(output, value.as_bytes(), &spec)?,
                        'q' => text(output, shell_quote(value).as_bytes(), &spec)?,
                        'T' => {
                            // -1 (or no argument) is now; -2 is when the shell
                            // started.
                            let seconds = if missing {
                                -1
                            } else {
                                match parse_number(value, 'd') {
                                    (Numeric::Signed(n), NumberStatus::Valid) => n,
                                    _ => {
                                        status = 1;
                                        diagnostic(format!("{value}: invalid number"));
                                        0
                                    }
                                }
                            };
                            let seconds = match seconds {
                                -1 => std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map_or(0, |elapsed| elapsed.as_secs() as i64),
                                -2 => context.shell_start,
                                other => other,
                            };
                            let formatted =
                                format_time(spec.time_format.as_deref().unwrap_or(""), seconds)?;
                            // Like %s: width pads, precision truncates; the `0`
                            // flag does not zero-pad.
                            text(output, &formatted, &spec)?;
                        }
                        'c' => {
                            let mut bytes = [0u8; 4];
                            let first =
                                value.chars().next().unwrap_or('\0').encode_utf8(&mut bytes);
                            // Precision does not truncate a character conversion.
                            text(
                                output,
                                first.as_bytes(),
                                &Spec {
                                    precision: None,
                                    ..spec
                                },
                            )?;
                        }
                        'b' => {
                            let mut bytes = Vec::new();
                            let stop = escaped(&mut bytes, value, EscapeMode::Argument)?;
                            text(output, &bytes, &spec)?;
                            if stop {
                                return Ok(status);
                            }
                        }
                        _ => {
                            let (number, number_status) = parse_number(value, conversion);
                            match number_status {
                                NumberStatus::Invalid => {
                                    status = 1;
                                    diagnostic(format!("invalid numeric argument: {value:?}"));
                                }
                                NumberStatus::OutOfRange => diagnostic(format!(
                                    "warning: numeric argument out of range: {value:?}"
                                )),
                                NumberStatus::Valid => {}
                            }
                            numeric(output, &spec, conversion, number)?;
                        }
                    }
                }
                _ => output.write_all(ch.encode_utf8(&mut [0u8; 4]).as_bytes())?,
            }
        }
        if index == start || index >= args.len() {
            return Ok(status);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct BrokenWriter;
    impl Write for BrokenWriter {
        fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "closed test pipe",
            ))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    const CONTEXT: Context = Context { shell_start: 0 };

    #[test]
    fn output_errors_are_propagated_for_text_numbers_and_escapes() {
        for format in ["text", "%d", "%b", r"\n"] {
            let result = write_printf(&mut BrokenWriter, format, &["1".into()], CONTEXT, |_| {});
            assert!(
                matches!(result, Err(Error::Io(error)) if error.kind() == io::ErrorKind::BrokenPipe)
            );
        }
    }

    fn render(format: &str, args: &[&str]) -> (String, i32) {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let mut out = Vec::new();
        let status = write_printf(&mut out, format, &args, CONTEXT, |_| {}).unwrap();
        (String::from_utf8(out).unwrap(), status)
    }

    /// Expected values are bash 5.2's `printf %q` output (UTF-8 locale).
    #[test]
    fn q_quotes_like_bash() {
        let cases = [
            ("a b", "a\\ b"),
            ("it's", "it\\'s"),
            ("$x", "\\$x"),
            ("", "''"),
            ("plain", "plain"),
            ("tab\there", "$'tab\\there'"),
            ("é", "é"),
            ("a,b", "a\\,b"),
            ("~x", "\\~x"),
            ("x~y", "x~y"),
            ("a=~b", "a=\\~b"),
            ("a:~b", "a:\\~b"),
            ("#c", "\\#c"),
            ("c#", "c#"),
            ("!x", "\\!x"),
            ("%^_+-./:=@", "%\\^_+-./:=@"),
            ("\u{1}\u{7f}", "$'\\001\\177'"),
            ("nl\n", "$'nl\\n'"),
            ("a'b c", "a\\'b\\ c"),
            (
                "[]{}|&;<>()*?\\`\"",
                "\\[\\]\\{\\}\\|\\&\\;\\<\\>\\(\\)\\*\\?\\\\\\`\\\"",
            ),
            ("\u{1b}\u{7}\u{b}\u{8}\u{c}\r", "$'\\E\\a\\v\\b\\f\\r'"),
            ("日本", "日本"),
        ];
        for (input, expected) in cases {
            assert_eq!(shell_quote(input), expected, "{input:?}");
        }
        assert_eq!(
            render("[%10q][%-6q][%.3q]", &["a b", "x", "abcdef"]).0,
            "[      a\\ b][x     ][abc]"
        );
    }

    #[test]
    fn time_conversion_formats_epoch_seconds() {
        // Every test that reads the local time zone sets it to UTC.
        std::env::set_var("TZ", "UTC");
        assert_eq!(
            render("%(%Y-%m-%d %H:%M:%S)T", &["86400"]).0,
            "1970-01-02 00:00:00"
        );
        assert_eq!(
            render("[%12(%H:%M)T][%-8(%H)T][%.2(%Y)T]", &["3600", "3600", "0"]).0,
            "[       01:00][01      ][19]"
        );
        assert_eq!(render("%()T", &["0"]).0, "00:00:00");
        assert_eq!(render("%(%s)T", &["-2"]).0, "0");
        assert_eq!(render("%(%Y)T", &["abc"]), ("1970".to_string(), 1));
        let (now, _) = render("%(%Y)T", &[]);
        assert!(now.parse::<u32>().unwrap() >= 2024, "{now}");
        assert!(matches!(
            write_printf(&mut Vec::new(), "%(%Y", &[], CONTEXT, |_| {}),
            Err(Error::Format(_))
        ));
    }
}
