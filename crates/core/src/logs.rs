//! Per-execution log files, kept outside SQLite so the database stays small.
//!
//! One line per entry: `unix_ms <TAB> kind <TAB> text`, where `kind` is `o`
//! (stdout), `e` (stderr) or `m` (a note from ScriptRunner itself, such as the
//! start/finish banner). Entry text never contains `\n`, so the format needs no
//! escaping and can be read back incrementally.

use std::fs::File;
use std::io::{self, BufRead, BufReader, BufWriter, Seek, SeekFrom, Write};
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

/// One page of a log read by byte position; see [`read_log_from`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogChunk {
    pub entries: Vec<LogEntry>,
    /// Where the next read starts: the end of the last line consumed.
    pub next_offset: u64,
    /// `true` when this read reached the end of what is in the file right now.
    /// A run that is still going may add more, so keep polling until the
    /// execution is over *and* a read comes back `eof`.
    pub eof: bool,
}

/// Reads whole log lines starting at byte `offset`, until about `max_bytes` of
/// the file are consumed (always at least one line, so a reader makes progress).
/// `offset` must be 0 or a `next_offset` returned earlier. A final line that is
/// still being written (no newline yet) is left for the next read, and
/// malformed lines are skipped. Unlike [`read_log`] it never rereads the part
/// already seen, so following a long log costs the same on every poll.
pub fn read_log_from(path: &Path, offset: u64, max_bytes: u64) -> io::Result<LogChunk> {
    let mut file = File::open(path)?;
    if offset >= file.metadata()?.len() {
        return Ok(LogChunk {
            entries: Vec::new(),
            next_offset: offset,
            eof: true,
        });
    }
    file.seek(SeekFrom::Start(offset))?;
    let mut reader = BufReader::new(file);
    let mut entries = Vec::new();
    let mut next = offset;
    let mut line = Vec::new();
    loop {
        if next > offset && next - offset >= max_bytes {
            return Ok(LogChunk {
                entries,
                next_offset: next,
                eof: false,
            });
        }
        line.clear();
        let read = reader.read_until(b'\n', &mut line)?;
        if read == 0 || line.last() != Some(&b'\n') {
            break; // end of file, or a line the writer has not finished
        }
        next += read as u64;
        if let Some(entry) = decode(&String::from_utf8_lossy(&line[..read - 1])) {
            entries.push(entry);
        }
    }
    Ok(LogChunk {
        entries,
        next_offset: next,
        eof: true,
    })
}

/// One entry as a line of the plain-text export, newline included.
pub fn format_entry(entry: &LogEntry) -> String {
    let tag = match entry.kind {
        LogKind::Stdout => "OUT",
        LogKind::Stderr => "ERR",
        LogKind::Meta => "---",
    };
    format!(
        "{} [{}] {}\n",
        entry.at.format("%Y-%m-%d %H:%M:%S%.3f"),
        tag,
        entry.text
    )
}

/// Renders a log as plain text for the "export to .txt" action.
pub fn export_text(path: &Path) -> io::Result<String> {
    Ok(read_log(path, 0, usize::MAX)?.iter().map(format_entry).collect())
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

    fn texts(chunk: &LogChunk) -> Vec<&str> {
        chunk.entries.iter().map(|e| e.text.as_str()).collect()
    }

    #[test]
    fn reading_by_offset_follows_a_log_without_rereading() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.log");
        let mut writer = LogWriter::create(&path, 1024).unwrap();
        for i in 0..5 {
            writer.write_line(&line(LogStream::Stdout, &format!("l{i}"))).unwrap();
        }
        writer.flush().unwrap();

        // A small budget returns a few lines at a time and then stops being `eof`.
        let first = read_log_from(&path, 0, 1).unwrap();
        assert_eq!(texts(&first), ["l0"], "at least one line, so a reader always advances");
        assert!(!first.eof);
        let rest = read_log_from(&path, first.next_offset, 1 << 20).unwrap();
        assert_eq!(texts(&rest), ["l1", "l2", "l3", "l4"]);
        assert!(rest.eof);

        // Caught up: nothing new, same offset. Then the writer adds a line.
        let idle = read_log_from(&path, rest.next_offset, 1 << 20).unwrap();
        assert!(idle.entries.is_empty() && idle.eof && idle.next_offset == rest.next_offset);
        writer.write_line(&line(LogStream::Stderr, "late")).unwrap();
        writer.flush().unwrap();
        let more = read_log_from(&path, idle.next_offset, 1 << 20).unwrap();
        assert_eq!(texts(&more), ["late"]);
        assert_eq!(more.entries[0].kind, LogKind::Stderr);
    }

    #[test]
    fn a_half_written_last_line_waits_for_the_next_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.log");
        std::fs::write(&path, "1\to\tdone\n2\to\tpart").unwrap();

        let chunk = read_log_from(&path, 0, 1 << 20).unwrap();
        assert_eq!(texts(&chunk), ["done"]);
        assert_eq!(chunk.next_offset, "1\to\tdone\n".len() as u64);
        assert!(chunk.eof);

        std::fs::write(&path, "1\to\tdone\n2\to\tpartial\n").unwrap();
        let chunk = read_log_from(&path, chunk.next_offset, 1 << 20).unwrap();
        assert_eq!(texts(&chunk), ["partial"]);
    }

    #[test]
    fn reading_past_the_end_or_a_bad_line_is_harmless() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.log");
        std::fs::write(&path, "garbage\n1\to\tok\n").unwrap();
        let chunk = read_log_from(&path, 0, 1 << 20).unwrap();
        assert_eq!(texts(&chunk), ["ok"], "a malformed line is skipped");
        let beyond = read_log_from(&path, 9_999, 1 << 20).unwrap();
        assert!(beyond.entries.is_empty() && beyond.eof && beyond.next_offset == 9_999);
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
