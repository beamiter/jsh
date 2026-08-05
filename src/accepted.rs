//! What completions were actually taken, so the next list leads with them.
//!
//! A completion list is ranked by how well a candidate matches the typed
//! text, which says nothing about whether it is the one this person wants.
//! Two branches share a prefix, twenty units start with `systemd-`, and the
//! ordering that results is alphabetical accident. What settles it is the
//! record of which candidate was accepted before, in this same position.
//!
//! The record is deliberately small and local:
//!
//!   * the key is the command plus the candidate — `git` and `checkout`, not
//!     the whole command line, so the same choice counts wherever the line
//!     went afterwards;
//!   * the value is frecency, the same rank-and-decay `z` uses for
//!     directories, so a habit that stops being one fades instead of ruling
//!     forever;
//!   * nothing is recorded for a candidate accepted with no command context
//!     (completing the command name itself), because there the typed prefix
//!     already decides and a learned order would fight it.
//!
//! It lives in `~/.jsh_completions` with the same private-file handling as
//! the history and `z` databases, and it is a ranking hint only: a candidate
//! is never added to a list because of it, and never removed from one.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_ACCEPTED_FILE_BYTES: usize = 1024 * 1024;
/// Enough to cover the commands a person actually uses, bounded so the file
/// cannot grow without limit. The lowest-scoring entries go first.
const MAX_ACCEPTED_ENTRIES: usize = 2000;
const MAX_ACCEPTED_TEXT_BYTES: usize = 512;

#[derive(Debug, Clone)]
struct AcceptedEntry {
    command: String,
    candidate: String,
    rank: f64,
    last_used: u64,
}

#[derive(Debug, Default)]
pub struct AcceptedDb {
    entries: Vec<AcceptedEntry>,
    file_path: PathBuf,
    dirty: bool,
}

impl AcceptedDb {
    fn load_default() -> Self {
        let file_path = dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join(".jsh_completions");
        let mut db = AcceptedDb {
            entries: Vec::new(),
            file_path,
            dirty: false,
        };
        db.load();
        db
    }

    fn load(&mut self) {
        let content =
            match crate::io_guard::read_private_text(&self.file_path, MAX_ACCEPTED_FILE_BYTES) {
                Ok(content) => content,
                Err(_) => return,
            };
        for line in content.lines().take(MAX_ACCEPTED_ENTRIES) {
            // command<TAB>candidate<TAB>rank<TAB>last_used
            let parts: Vec<&str> = line.splitn(4, '\t').collect();
            if parts.len() != 4 {
                continue;
            }
            let (Ok(rank), Ok(last_used)) = (parts[2].parse::<f64>(), parts[3].parse::<u64>())
            else {
                continue;
            };
            if !rank.is_finite() || rank <= 0.0 || parts[0].is_empty() || parts[1].is_empty() {
                continue;
            }
            if parts[0].len() > MAX_ACCEPTED_TEXT_BYTES || parts[1].len() > MAX_ACCEPTED_TEXT_BYTES
            {
                continue;
            }
            self.entries.push(AcceptedEntry {
                command: parts[0].to_string(),
                candidate: parts[1].to_string(),
                rank,
                last_used,
            });
        }
    }

