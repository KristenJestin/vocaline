//! Statistics of dictation: how much, where, and how fast it was pasted.
//!
//! The store keeps counts and durations, never what was said: not a word, not a window title,
//! only the window's class. One JSON line per dictation, appended by the daemon when the
//! dictation ends; `vocaline stats` reads the file itself, so it works without the daemon.

use jiff::ToSpan;
use jiff::civil::Date;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::PathBuf;

/// The application of a dictation whose window was not asked, or not known.
pub const UNKNOWN: &str = "unknown";

/// What is kept of one dictation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Dictation {
    /// The class of the window it started in.
    pub app: String,
    pub words: u32,
    /// How long the microphone was heard.
    pub audio_ms: u64,
    /// From the stop to the hand-over of the text to the output; none when nothing was pasted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paste_ms: Option<u64>,
    /// The GPU had no memory left for it: it was decoded again on the CPU.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cpu_fallback: bool,
}

/// A line of the store.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    /// The local day the dictation ended.
    pub day: Date,
    #[serde(flatten)]
    pub dictation: Dictation,
}

/// Where the daemon writes what it kept of a dictation.
pub trait Ledger: Send {
    fn record(&self, dictation: &Dictation) -> io::Result<()>;
}

/// The words of a text: what has a letter or a digit in it, so a lone `?` is not one.
pub fn count_words(text: &str) -> u32 {
    text.split_whitespace()
        .filter(|word| word.chars().any(char::is_alphanumeric))
        .count() as u32
}

/// The store file.
pub struct Store {
    path: PathBuf,
}

impl Store {
    pub fn new(path: PathBuf) -> Store {
        Store { path }
    }

    /// The entries, oldest first, and how many lines could not be read.
    pub fn read(&self) -> io::Result<(Vec<Entry>, usize)> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((Vec::new(), 0)),
            Err(e) => return Err(e),
        };
        let mut entries = Vec::new();
        let mut skipped = 0;
        for line in text.lines().filter(|line| !line.trim().is_empty()) {
            // A line cut short by a crash must not hide the others.
            match serde_json::from_str(line) {
                Ok(entry) => entries.push(entry),
                Err(_) => skipped += 1,
            }
        }
        Ok((entries, skipped))
    }
}

impl Ledger for Store {
    fn record(&self, dictation: &Dictation) -> io::Result<()> {
        let entry = Entry {
            day: jiff::Zoned::now().date(),
            dictation: dictation.clone(),
        };
        let line = serde_json::to_string(&entry).expect("an entry serialises") + "\n";
        if let Some(folder) = self.path.parent() {
            std::fs::create_dir_all(folder)?;
        }
        // One write for the whole line: appended lines do not interleave.
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?
            .write_all(line.as_bytes())
    }
}

