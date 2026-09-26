//! Streaming mbox reader (spec §7.8). Reads one message at a time from any
//! `BufRead`, so a multi-gigabyte Google Takeout never sits in memory.
//!
//! Format: each message starts with a `From ` separator line at the start
//! of the file or after an empty line; the separator is not part of the
//! message. Body lines that begin with `From ` were escaped by prefixing
//! `>`; mboxrd escapes every `>*From ` line one level deeper, so one `>` is
//! removed from any line matching `^>+From `. Line endings (LF or CRLF)
//! are kept as found.

use std::io::{self, BufRead};

/// One message: its raw RFC 5322 bytes and where its separator started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MboxMessage {
    /// Byte offset of the `From ` separator line, for progress and resume.
    pub offset: u64,
    pub raw: Vec<u8>,
}

/// Iterator over the messages of an mbox. Blank space before the first
/// separator is skipped; a file with no separator at all is one message
/// (some exporters write a single message without one).
pub struct MboxReader<R> {
    input: R,
    /// Bytes consumed so far.
    position: u64,
    /// The separator line that starts the next message, already read.
    pending_separator: Option<u64>,
    started: bool,
    done: bool,
    line: Vec<u8>,
}

impl<R: BufRead> MboxReader<R> {
    pub fn new(input: R) -> Self {
        Self { input, position: 0, pending_separator: None, started: false, done: false, line: Vec::new() }
    }

    /// Bytes read so far.
    pub fn position(&self) -> u64 {
        self.position
    }

    fn read_line(&mut self) -> io::Result<bool> {
        self.line.clear();
        let n = self.input.read_until(b'\n', &mut self.line)?;
        self.position += n as u64;
        Ok(n > 0)
    }

    fn next_message(&mut self) -> io::Result<Option<MboxMessage>> {
        if self.done {
            return Ok(None);
        }
        // Find the first separator (or the first content, for a bare file).
        let offset = match self.pending_separator.take() {
            Some(offset) => offset,
            None => {
                let mut offset;
                loop {
                    offset = self.position;
                    if !self.read_line()? {
                        self.done = true;
                        return Ok(None);
                    }
                    if is_separator(&self.line) {
                        break;
                    }
                    if !self.started && !is_blank(&self.line) {
                        // No separator before content: the whole file is
                        // one message, starting here.
                        self.started = true;
                        let mut raw = std::mem::take(&mut self.line);
                        self.read_body(&mut raw)?;
                        return Ok(Some(MboxMessage { offset, raw }));
                    }
                }
                offset
            }
        };
        self.started = true;
        let mut raw = Vec::new();
        self.read_body(&mut raw)?;
        Ok(Some(MboxMessage { offset, raw }))
    }

    /// Read body lines into `raw` until the next separator (remembered as
    /// pending) or the end. The blank line before a separator belongs to
    /// the format, not the message, and is dropped.
    fn read_body(&mut self, raw: &mut Vec<u8>) -> io::Result<()> {
        let mut previous_blank = raw.is_empty() || is_blank(raw);
        loop {
            let line_start = self.position;
            if !self.read_line()? {
                self.done = true;
                break;
            }
            if previous_blank && is_separator(&self.line) {
                self.pending_separator = Some(line_start);
                trim_one_trailing_blank_line(raw);
                return Ok(());
            }
            previous_blank = is_blank(&self.line);
            unescape_from(&self.line, raw);
        }
        trim_one_trailing_blank_line(raw);
        Ok(())
    }
}

impl<R: BufRead> Iterator for MboxReader<R> {
    type Item = io::Result<MboxMessage>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match self.next_message() {
                Ok(Some(m)) if m.raw.iter().all(u8::is_ascii_whitespace) => continue, // empty entry
                Ok(Some(m)) => return Some(Ok(m)),
                Ok(None) => return None,
                Err(e) => {
                    self.done = true;
                    return Some(Err(e));
                }
            }
        }
    }
}

/// `From ` followed by something: an address or `MAILER-DAEMON`, then a
/// date. Checking for a second space keeps a body line such as `From here
/// on…` after a blank line from splitting a message in the common case.
fn is_separator(line: &[u8]) -> bool {
    line.starts_with(b"From ") && line[5..].iter().filter(|b| **b == b' ').count() >= 2 && !line[5..].starts_with(b" ")
}

fn is_blank(line: &[u8]) -> bool {
    line == b"\n" || line == b"\r\n"
}

/// mboxrd unescaping: one `>` comes off any `>+From ` line.
fn unescape_from(line: &[u8], out: &mut Vec<u8>) {
    let quotes = line.iter().take_while(|b| **b == b'>').count();
    if quotes > 0 && line[quotes..].starts_with(b"From ") {
        out.extend_from_slice(&line[1..]);
    } else {
        out.extend_from_slice(line);
    }
}