    /// Record that `candidate` was accepted while completing an argument of
    /// `command`. A candidate carrying a tab or a newline is not recorded
    /// rather than corrupting the line-oriented file.
    pub fn record(&mut self, command: &str, candidate: &str) {
        if command.is_empty()
            || candidate.is_empty()
            || command.len() > MAX_ACCEPTED_TEXT_BYTES
            || candidate.len() > MAX_ACCEPTED_TEXT_BYTES
            || [command, candidate]
                .iter()
                .any(|text| text.contains('\t') || text.contains('\n'))
        {
            return;
        }
        let now = now_secs();
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.command == command && entry.candidate == candidate)
        {
            entry.rank += 1.0;
            entry.last_used = now;
        } else {
            self.entries.push(AcceptedEntry {
                command: command.to_string(),
                candidate: candidate.to_string(),
                rank: 1.0,
                last_used: now,
            });
        }
        self.dirty = true;
        if self.entries.len() > MAX_ACCEPTED_ENTRIES {
            self.prune();
        }
    }

    fn prune(&mut self) {
        let now = now_secs();
        self.entries.sort_by(|a, b| {
            frecency(b, now)
                .partial_cmp(&frecency(a, now))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        self.entries.truncate(MAX_ACCEPTED_ENTRIES * 3 / 4);
    }

    /// Frecency score for one candidate of one command, or `None` when this
    /// pair has never been accepted.
    pub fn score(&self, command: &str, candidate: &str) -> Option<f64> {
        let now = now_secs();
        self.entries
            .iter()
            .find(|entry| entry.command == command && entry.candidate == candidate)
            .map(|entry| frecency(entry, now))
    }

    /// Every recorded candidate for one command, as a lookup for ranking a
    /// whole list without a scan per item.
    pub fn scores_for(&self, command: &str) -> HashMap<&str, f64> {
        let now = now_secs();
        self.entries
            .iter()
            .filter(|entry| entry.command == command)
            .map(|entry| (entry.candidate.as_str(), frecency(entry, now)))
            .collect()
    }

    pub fn save(&mut self) {
        if !self.dirty {
            return;
        }
        let body: String = self
            .entries
            .iter()
            .map(|entry| {
                format!(
                    "{}\t{}\t{:.3}\t{}\n",
                    entry.command, entry.candidate, entry.rank, entry.last_used
                )
            })
            .collect();
        if crate::io_guard::write_private_file_atomic(
            &self.file_path,
            body.as_bytes(),
            MAX_ACCEPTED_FILE_BYTES,
        )
        .is_ok()
        {
            self.dirty = false;
        }
    }
}

/// The same shape `z` uses: a count that decays with age, so a habit that
/// stops being one stops ranking.
fn frecency(entry: &AcceptedEntry, now: u64) -> f64 {
    let age = now.saturating_sub(entry.last_used);
    let weight = if age < 3600 {
        4.0
    } else if age < 86_400 {
        2.0
    } else if age < 604_800 {
        1.0
    } else {
        0.5
    };
    entry.rank * weight
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn get_accepted_db() -> &'static Mutex<AcceptedDb> {
    static DB: OnceLock<Mutex<AcceptedDb>> = OnceLock::new();
    DB.get_or_init(|| Mutex::new(AcceptedDb::load_default()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_db() -> AcceptedDb {
        AcceptedDb {
            entries: Vec::new(),
            file_path: PathBuf::from("/nonexistent"),
            dirty: false,
        }
    }

    #[test]
    fn accepting_a_candidate_raises_it_for_that_command_only() {
        let mut db = empty_db();
        db.record("git", "checkout");
        db.record("git", "checkout");
        db.record("git", "commit");

        let checkout = db.score("git", "checkout").unwrap();
        let commit = db.score("git", "commit").unwrap();
        assert!(checkout > commit, "{checkout} vs {commit}");

        // The same candidate under a different command is a different habit.
        assert!(db.score("cargo", "checkout").is_none());
        assert!(db.score("git", "never-taken").is_none());
    }

    #[test]
    fn scores_decay_so_an_old_habit_stops_ruling() {
        let now = now_secs();
        let recent = AcceptedEntry {
            command: "git".to_string(),
            candidate: "fresh".to_string(),
            rank: 2.0,
            last_used: now,
        };
        let ancient = AcceptedEntry {
            command: "git".to_string(),
            candidate: "stale".to_string(),
            // Taken three times as often, but a month ago.
            rank: 6.0,
            last_used: now.saturating_sub(30 * 86_400),
        };
        assert!(frecency(&recent, now) > frecency(&ancient, now));
    }

    #[test]
    fn entries_that_would_corrupt_the_file_are_not_recorded() {
        let mut db = empty_db();
        db.record("git", "with\ttab");
        db.record("git", "with\nnewline");
        db.record("", "empty-command");
        db.record("git", "");
        db.record("git", &"x".repeat(MAX_ACCEPTED_TEXT_BYTES + 1));
        assert!(db.entries.is_empty());
        assert!(!db.dirty);
    }

    #[test]
    fn the_record_stays_bounded() {
        let mut db = empty_db();
        for index in 0..(MAX_ACCEPTED_ENTRIES + 50) {
            db.record("git", &format!("candidate-{index}"));
        }
        assert!(db.entries.len() <= MAX_ACCEPTED_ENTRIES);
        // Pruning keeps the most recent, which are the highest scoring.
        assert!(db
            .score("git", &format!("candidate-{}", MAX_ACCEPTED_ENTRIES + 49))
            .is_some());
    }

    #[test]
    fn scores_for_one_command_are_looked_up_together() {
        let mut db = empty_db();
        db.record("systemctl", "nginx.service");
        db.record("systemctl", "nginx.service");
        db.record("docker", "web");

        let scores = db.scores_for("systemctl");
        assert_eq!(scores.len(), 1);
        assert!(scores["nginx.service"] > 0.0);
        assert!(db.scores_for("unknown").is_empty());
    }
}
