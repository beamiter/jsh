//! Shared glob pattern matching using an iterative algorithm.
//! Replaces the three duplicate recursive implementations.

/// Match a value against a glob pattern supporting `*`, `?`, and `[...]`.
pub fn glob_match(pattern: &str, value: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if pattern == value {
        return true;
    }
    let p: Vec<char> = pattern.chars().collect();
    let v: Vec<char> = value.chars().collect();
    glob_match_iter(&p, &v)
}

/// Parse a `[...]` character class starting at `pattern[pi]` (which must be `[`).
/// Returns `Some((negated, ranges))` where each range is `(start, end)` inclusive,
/// and advances `*pi` past the closing `]`.
/// Returns `None` if there is no closing `]` (treat `[` as literal).
fn match_char_class(pattern: &[char], pi: &mut usize) -> Option<(bool, Vec<(char, char)>)> {
    let start = *pi;
    // Must start with '['
    if pattern[start] != '[' {
        return None;
    }
    let mut i = start + 1;
    if i >= pattern.len() {
        return None;
    }

    // Check for negation
    let negated = pattern[i] == '!' || pattern[i] == '^';
    if negated {
        i += 1;
    }

    // If ']' appears right after '[' or '[!' / '[^', treat it as a literal char in the class
    let mut ranges: Vec<(char, char)> = Vec::new();
    let mut first = true;
    while i < pattern.len() {
        let c = pattern[i];
        if c == ']' && !first {
            // Found closing bracket
            *pi = i + 1; // advance past ']'
            return Some((negated, ranges));
        }
        first = false;
        // Check for range: a-z
        if i + 2 < pattern.len() && pattern[i + 1] == '-' && pattern[i + 2] != ']' {
            let range_start = c;
            let range_end = pattern[i + 2];
            ranges.push((range_start, range_end));
            i += 3;
        } else {
            ranges.push((c, c));
            i += 1;
        }
    }

    // No closing ']' found -- treat '[' as literal
    None
}

/// Check if a character matches a parsed character class.
fn char_in_class(ch: char, negated: bool, ranges: &[(char, char)]) -> bool {
    let mut found = false;
    for &(lo, hi) in ranges {
        let (lo, hi) = if lo <= hi { (lo, hi) } else { (hi, lo) };
        if ch >= lo && ch <= hi {
            found = true;
            break;
        }
    }
    if negated {
        !found
    } else {
        found
    }
}

/// Iterative two-pointer glob matching (O(n*m) worst case, no stack overflow).
/// Supports `*`, `?`, and `[...]` character classes.
fn glob_match_iter(pattern: &[char], value: &[char]) -> bool {
    let mut pi = 0;
    let mut vi = 0;
    let mut star_pi: Option<usize> = None;
    let mut star_vi = 0;

    while vi < value.len() {
        if pi < pattern.len() && pattern[pi] == '[' {
            let mut tmp_pi = pi;
            if let Some((negated, ranges)) = match_char_class(pattern, &mut tmp_pi) {
                if char_in_class(value[vi], negated, &ranges) {
                    pi = tmp_pi;
                    vi += 1;
                    continue;
                }
                // Didn't match the class -- try star backtrack
                if let Some(sp) = star_pi {
                    pi = sp + 1;
                    star_vi += 1;
                    vi = star_vi;
                    continue;
                }
                return false;
            }
            // No closing ']' -- treat '[' as literal
            if pattern[pi] == value[vi] {
                pi += 1;
                vi += 1;
            } else if let Some(sp) = star_pi {
                pi = sp + 1;
                star_vi += 1;
                vi = star_vi;
            } else {
                return false;
            }
        } else if pi < pattern.len() && (pattern[pi] == '?' || pattern[pi] == value[vi]) {
            pi += 1;
            vi += 1;
        } else if pi < pattern.len() && pattern[pi] == '*' {
            star_pi = Some(pi);
            star_vi = vi;
            pi += 1;
        } else if let Some(sp) = star_pi {
            pi = sp + 1;
            star_vi += 1;
            vi = star_vi;
        } else {
            return false;
        }
    }

    // Consume trailing stars
    while pi < pattern.len() && pattern[pi] == '*' {
        pi += 1;
    }

    pi == pattern.len()
}