fn trim_one_trailing_blank_line(raw: &mut Vec<u8>) {
    if raw.ends_with(b"\r\n\r\n") {
        raw.truncate(raw.len() - 2);
    } else if raw.ends_with(b"\n\n") {
        raw.truncate(raw.len() - 1);
    }
}

/// Test fixtures: build mbox files from synthetic messages. Never real
/// mail (spec §7.8 Testing).
pub mod fixture {
    /// A synthetic message for an mbox fixture.
    #[derive(Debug, Clone, Default)]
    pub struct FixtureMessage {
        pub from: String,
        pub to: String,
        pub subject: String,
        pub date: String,
        pub message_id: String,
        pub in_reply_to: Option<String>,
        pub body: String,
        /// Google Takeout's `X-Gmail-Labels`, comma-separated.
        pub gmail_labels: Option<String>,
        /// Google Takeout's thread id (decimal).
        pub gm_thrid: Option<String>,
        /// (file name, MIME type, bytes).
        pub attachment: Option<(String, String, Vec<u8>)>,
    }

    impl FixtureMessage {
        pub fn simple(n: usize) -> Self {
            Self {
                from: format!("Sender {n} <sender{n}@example.com>"),
                to: "me@example.com".into(),
                subject: format!("Subject {n}"),
                date: format!("Mon, {:02} Sep 2025 10:00:00 +0000", 1 + n % 28),
                message_id: format!("m{n}@example.com"),
                body: format!("Body of message {n}.\n"),
                ..Default::default()
            }
        }

        /// RFC 5322 bytes with LF line endings.
        pub fn to_rfc822(&self) -> Vec<u8> {
            let mut h = String::new();
            if let Some(labels) = &self.gmail_labels {
                h += &format!("X-Gmail-Labels: {labels}\n");
            }
            if let Some(thrid) = &self.gm_thrid {
                h += &format!("X-GM-THRID: {thrid}\n");
            }
            h += &format!(
                "From: {}\nTo: {}\nSubject: {}\nDate: {}\nMessage-ID: <{}>\n",
                self.from, self.to, self.subject, self.date, self.message_id
            );
            if let Some(parent) = &self.in_reply_to {
                h += &format!("In-Reply-To: <{parent}>\nReferences: <{parent}>\n");
            }
            h += "MIME-Version: 1.0\n";
            match &self.attachment {
                None => {
                    h += "Content-Type: text/plain; charset=utf-8\n\n";
                    let mut out = h.into_bytes();
                    out.extend_from_slice(self.body.as_bytes());
                    out
                }
                Some((name, mime, bytes)) => {
                    let boundary = "fixture-boundary";
                    h += &format!("Content-Type: multipart/mixed; boundary=\"{boundary}\"\n\n");
                    h += &format!("--{boundary}\nContent-Type: text/plain; charset=utf-8\n\n{}\n", self.body);
                    h += &format!(
                        "--{boundary}\nContent-Type: {mime}; name=\"{name}\"\nContent-Disposition: attachment; filename=\"{name}\"\nContent-Transfer-Encoding: base64\n\n"
                    );
                    let mut out = h.into_bytes();
                    out.extend_from_slice(base64_lines(bytes).as_bytes());
                    out.extend_from_slice(format!("--{boundary}--\n").as_bytes());
                    out
                }
            }
        }
    }

    /// An mbox of `messages`, mboxrd-escaping body lines that start with
    /// `From ` (as real exporters do).
    pub fn build(messages: &[FixtureMessage]) -> Vec<u8> {
        let mut out = Vec::new();
        for m in messages {
            out.extend_from_slice(b"From sender@example.com Mon Sep  1 10:00:00 2025\n");
            for line in m.to_rfc822().split_inclusive(|b| *b == b'\n') {
                let quotes = line.iter().take_while(|b| **b == b'>').count();
                if line[quotes..].starts_with(b"From ") {
                    out.push(b'>');
                }
                out.extend_from_slice(line);
            }
            if !out.ends_with(b"\n") {
                out.push(b'\n');
            }
            out.push(b'\n');
        }
        out
    }