/// The time from the stop to the paste, over the dictations that were pasted.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Latency {
    pub count: u32,
    pub median_ms: u64,
    pub max_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Totals {
    pub dictations: u32,
    pub words: u64,
    pub audio_ms: u64,
    pub paste: Option<Latency>,
    /// The dictations decoded again on the CPU because the GPU had no memory left.
    pub cpu_fallbacks: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Application {
    pub app: String,
    #[serde(flatten)]
    pub totals: Totals,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Day {
    pub day: Date,
    #[serde(flatten)]
    pub totals: Totals,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Summary {
    #[serde(flatten)]
    pub totals: Totals,
    /// The most dictated first.
    pub by_application: Vec<Application>,
    /// Oldest first; the days with no dictation are left out.
    pub by_day: Vec<Day>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Report {
    pub date: Date,
    pub today: Summary,
    pub last_7_days: Summary,
    pub all_time: Summary,
}

/// The entries grouped as `vocaline stats` shows them, `today` being the local day now.
pub fn report(entries: &[Entry], today: Date) -> Report {
    let week_start = today.saturating_sub(6.days());
    let summary = |keep: &dyn Fn(&Entry) -> bool| {
        summarise(&entries.iter().filter(|e| keep(e)).collect::<Vec<_>>())
    };
    Report {
        date: today,
        today: summary(&|e| e.day == today),
        last_7_days: summary(&|e| (week_start..=today).contains(&e.day)),
        all_time: summary(&|_| true),
    }
}

fn summarise(entries: &[&Entry]) -> Summary {
    let mut apps: BTreeMap<&str, Vec<&Entry>> = BTreeMap::new();
    let mut days: BTreeMap<Date, Vec<&Entry>> = BTreeMap::new();
    for &entry in entries {
        apps.entry(&entry.dictation.app).or_default().push(entry);
        days.entry(entry.day).or_default().push(entry);
    }
    let mut by_application: Vec<Application> = apps
        .into_iter()
        .map(|(app, entries)| Application {
            app: app.to_string(),
            totals: totals(&entries),
        })
        .collect();
    // The sort is stable and the map is by name: equal words are in alphabetical order.
    by_application.sort_by_key(|a| std::cmp::Reverse(a.totals.words));
    Summary {
        totals: totals(entries),
        by_application,
        by_day: days
            .into_iter()
            .map(|(day, entries)| Day {
                day,
                totals: totals(&entries),
            })
            .collect(),
    }
}

fn totals(entries: &[&Entry]) -> Totals {
    let mut pastes: Vec<u64> = entries
        .iter()
        .filter_map(|e| e.dictation.paste_ms)
        .collect();
    pastes.sort_unstable();
    let paste = (!pastes.is_empty()).then(|| {
        let middle = pastes.len() / 2;
        Latency {
            count: pastes.len() as u32,
            median_ms: if pastes.len().is_multiple_of(2) {
                (pastes[middle - 1] + pastes[middle]) / 2
            } else {
                pastes[middle]
            },
            max_ms: pastes[pastes.len() - 1],
        }
    });
    Totals {
        dictations: entries.len() as u32,
        words: entries.iter().map(|e| u64::from(e.dictation.words)).sum(),
        audio_ms: entries.iter().map(|e| e.dictation.audio_ms).sum(),
        paste,
        cpu_fallbacks: entries.iter().filter(|e| e.dictation.cpu_fallback).count() as u32,
    }
}

impl Totals {
    fn line(&self) -> String {
        format!(
            "{} {}, {} {}, {:.1} min",
            self.dictations,
            plural(self.dictations.into(), "dictation"),
            self.words,
            plural(self.words, "word"),
            self.audio_ms as f64 / 60_000.0,
        )
    }
}

fn plural(count: u64, noun: &str) -> String {
    if count == 1 {
        noun.to_string()
    } else {
        format!("{noun}s")
    }
}

impl Report {
    /// What `vocaline stats` prints.
    pub fn text(&self) -> String {
        let mut out = String::new();
        self.section(
            &mut out,
            &format!("today ({})", self.date),
            &self.today,
            false,
        );
        self.section(&mut out, "last 7 days", &self.last_7_days, true);
        self.section(&mut out, "all time", &self.all_time, false);
        out
    }

    fn section(&self, out: &mut String, title: &str, summary: &Summary, days: bool) {
        use std::fmt::Write as _;
        if !out.is_empty() {
            out.push('\n');
        }
        let _ = writeln!(out, "{title}");
        if summary.totals.dictations == 0 {
            out.push_str("  no dictation\n");
            return;
        }
        let _ = writeln!(out, "  {}", summary.totals.line());
        if let Some(paste) = &summary.totals.paste {
            let _ = writeln!(
                out,
                "  stop to paste: median {} ms, max {} ms",
                paste.median_ms, paste.max_ms
            );
        }
        if summary.totals.cpu_fallbacks > 0 {
            let _ = writeln!(
                out,
                "  decoded on the CPU, the GPU being full: {}",
                summary.totals.cpu_fallbacks
            );
        }
        let _ = writeln!(out, "  by application");
        let width = summary
            .by_application
            .iter()
            .map(|a| a.app.chars().count())
            .max()
            .unwrap_or(0);
        for a in &summary.by_application {
            let _ = writeln!(out, "    {:<width$}  {}", a.app, a.totals.line());
        }
        if days {
            let _ = writeln!(out, "  by day");
            for d in &summary.by_day {
                let _ = writeln!(out, "    {}  {}", d.day, d.totals.line());
            }
        }
    }
}

/// Prints the statistics of `store`: as text, or as one JSON object. The second value is how
/// many lines of the store could not be read.
pub fn print(
    store: &Store,
    today: Date,
    json: bool,
    out: &mut impl Write,
) -> Result<usize, String> {
    let (entries, skipped) = store
        .read()
        .map_err(|e| format!("{}: {e}", store.path.display()))?;
    let report = report(&entries, today);
    let text = if json {
        serde_json::to_string(&report).expect("a report serialises") + "\n"
    } else {
        report.text()
    };
    out.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
    Ok(skipped)
}

#[cfg(test)]
mod tests;