// ============================================================
// Extended glob (extglob) pattern matching
// Supports: !(pat), ?(pat), *(pat), +(pat), @(pat)
// ============================================================

/// Check if a pattern contains extglob syntax
pub fn contains_extglob(pattern: &str) -> bool {
    let mut escaped = false;
    let mut i = 0;
    let chars: Vec<char> = pattern.chars().collect();

    while i < chars.len() {
        if escaped {
            escaped = false;
            i += 1;
            continue;
        }

        if chars[i] == '\\' {
            escaped = true;
            i += 1;
            continue;
        }

        // Check for extglob patterns: !(, ?(, *(, +(, @(
        if i + 1 < chars.len() && chars[i + 1] == '(' {
            match chars[i] {
                '!' | '?' | '*' | '+' | '@' => return true,
                _ => {}
            }
        }

        i += 1;
    }

    false
}

/// Match `value` against a shell pattern the way `case`, `[[ == ]]` and
/// `${var#pat}` do: `@(a|b)` and friends are patterns only while
/// `shopt -s extglob` is on, and plain glob syntax otherwise.
pub fn pattern_match(pattern: &str, value: &str, extglob: bool) -> bool {
    if extglob && contains_extglob(pattern) {
        extglob_match(pattern, value)
    } else {
        glob_match(pattern, value)
    }
}

/// Match `value` against an extended-glob `pattern`, anchored at both ends.
///
/// The extended forms are `?(a|b)` (zero or one), `*(a|b)` (zero or more),
/// `+(a|b)` (one or more), `@(a|b)` (exactly one) and `!(a|b)` (anything that
/// is not one of them). Every one of them can be followed by more pattern, so
/// matching has to backtrack over how much of the value the group consumed:
/// `!(no-*)dir*` matches `nodir` only if `!(no-*)` settles on `no` and leaves
/// `dir` for the rest of the pattern.
pub fn extglob_match(pattern: &str, value: &str) -> bool {
    // A pattern with no group is a plain glob; the iterative matcher answers it
    // without the recursion this one needs.
    if !contains_extglob(pattern) {
        return glob_match(pattern, value);
    }
    let p: Vec<char> = pattern.chars().collect();
    let v: Vec<char> = value.chars().collect();
    match_from(&p, 0, &v, 0)
}

/// True when `p[pi..]` matches all of `v[vi..]`.
fn match_from(p: &[char], pi: usize, v: &[char], vi: usize) -> bool {
    if pi >= p.len() {
        return vi >= v.len();
    }

    if let Some((kind, alts, end)) = parse_extglob_group(p, pi) {
        return match_group(kind, &alts, p, end, v, vi);
    }

    match p[pi] {
        '\\' if pi + 1 < p.len() => {
            if vi < v.len() && v[vi] == p[pi + 1] {
                match_from(p, pi + 2, v, vi + 1)
            } else {
                false
            }
        }
        '*' => {
            // Every split point, shortest first.
            (vi..=v.len()).any(|k| match_from(p, pi + 1, v, k))
        }
        '?' => vi < v.len() && match_from(p, pi + 1, v, vi + 1),
        '[' => {
            let mut after = pi;
            match match_char_class(p, &mut after) {
                Some((negated, ranges)) => {
                    vi < v.len()
                        && char_in_class(v[vi], negated, &ranges)
                        && match_from(p, after, v, vi + 1)
                }
                // An unclosed `[` is a literal, as in a plain glob.
                None => vi < v.len() && v[vi] == '[' && match_from(p, pi + 1, v, vi + 1),
            }
        }
        c => vi < v.len() && v[vi] == c && match_from(p, pi + 1, v, vi + 1),
    }
}

/// The five group operators, by their leading character.
#[derive(Clone, Copy, PartialEq, Eq)]
enum GroupKind {
    ZeroOrOne,  // ?(...)
    ZeroOrMore, // *(...)
    OneOrMore,  // +(...)
    ExactlyOne, // @(...)
    Not,        // !(...)
}

