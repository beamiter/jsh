//! Ranking: which candidates match what was typed, and in what order.
//!
//! Two rules run the whole list. A prefix match keeps the order its source
//! chose, because a curated list of subcommands is already in a useful
//! order and alphabetising it would lose that. Only when nothing starts
//! with the typed text does fuzzy subsequence matching take over, so
//! precise typing is never reordered by a guess.

use super::Completion;

pub fn common_prefix(completions: &[Completion]) -> String {
    let Some(first) = completions.first() else {
        return String::new();
    };
    let mut prefix: Vec<char> = first.text.chars().collect();
    for completion in &completions[1..] {
        let common_chars = prefix
            .iter()
            .copied()
            .zip(completion.text.chars())
            .take_while(|(left, right)| left == right)
            .count();
        prefix.truncate(common_chars);
        if prefix.is_empty() {
            break;
        }
    }
    prefix.into_iter().collect()
}

/// Fuzzy match score: higher is better
/// 精确前缀匹配最高分，然后是首字母匹配，最后是子字符串匹配
pub fn fuzzy_match_score(text: &str, pattern: &str) -> i32 {
    fuzzy_match_score_lowered(text, &lowered(pattern))
}

/// Lowercase only when something is actually uppercase. Candidate lists are
/// mostly lowercase command, file and branch names, and this runs once per
/// candidate per keystroke — the allocation is the cost, not the comparison.
pub(super) fn lowered(text: &str) -> std::borrow::Cow<'_, str> {
    if text.bytes().any(|byte| byte.is_ascii_uppercase()) || !text.is_ascii() {
        std::borrow::Cow::Owned(text.to_lowercase())
    } else {
        std::borrow::Cow::Borrowed(text)
    }
}

/// [`fuzzy_match_score`] with the pattern already lowercased, so a ranking
/// pass over many candidates lowercases it once rather than once per item.
pub(super) fn fuzzy_match_score_lowered(text: &str, pattern_lower: &str) -> i32 {
    if pattern_lower.is_empty() {
        return 1000; // Empty pattern matches everything with high score
    }

    let text_lower = lowered(text);

    // Exact prefix match: highest score
    if text_lower.starts_with(pattern_lower) {
        return 1000 - (text_lower.len() as i32 - pattern_lower.len() as i32).abs();
    }

    // Check if all characters of pattern exist in text in order
    let mut pattern_chars = pattern_lower.chars().peekable();
    let mut last_match_pos = 0;
    let mut match_count = 0;
    let mut gap_penalty = 0;
    let mut previous_matched = false;

    for (pos, text_char) in text_lower.chars().enumerate() {
        let Some(&pattern_char) = pattern_chars.peek() else {
            break;
        };
        if text_char != pattern_char {
            previous_matched = false;
            continue;
        }
        pattern_chars.next();
        match_count += 1;

        // Penalty for gaps between matches
        gap_penalty += pos.saturating_sub(last_match_pos).saturating_sub(1) as i32;
        last_match_pos = pos;

        // Bonus for a run: this position matched and so did the one before,
        // which is what makes `chk` prefer checkout over cherry-pick.
        if previous_matched {
            gap_penalty = gap_penalty.saturating_sub(5);
        }
        previous_matched = true;
    }

    if match_count == pattern_lower.chars().count() {
        // All characters matched, score based on gaps and position
        500 + (match_count as i32 * 10) - gap_penalty
    } else {
        0 // No match
    }
}

/// Move the candidates this command has had accepted before to the front,
/// highest frecency first, keeping every other candidate in its existing
/// order behind them.
///
/// A stable partition, not a re-sort: the ranking a source chose still holds
/// for everything that has no history, and nothing is added or dropped.
pub(super) fn promote_accepted(completions: Vec<Completion>, cmd: &str) -> Vec<Completion> {
    if cmd.is_empty() || completions.len() < 2 {
        return completions;
    }
    let Ok(db) = crate::accepted::get_accepted_db().lock() else {
        return completions;
    };
    let scores = db.scores_for(cmd);
    if scores.is_empty() {
        return completions;
    }
    let mut accepted: Vec<(f64, Completion)> = Vec::new();
    let mut rest = Vec::with_capacity(completions.len());
    for completion in completions {
        match scores.get(completion.text.as_str()) {
            Some(score) => accepted.push((*score, completion)),
            None => rest.push(completion),
        }
    }
    accepted.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    let mut ranked: Vec<Completion> = accepted.into_iter().map(|(_, item)| item).collect();
    ranked.extend(rest);
    ranked
}

/// Which characters of `text` the typed `pattern` matched, as byte offsets.
///
/// The same subsequence walk the score uses, so what a menu underlines is
/// exactly what made a candidate rank where it did. An empty pattern matches
/// nothing to highlight, and text the pattern does not match at all yields
/// no offsets rather than a partial run.
pub fn match_positions(text: &str, pattern: &str) -> Vec<usize> {
    if pattern.is_empty() {
        return Vec::new();
    }
    // Offsets must index the original text, and lowercasing can change a
    // character's length, so the walk stays on the original and folds case
    // one character at a time.
    let fold = |ch: char| ch.to_lowercase().next().unwrap_or(ch);
    let mut pattern_chars = pattern.chars().map(fold).peekable();
    let mut positions = Vec::new();
    for (offset, ch) in text.char_indices() {
        let Some(&wanted) = pattern_chars.peek() else {
            break;
        };
        if fold(ch) == wanted {
            pattern_chars.next();
            positions.push(offset);
        }
    }
    if pattern_chars.peek().is_some() {
        return Vec::new();
    }
    positions
}

/// Prefix matches in their source order when any exist; otherwise
/// fuzzy-ranked subsequence matches, so `git chk<TAB>` still finds checkout
/// without prefix matches losing their curated order.
pub(super) fn rank_prefix_then_fuzzy(
    completions: Vec<Completion>,
    pattern: &str,
) -> Vec<Completion> {
    if pattern.is_empty() || completions.iter().any(|c| c.text.starts_with(pattern)) {
        completions
            .into_iter()
            .filter(|c| c.text.starts_with(pattern))
            .collect()
    } else {
        filter_completions(completions, pattern)
    }
}

/// Filter completions using fuzzy matching
pub fn filter_completions(completions: Vec<Completion>, pattern: &str) -> Vec<Completion> {
    let pattern_lower = lowered(pattern);
    let mut scored: Vec<(Completion, i32)> = completions
        .into_iter()
        .map(|c| {
            let score = fuzzy_match_score_lowered(&c.text, &pattern_lower);
            (c, score)
        })
        .filter(|(_, score)| *score > 0)
        .collect();

    // Sort by score descending, then by text length (shorter is better)
    scored.sort_by(|a, b| {
        let score_cmp = b.1.cmp(&a.1);
        if score_cmp == std::cmp::Ordering::Equal {
            a.0.text.len().cmp(&b.0.text.len())
        } else {
            score_cmp
        }
    });

    scored.into_iter().map(|(c, _)| c).collect()
}
