//! Per-execution log files, kept outside SQLite so the database stays small.
//!
//! One line per entry: `unix_ms <TAB> kind <TAB> text`, where `kind` is `o`
//! (stdout), `e` (stderr) or `m` (a note from ScriptRunner itself, such as the
//! start/finish banner). Entry text never contains `\n`, so the format needs no
//! escaping and can be read back incrementally.

use std::fs::File;
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::Path;

use chrono::{DateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};

use crate::executor::{LogLine, LogStream};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogKind {
    Stdout,
    Stderr,
    /// Written by ScriptRunner, not the script.
    Meta,
}

impl LogKind {
    fn code(self) -> char {
        match self {
            Self::Stdout => 'o',
            Self::Stderr => 'e',
            Self::Meta => 'm',
        }
    }

    fn from_code(code: &str) -> Option<Self> {
        match code {
            "o" => Some(Self::Stdout),
            "e" => Some(Self::Stderr),
            "m" => Some(Self::Meta),
            _ => None,
        }
    }
}

impl From<LogStream> for LogKind {
    fn from(stream: LogStream) -> Self {
        match stream {
            LogStream::Stdout => Self::Stdout,
            LogStream::Stderr => Self::Stderr,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogEntry {
    pub at: DateTime<Utc>,
    pub kind: LogKind,
    pub text: String,
}

pub struct LogWriter {
    out: BufWriter<File>,
    /// Bytes of script output written so far (notes from us are not counted).
    bytes: u64,
    max_bytes: u64,
    truncated: bool,
}

impl LogWriter {
    /// Creates (or truncates) the file. `max_bytes` caps script output; once
    /// reached, a single note is written and further output is dropped.
    pub fn create(path: &Path, max_bytes: u64) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Ok(Self {
            out: BufWriter::new(File::create(path)?),
            bytes: 0,
            max_bytes,
            truncated: false,
        })
    }

    pub fn write_line(&mut self, line: &LogLine) -> io::Result<()> {
        let encoded = encode(line.at, LogKind::from(line.stream), &line.text);
        let size = encoded.len() as u64;
        if self.truncated || self.bytes + size > self.max_bytes {
            if !self.truncated {
                self.truncated = true;
                let note = format!("[log truncated: {} byte limit reached]", self.max_bytes);
                self.out
                    .write_all(encode(Utc::now(), LogKind::Meta, &note).as_bytes())?;
            }
            return Ok(());
        }
        self.bytes += size;
        self.out.write_all(encoded.as_bytes())
    }

    pub fn write_meta(&mut self, text: &str) -> io::Result<()> {
        self.out.write_all(encode(Utc::now(), LogKind::Meta, text).as_bytes())
    }

    pub fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }

    pub fn bytes_written(&self) -> u64 {
        self.bytes
    }

    pub fn truncated(&self) -> bool {
        self.truncated
    }
}

fn encode(at: DateTime<Utc>, kind: LogKind, text: &str) -> String {
    // Defensive: a stray line break would corrupt the framing.
    let text = text.replace(['\n', '\r'], " ");
    format!("{}\t{}\t{}\n", at.timestamp_millis(), kind.code(), text)
}

fn decode(line: &str) -> Option<LogEntry> {
    let mut parts = line.splitn(3, '\t');
    let millis: i64 = parts.next()?.parse().ok()?;
    let kind = LogKind::from_code(parts.next()?)?;
    let text = parts.next()?.to_owned();
    Some(LogEntry {
        at: Utc.timestamp_millis_opt(millis).single()?,
        kind,
        text,
    })
}

/// Reads up to `limit` entries, skipping the first `skip`. Polling with
/// `skip = entries already seen` gives a cheap live tail. Malformed lines
/// (e.g. a half-flushed last line) are ignored.
pub fn read_log(path: &Path, skip: usize, limit: usize) -> io::Result<Vec<LogEntry>> {
    let reader = BufReader::new(File::open(path)?);
    let mut entries = Vec::new();
    let mut seen = 0usize;
    for line in reader.lines() {
        let line = line?;
        let Some(entry) = decode(&line) else { continue };
        if seen >= skip {
            if entries.len() >= limit {
                break;
            }
            entries.push(entry);
        }
        seen += 1;
    }
    Ok(entries)
}

/// Renders a log as plain text for the "export to .txt" action.
pub fn export_text(path: &Path) -> io::Result<String> {
    let mut out = String::new();
    for entry in read_log(path, 0, usize::MAX)? {
        let tag = match entry.kind {
            LogKind::Stdout => "OUT",
            LogKind::Stderr => "ERR",
            LogKind::Meta => "---",
        };
        out.push_str(&format!(
            "{} [{}] {}\n",
            entry.at.format("%Y-%m-%d %H:%M:%S%.3f"),
            tag,
            entry.text
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(stream: LogStream, text: &str) -> LogLine {
        LogLine {
            at: Utc::now(),
            stream,
            text: text.to_owned(),
        }
    }

    #[test]
    fn entries_round_trip_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("a.log");
        let mut writer = LogWriter::create(&path, 1024).unwrap();
        writer.write_meta("started").unwrap();
        writer.write_line(&line(LogStream::Stdout, "hello\tworld")).unwrap();
        writer.write_line(&line(LogStream::Stderr, "boom")).unwrap();
        writer.flush().unwrap();

        let entries = read_log(&path, 0, 100).unwrap();
        let shape: Vec<_> = entries.iter().map(|e| (e.kind, e.text.as_str())).collect();
        assert_eq!(
            shape,
            vec![
                (LogKind::Meta, "started"),
                (LogKind::Stdout, "hello\tworld"),
                (LogKind::Stderr, "boom"),
            ]
        );
    }

    #[test]
    fn paging_supports_live_tailing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.log");
        let mut writer = LogWriter::create(&path, 1024).unwrap();
        for i in 0..5 {
            writer.write_line(&line(LogStream::Stdout, &format!("l{i}"))).unwrap();
        }
        writer.flush().unwrap();
        let page = read_log(&path, 2, 2).unwrap();
        assert_eq!(page.iter().map(|e| e.text.as_str()).collect::<Vec<_>>(), ["l2", "l3"]);
    }

    #[test]
    fn output_beyond_the_limit_is_dropped_with_one_note() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.log");
        let mut writer = LogWriter::create(&path, 80).unwrap();
        for i in 0..20 {
            writer
                .write_line(&line(LogStream::Stdout, &format!("line number {i}")))
                .unwrap();
        }
        writer.write_meta("finished").unwrap();
        writer.flush().unwrap();

        assert!(writer.truncated());
        assert!(writer.bytes_written() <= 80);
        let entries = read_log(&path, 0, 1000).unwrap();
        let notes: Vec<_> = entries
            .iter()
            .filter(|e| e.kind == LogKind::Meta)
            .map(|e| e.text.as_str())
            .collect();
        assert_eq!(notes.len(), 2);
        assert!(notes[0].contains("truncated"));
        assert_eq!(notes[1], "finished");
    }

    #[test]
    fn export_is_human_readable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.log");
        let mut writer = LogWriter::create(&path, 1024).unwrap();
        writer.write_line(&line(LogStream::Stderr, "oops")).unwrap();
        writer.flush().unwrap();
        let text = export_text(&path).unwrap();
        assert!(text.contains("[ERR] oops"));
    }
}