/// Match one group at `v[vi..]`, then `p[end..]` against what is left.
fn match_group(
    kind: GroupKind,
    alts: &[Vec<char>],
    p: &[char],
    end: usize,
    v: &[char],
    vi: usize,
) -> bool {
    match kind {
        GroupKind::Not => {
            // Anything the alternatives do NOT match, of any length.
            (vi..=v.len()).any(|k| !alts_match(alts, &v[vi..k]) && match_from(p, end, v, k))
        }
        GroupKind::ExactlyOne => {
            (vi..=v.len()).any(|k| alts_match(alts, &v[vi..k]) && match_from(p, end, v, k))
        }
        GroupKind::ZeroOrOne => {
            match_from(p, end, v, vi)
                || (vi..=v.len()).any(|k| alts_match(alts, &v[vi..k]) && match_from(p, end, v, k))
        }
        GroupKind::ZeroOrMore => match_repeat(alts, p, end, v, vi, true),
        GroupKind::OneOrMore => match_repeat(alts, p, end, v, vi, false),
    }
}

/// Match zero (or one, when `allow_empty` is false) or more repetitions of
/// `alts`, then the rest of the pattern. Each repetition must consume at least
/// one character, which is what keeps `*(a|)` from looping forever.
fn match_repeat(
    alts: &[Vec<char>],
    p: &[char],
    end: usize,
    v: &[char],
    vi: usize,
    allow_empty: bool,
) -> bool {
    if allow_empty && match_from(p, end, v, vi) {
        return true;
    }
    (vi + 1..=v.len())
        .any(|k| alts_match(alts, &v[vi..k]) && match_repeat(alts, p, end, v, k, true))
}

/// True when one alternative matches the whole of `value`.
fn alts_match(alts: &[Vec<char>], value: &[char]) -> bool {
    alts.iter().any(|alt| match_from(alt, 0, value, 0))
}