    fn base64_lines(bytes: &[u8]) -> String {
        const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut s = String::with_capacity(bytes.len() * 4 / 3 + bytes.len() / 57 + 4);
        for (i, chunk) in bytes.chunks(3).enumerate() {
            let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
            let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
            s.push(T[(n >> 18) as usize & 63] as char);
            s.push(T[(n >> 12) as usize & 63] as char);
            s.push(if chunk.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
            s.push(if chunk.len() > 2 { T[n as usize & 63] as char } else { '=' });
            if (i + 1) % 19 == 0 {
                s.push('\n');
            }
        }
        if !s.ends_with('\n') {
            s.push('\n');
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::{FixtureMessage, build};
    use super::*;

    fn read_all(bytes: &[u8]) -> Vec<MboxMessage> {
        MboxReader::new(bytes).collect::<io::Result<Vec<_>>>().unwrap()
    }

    #[test]
    fn messages_split_on_separators_with_offsets() {
        let mbox = build(&[FixtureMessage::simple(1), FixtureMessage::simple(2), FixtureMessage::simple(3)]);
        let messages = read_all(&mbox);
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].offset, 0);
        for m in &messages {
            assert!(mbox[m.offset as usize..].starts_with(b"From sender@example.com"));
            assert!(m.raw.starts_with(b"From: Sender"), "separator not included");
            assert!(!m.raw.ends_with(b"\n\n"), "the format's blank line is dropped");
        }
        let parsed = crate::parse(&messages[1].raw).unwrap();
        assert_eq!(parsed.headers.subject, "Subject 2");
    }

    #[test]
    fn escaped_from_lines_are_unescaped_and_do_not_split() {
        let mut m = FixtureMessage::simple(1);
        m.body = "Hi,\n\nFrom the team: hello.\n>From quoted text.\n".into();
        let mbox = build(&[m, FixtureMessage::simple(2)]);
        assert!(mbox.windows(21).any(|w| w == b">From the team: hello"), "fixture escaped it");
        let messages = read_all(&mbox);
        assert_eq!(messages.len(), 2);
        let body = String::from_utf8(messages[0].raw.clone()).unwrap();
        assert!(body.contains("\nFrom the team: hello."), "{body}");
        assert!(body.contains("\n>From quoted text."), "one level removed from >>From");
    }

    #[test]
    fn crlf_files_and_leading_blank_lines_work() {
        let lf = build(&[FixtureMessage::simple(1), FixtureMessage::simple(2)]);
        let mut crlf = b"\r\n\r\n".to_vec();
        for line in lf.split_inclusive(|b| *b == b'\n') {
            crlf.extend_from_slice(&line[..line.len() - 1]);
            crlf.extend_from_slice(b"\r\n");
        }
        let messages = read_all(&crlf);
        assert_eq!(messages.len(), 2);
        assert!(messages[0].raw.ends_with(b"\r\n") && !messages[0].raw.ends_with(b"\r\n\r\n"));
        assert_eq!(crate::parse(&messages[1].raw).unwrap().headers.subject, "Subject 2");
    }

    #[test]
    fn a_file_without_separators_is_one_message_and_empty_files_are_empty() {
        let raw = FixtureMessage::simple(7).to_rfc822();
        let messages = read_all(&raw);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].raw, raw);
        assert!(read_all(b"").is_empty());
        assert!(read_all(b"\n\n").is_empty());
    }

    #[test]
    fn a_body_line_starting_from_after_a_blank_line_is_not_a_separator_unless_it_looks_like_one() {
        let mut m = FixtureMessage::simple(1);
        // Unescaped, as a sloppy exporter might leave it: two words only.
        m.body = "Hello\n".into();
        let mut mbox = build(&[m]);
        mbox.extend_from_slice(b"From here\n\nstill the same message\n");
        let messages = read_all(&mbox);
        assert_eq!(messages.len(), 1, "`From here` has no date part");
    }

    #[test]
    fn attachments_survive_the_round_trip() {
        let mut m = FixtureMessage::simple(1);
        m.attachment =
            Some(("report.bin".into(), "application/octet-stream".into(), (0..=255u8).cycle().take(5000).collect()));
        let messages = read_all(&build(&[m]));
        let parsed = crate::parse(&messages[0].raw).unwrap();
        assert_eq!(parsed.attachments.len(), 1);
        assert_eq!(parsed.attachments[0].filename, "report.bin");
        assert_eq!(parsed.attachments[0].data.len(), 5000);
    }

    #[test]
    fn position_tracks_bytes_read_for_progress() {
        let mbox = build(&[FixtureMessage::simple(1), FixtureMessage::simple(2)]);
        let mut reader = MboxReader::new(&mbox[..]);
        reader.next().unwrap().unwrap();
        assert!(reader.position() > 0 && reader.position() < mbox.len() as u64);
        reader.next().unwrap().unwrap();
        assert!(reader.next().is_none());
        assert_eq!(reader.position(), mbox.len() as u64);
    }
}
