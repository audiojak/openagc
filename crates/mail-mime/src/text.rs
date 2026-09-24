//! Plain text for agents (spec §10.2): quoted replies stripped from
//! bodies, and text extracted from attachments. Never binary data.

use std::io::{Cursor, Read};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ExtractError {
    #[error("{0} attachments have no text to extract")]
    Unsupported(String),
    #[error("could not read the attachment: {0}")]
    Unreadable(String),
}

/// The body without the quoted conversation below it: stops at an
/// "On …, … wrote:" attribution or a forwarded/original-message divider,
/// and drops `>` lines.
pub fn strip_quoted(text: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i].trim_end();
        let t = line.trim();
        if t.starts_with('>') {
            i += 1;
            continue;
        }
        if is_divider(t) {
            break;
        }
        // "On Tue, Sep 15, 2026 at 9:00 AM Alex <a@x.com> wrote:" often
        // wraps over two lines.
        let joined = match lines.get(i + 1) {
            Some(next) if t.starts_with("On ") && !t.ends_with("wrote:") => format!("{t} {}", next.trim()),
            _ => t.to_owned(),
        };
        if joined.starts_with("On ") && joined.ends_with("wrote:") {
            break;
        }
        out.push(line);
        i += 1;
    }
    while out.last().is_some_and(|l| l.trim().is_empty()) {
        out.pop();
    }
    out.join("\n")
}

fn is_divider(t: &str) -> bool {
    let lower = t.to_ascii_lowercase();
    lower.starts_with("-----original message-----")
        || lower.starts_with("---------- forwarded message")
        || lower.starts_with("-------- original message")
        || lower == "________________________________"
}

/// At most `max` characters (not bytes), and whether anything was cut.
pub fn truncate_chars(text: &str, max: usize) -> (String, bool) {
    match text.char_indices().nth(max) {
        Some((cut, _)) => (text[..cut].to_owned(), true),
        None => (text.to_owned(), false),
    }
}

/// Whether an attachment is a PDF. PDF text comes from the app (PDFKit,
/// through a foreign trait) rather than a Rust parser: Apple's is
/// maintained and hardened, and untrusted PDFs are common in mail.
pub fn is_pdf(mime_type: &str, filename: &str) -> bool {
    mime_type.eq_ignore_ascii_case("application/pdf") || filename.to_ascii_lowercase().ends_with(".pdf")
}

/// Text from an attachment: `text/*` (HTML converted) and `.docx`. See
/// [`is_pdf`] for PDFs.
pub fn extract_attachment_text(bytes: &[u8], mime_type: &str, filename: &str) -> Result<String, ExtractError> {
    let mime = mime_type.to_ascii_lowercase();
    let ext = filename.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    if mime == "text/html" || ext == "html" || ext == "htm" {
        return Ok(crate::html_to_text(&String::from_utf8_lossy(bytes)));
    }
    if mime.starts_with("text/") || matches!(ext.as_str(), "txt" | "csv" | "md" | "ics" | "json" | "xml") {
        return Ok(String::from_utf8_lossy(bytes).into_owned());
    }
    if mime == "application/vnd.openxmlformats-officedocument.wordprocessingml.document" || ext == "docx" {
        return docx_text(bytes);
    }
    Err(ExtractError::Unsupported(if mime.is_empty() { ext } else { mime }))
}

/// Paragraph text from `word/document.xml`.
fn docx_text(bytes: &[u8]) -> Result<String, ExtractError> {
    let unreadable = |e: &dyn std::fmt::Display| ExtractError::Unreadable(e.to_string());
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| unreadable(&e))?;
    let mut xml = String::new();
    let entry = archive.by_name("word/document.xml").map_err(|e| unreadable(&e))?;
    // A zip bomb cannot expand past 20 MB of XML.
    entry.take(20 * 1024 * 1024).read_to_string(&mut xml).map_err(|e| unreadable(&e))?;
    let mut out = String::new();
    let mut rest = xml.as_str();
    while let Some(start) = rest.find('<') {
        out.push_str(&unescape_xml(&rest[..start]));
        let Some(end) = rest[start..].find('>') else { break };
        let tag = &rest[start + 1..start + end];
        if tag == "/w:p" || tag.starts_with("w:br") {
            out.push('\n');
        } else if tag.starts_with("w:tab") && !tag.starts_with("w:tabs") {
            out.push('\t');
        }
        rest = &rest[start + end + 1..];
    }
    Ok(out.trim().to_owned())
}

fn unescape_xml(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    #[test]
    fn quoted_replies_are_removed() {
        let text = "Thursday works.\n\nBest,\nSam\n\nOn Tue, Sep 15, 2026 at 9:00 AM Alex Rivera <alex@example.org>\nwrote:\n> Can we meet?\n> Thanks";
        assert_eq!(strip_quoted(text), "Thursday works.\n\nBest,\nSam");
        assert_eq!(strip_quoted("Yes\n> quoted\nNo"), "Yes\nNo");
        assert_eq!(strip_quoted("FYI\n---------- Forwarded message ---------\nFrom: x"), "FYI");
        assert_eq!(strip_quoted("On second thought, no."), "On second thought, no.");
    }

    #[test]
    fn truncation_counts_characters() {
        assert_eq!(truncate_chars("héllo", 2), ("hé".to_owned(), true));
        assert_eq!(truncate_chars("hi", 5), ("hi".to_owned(), false));
    }

    #[test]
    fn text_html_docx_and_unsupported() {
        assert_eq!(extract_attachment_text(b"a,b\n1,2", "text/csv", "x.csv").unwrap(), "a,b\n1,2");
        assert!(
            extract_attachment_text(b"<p>Hi <b>there</b></p>", "text/html", "x.html").unwrap().contains("Hi there")
        );
        assert_eq!(
            extract_attachment_text(b"\x89PNG", "image/png", "x.png"),
            Err(ExtractError::Unsupported("image/png".into()))
        );

        let mut buf = Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut buf);
            zip.start_file("word/document.xml", zip::write::SimpleFileOptions::default()).unwrap();
            zip.write_all(
                br#"<w:document><w:body><w:p><w:r><w:t>Q3 &amp; Q4</w:t></w:r></w:p><w:p><w:r><w:t>Plan</w:t><w:tab/><w:t>v2</w:t></w:r></w:p></w:body></w:document>"#,
            )
            .unwrap();
            zip.finish().unwrap();
        }
        let text = extract_attachment_text(buf.get_ref(), "application/octet-stream", "plan.docx").unwrap();
        assert_eq!(text, "Q3 & Q4\nPlan\tv2");
        assert!(matches!(extract_attachment_text(b"not a zip", "", "x.docx"), Err(ExtractError::Unreadable(_))));
    }
}