/// Parse the extended-glob group starting at `pattern[start]`, if there is one.
/// Returns its kind, its `|`-separated alternatives and the index just past the
/// closing paren. Alternatives keep their own metacharacters — including nested
/// groups — because they are matched by the same function that parsed them.
fn parse_extglob_group(
    pattern: &[char],
    start: usize,
) -> Option<(GroupKind, Vec<Vec<char>>, usize)> {
    let kind = match pattern.get(start)? {
        '?' => GroupKind::ZeroOrOne,
        '*' => GroupKind::ZeroOrMore,
        '+' => GroupKind::OneOrMore,
        '@' => GroupKind::ExactlyOne,
        '!' => GroupKind::Not,
        _ => return None,
    };
    if pattern.get(start + 1) != Some(&'(') {
        return None;
    }

    let mut alts = Vec::new();
    let mut current = Vec::new();
    let mut depth = 1usize;
    let mut i = start + 2;
    while i < pattern.len() {
        let c = pattern[i];
        match c {
            '\\' if i + 1 < pattern.len() => {
                current.push(c);
                current.push(pattern[i + 1]);
                i += 1;
            }
            '(' => {
                depth += 1;
                current.push(c);
            }
            ')' => {
                depth -= 1;
                if depth == 0 {
                    alts.push(current);
                    return Some((kind, alts, i + 1));
                }
                current.push(c);
            }
            '|' if depth == 1 => {
                alts.push(std::mem::take(&mut current));
            }
            _ => current.push(c),
        }
        i += 1;
    }

    None // Unclosed group: not an extended glob after all.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_exact() {
        assert!(glob_match("hello", "hello"));
        assert!(!glob_match("hello", "world"));
    }

    #[test]
    fn test_star() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("he*", "hello"));
        assert!(glob_match("*lo", "hello"));
        assert!(glob_match("h*l*o", "hello"));
        assert!(!glob_match("h*x", "hello"));
    }

    #[test]
    fn test_question() {
        assert!(glob_match("h?llo", "hello"));
        assert!(!glob_match("h?llo", "hllo"));
    }

    #[test]
    fn test_empty() {
        assert!(glob_match("", ""));
        assert!(glob_match("*", ""));
        assert!(!glob_match("?", ""));
    }

    #[test]
    fn test_many_stars_no_exponential() {
        // This would blow up with naive recursion
        let pat = "a*a*a*a*a*a*a*a*b";
        let val = "aaaaaaaaaaaaaaaa";
        assert!(!glob_match(pat, val));
    }

    // Character class tests
    #[test]
    fn test_char_class_basic() {
        assert!(glob_match("[abc]", "a"));
        assert!(glob_match("[abc]", "b"));
        assert!(glob_match("[abc]", "c"));
        assert!(!glob_match("[abc]", "d"));
        assert!(!glob_match("[abc]", ""));
    }

    #[test]
    fn test_char_class_range() {
        assert!(glob_match("[a-z]", "a"));
        assert!(glob_match("[a-z]", "m"));
        assert!(glob_match("[a-z]", "z"));
        assert!(!glob_match("[a-z]", "A"));
        assert!(!glob_match("[a-z]", "0"));
    }

    #[test]
    fn test_char_class_negation() {
        assert!(!glob_match("[!abc]", "a"));
        assert!(glob_match("[!abc]", "d"));
        assert!(!glob_match("[^abc]", "b"));
        assert!(glob_match("[^abc]", "x"));
    }

    #[test]
    fn test_char_class_negated_range() {
        assert!(!glob_match("[!a-z]", "m"));
        assert!(glob_match("[!a-z]", "M"));
        assert!(glob_match("[!a-z]", "5"));
    }

    #[test]
    fn test_char_class_with_wildcards() {
        assert!(glob_match("*.[ch]", "foo.c"));
        assert!(glob_match("*.[ch]", "bar.h"));
        assert!(!glob_match("*.[ch]", "baz.o"));
        assert!(glob_match("[abc]*", "alpha"));
        assert!(!glob_match("[abc]*", "delta"));
        assert!(glob_match("?[0-9]?", "a5b"));
        assert!(!glob_match("?[0-9]?", "abc"));
    }

    #[test]
    fn test_char_class_unclosed_bracket() {
        // Unclosed '[' should be treated as a literal
        assert!(glob_match("[", "["));
        assert!(!glob_match("[", "a"));
        assert!(glob_match("[abc", "[abc"));
    }

    #[test]
    fn test_char_class_bracket_as_member() {
        // ']' right after '[' is treated as literal member
        assert!(glob_match("[]abc]", "]"));
        assert!(glob_match("[]abc]", "a"));
    }

    #[test]
    fn test_char_class_multiple_ranges() {
        assert!(glob_match("[a-zA-Z0-9]", "g"));
        assert!(glob_match("[a-zA-Z0-9]", "G"));
        assert!(glob_match("[a-zA-Z0-9]", "5"));
        assert!(!glob_match("[a-zA-Z0-9]", "!"));
    }

    #[test]
    fn test_char_class_in_complex_pattern() {
        assert!(glob_match("file[0-9].txt", "file3.txt"));
        assert!(!glob_match("file[0-9].txt", "fileA.txt"));
        assert!(glob_match("*[!.]*", "hello"));
        assert!(glob_match("[a-z][a-z][a-z]", "abc"));
        assert!(!glob_match("[a-z][a-z][a-z]", "ab1"));
    }

    #[test]
    fn test_extglob_basic() {
        // Test contains_extglob detection
        assert!(contains_extglob("!(pattern)"));
        assert!(contains_extglob("?(pattern)"));
        assert!(contains_extglob("*(pattern)"));
        assert!(contains_extglob("+(pattern)"));
        assert!(contains_extglob("@(pattern)"));
        assert!(!contains_extglob("*pattern"));
        assert!(!contains_extglob("?pattern"));
        assert!(!contains_extglob("[pattern]"));
    }

    #[test]
    fn test_extglob_negation() {
        // !(pat): match anything NOT matching pat
        assert!(extglob_match("!(test)", "hello"));
        assert!(extglob_match("!(test)", "foo"));
        assert!(!extglob_match("!(test)", "test"));
        assert!(extglob_match("!(*.txt)", "file.rs"));
        assert!(!extglob_match("!(*.txt)", "file.txt"));
    }

    #[test]
    fn test_extglob_optional() {
        // ?(pat): match 0 or 1 occurrence
        assert!(extglob_match("?(test)", ""));
        assert!(extglob_match("?(test)", "test"));
        assert!(!extglob_match("?(test)", "testtest"));
    }

    #[test]
    fn test_extglob_zero_or_more() {
        // *(pat): match 0 or more occurrences
        assert!(extglob_match("*(test)", ""));
        assert!(extglob_match("*(test)", "test"));
        assert!(extglob_match("*(test)", "testtest"));
        assert!(extglob_match("*(test)", "testtesttest"));
    }

    #[test]
    fn test_extglob_one_or_more() {
        // +(pat): match 1 or more occurrences
        assert!(!extglob_match("+(test)", ""));
        assert!(extglob_match("+(test)", "test"));
        assert!(extglob_match("+(test)", "testtest"));
        assert!(extglob_match("+(test)", "testtesttest"));
    }

    #[test]
    fn test_extglob_exactly_one() {
        // @(pat): match exactly one pattern
        assert!(!extglob_match("@(foo|bar)", "foobar"));
        assert!(extglob_match("@(foo|bar)", "foo"));
        assert!(extglob_match("@(foo|bar)", "bar"));
        assert!(!extglob_match("@(foo|bar)", "baz"));
    }

    /// A group is rarely the whole pattern. Matching it used to consume nothing
    /// (`!(...)`) or the first alternative that fit (`@(...)`), so anything
    /// after the group could not match — the shape bash-completion is built on.
    #[test]
    fn test_extglob_group_followed_by_more_pattern() {
        assert!(extglob_match("!(x)bc", "abc"));
        assert!(extglob_match("!(no-*)dir*", "nodir"));
        assert!(extglob_match("--!(no-*)dir*", "--nodir"));
        assert!(!extglob_match("--!(no-*)dir*", "--no-fdir"));
        assert!(extglob_match("@(a|ab)c", "abc"));
        assert!(extglob_match("+(a|b)c", "aabc"));
        assert!(extglob_match("?(a)bc", "bc"));
        assert!(extglob_match("*(ab)c", "ababc"));
        assert!(!extglob_match("*(ab)c", "abax"));
        // Groups nest, and the value after them still has to line up.
        assert!(extglob_match("@(a|!(b))z", "cz"));
        assert!(extglob_match("-?(\\[)+([a-zA-Z0-9?])", "-abc"));
        assert!(extglob_match("-?(\\[)+([a-zA-Z0-9?])", "-[a"));
    }

    #[test]
    fn test_extglob_alternatives_carry_glob_syntax() {
        assert!(extglob_match("@(Linux|GNU/*)", "Linux"));
        assert!(extglob_match("@(Linux|GNU/*)", "GNU/kFreeBSD"));
        assert!(!extglob_match("@(Linux|GNU/*)", "Darwin"));
        assert!(extglob_match("*@(solaris|aix)*", "solaris2.11"));
        assert!(!extglob_match("*@(solaris|aix)*", "linux-gnu"));
        assert!(extglob_match("!(*.txt)", "file.rs"));
        assert!(!extglob_match("!(*.txt)", "file.txt"));
    }

    /// `pattern_match` is the gate `shopt -s extglob` controls: with the option
    /// off, `@(a|b)` is not a group at all.
    #[test]
    fn test_pattern_match_honours_extglob_option() {
        assert!(pattern_match("@(foo|bar)", "foo", true));
        assert!(!pattern_match("@(foo|bar)", "foo", false));
        assert!(pattern_match("@(foo|bar)", "@(foo|bar)", false));
        // Plain globs behave the same either way.
        assert!(pattern_match("*.txt", "a.txt", true));
        assert!(pattern_match("*.txt", "a.txt", false));
    }
}
