//! Session history: an append-only log of "deck became audible"
//! events, used for the History tab in the browser and (later) as the
//! recently-played input to auto-mix and the cue-sheet markers for a
//! recorded set.
//!
//! On-disk format mirrors the rest of `persistence.rs`: a line-based
//! text file `.history` in the music directory, one entry per line,
//! pipe-delimited as `<unix_secs>|<deck>|<path>`.
//!
//! Lines whose `path` contains a pipe are silently skipped at write
//! time rather than risk a non-roundtrippable entry (same convention
//! as the analysis cache).
//!
//! Append uses a single `write_all` on a pre-built byte buffer. POSIX
//! guarantees that an `O_APPEND` write smaller than `PIPE_BUF` (≥ 512
//! bytes, in practice 4 KB) is atomic, so concurrent writers — if any
//! ever appear — won't interleave bytes from different entries.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use control::DeckId;

/// One "deck became audible" event.
#[derive(Clone, Debug)]
pub struct HistoryEntry {
    /// Unix epoch seconds at the moment the event fired.
    pub timestamp: u64,
    pub deck: DeckId,
    pub path: PathBuf,
}

pub struct HistoryStore {
    file: PathBuf,
    entries: Vec<HistoryEntry>,
    /// Per-path all-time play count — mirrors `entries`, kept in sync
    /// on `append`. Exposed via `counts()` so the track table can
    /// render a Plays column without rebuilding the map every frame.
    counts: std::collections::HashMap<PathBuf, u32>,
    /// Monotonic counter bumped on every `append`. Lets caches keyed
    /// on history contents (e.g. the track-table filter/sort cache,
    /// when sorting by play count) invalidate automatically.
    generation: u64,
}

impl HistoryStore {
    pub fn load(dir: &Path) -> Self {
        let file = dir.join(".history");
        let mut entries = Vec::new();
        if let Ok(f) = File::open(&file) {
            for line in BufReader::new(f).lines().map_while(Result::ok) {
                if let Some(e) = parse_line(&line) {
                    entries.push(e);
                }
            }
        }
        // The file is append-only and grows in time order, but be
        // defensive — a clock skew or a hand-edit shouldn't desync the
        // session grouping. Cheap O(N log N) and runs once at startup.
        entries.sort_by_key(|e| e.timestamp);
        let mut counts: std::collections::HashMap<PathBuf, u32> =
            std::collections::HashMap::with_capacity(entries.len());
        for e in &entries {
            *counts.entry(e.path.clone()).or_default() += 1;
        }
        Self { file, entries, counts, generation: 0 }
    }

    /// Map of path → all-time play count. Updated incrementally in
    /// `append`; safe to call every frame.
    pub fn counts(&self) -> &std::collections::HashMap<PathBuf, u32> {
        &self.counts
    }

    /// Bumped on every `append`. Use as a cheap cache-key field.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    #[allow(dead_code)] // Kept for tests + future "recent N picks" feed to auto-mix.
    pub fn entries(&self) -> &[HistoryEntry] {
        &self.entries
    }

    /// Append a new audible event. Best-effort: if the disk write
    /// fails (read-only mount, full disk) the in-memory list still
    /// gets the entry so the UI stays responsive for the session.
    pub fn append(&mut self, deck: DeckId, path: &Path) {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let entry = HistoryEntry {
            timestamp,
            deck,
            path: path.to_path_buf(),
        };
        let _ = self.write_one(&entry);
        *self.counts.entry(entry.path.clone()).or_default() += 1;
        self.entries.push(entry);
        self.generation = self.generation.wrapping_add(1);
    }

    fn write_one(&self, e: &HistoryEntry) -> std::io::Result<()> {
        let path_str = e.path.to_string_lossy();
        if path_str.contains('|') {
            // Same compromise as the analysis cache: silently skip
            // rather than emit a line we can't round-trip.
            return Ok(());
        }
        let deck = match e.deck {
            DeckId::A => 'A',
            DeckId::B => 'B',
        };
        let line = format!("{}|{}|{}\n", e.timestamp, deck, path_str);
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.file)?;
        // Single write_all: atomic under POSIX O_APPEND for buffers
        // smaller than PIPE_BUF. Don't split into writeln! + flush.
        f.write_all(line.as_bytes())
    }

    /// Group entries into sessions where consecutive entries are
    /// separated by no more than `gap_secs`. Returns slices into the
    /// internal Vec, newest session first.
    pub fn sessions(&self, gap_secs: u64) -> Vec<&[HistoryEntry]> {
        let n = self.entries.len();
        if n == 0 {
            return Vec::new();
        }
        let mut breaks = vec![0usize];
        for i in 1..n {
            if self.entries[i].timestamp.saturating_sub(self.entries[i - 1].timestamp)
                > gap_secs
            {
                breaks.push(i);
            }
        }
        breaks.push(n);
        // Build sessions newest-first by walking the breaks in reverse.
        let mut out = Vec::with_capacity(breaks.len().saturating_sub(1));
        for w in breaks.windows(2).rev() {
            out.push(&self.entries[w[0]..w[1]]);
        }
        out
    }
}

fn parse_line(line: &str) -> Option<HistoryEntry> {
    let mut parts = line.splitn(3, '|');
    let ts: u64 = parts.next()?.parse().ok()?;
    let deck_str = parts.next()?;
    let deck = match deck_str.trim() {
        "A" => DeckId::A,
        "B" => DeckId::B,
        _ => return None,
    };
    let path_str = parts.next()?.trim();
    if path_str.is_empty() {
        return None;
    }
    Some(HistoryEntry {
        timestamp: ts,
        deck,
        path: PathBuf::from(path_str),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(t: u64, d: DeckId, p: &str) -> HistoryEntry {
        HistoryEntry {
            timestamp: t,
            deck: d,
            path: PathBuf::from(p),
        }
    }

    #[test]
    fn sessions_groups_by_gap() {
        let mut s = HistoryStore {
            file: PathBuf::new(),
            entries: vec![
                entry(100, DeckId::A, "/a"),
                entry(200, DeckId::B, "/b"),
                // 3-hour gap → new session
                entry(200 + 3 * 3600, DeckId::A, "/c"),
                entry(200 + 3 * 3600 + 10, DeckId::B, "/d"),
            ],
            counts: std::collections::HashMap::new(),
            generation: 0,
        };
        // Sanity: pretend load already sorted.
        s.entries.sort_by_key(|e| e.timestamp);
        let sessions = s.sessions(2 * 3600);
        // Newest first.
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].len(), 2);
        assert_eq!(sessions[0][0].path, PathBuf::from("/c"));
        assert_eq!(sessions[1].len(), 2);
        assert_eq!(sessions[1][0].path, PathBuf::from("/a"));
    }

    #[test]
    fn parse_roundtrip() {
        let parsed = parse_line("1717939200|A|/music/song.mp3").unwrap();
        assert_eq!(parsed.timestamp, 1717939200);
        assert!(matches!(parsed.deck, DeckId::A));
        assert_eq!(parsed.path, PathBuf::from("/music/song.mp3"));
    }

    #[test]
    fn parse_rejects_bad_lines() {
        assert!(parse_line("").is_none());
        assert!(parse_line("nope").is_none());
        assert!(parse_line("123|C|/p").is_none());
        assert!(parse_line("123|A|").is_none());
    }
}
