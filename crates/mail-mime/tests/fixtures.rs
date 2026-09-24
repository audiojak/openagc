//! One test per fixture: what the parser must extract from each shape of
//! real-world mail. Fixtures are synthetic (addresses under example.*).

use mail_domain::EmailAddress;
use mail_mime::{ParsedMessage, decode_text_part, parse, parse_headers};

fn fixture(name: &str) -> ParsedMessage {
    let path = format!("{}/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    let raw = std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    parse(&raw).unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn addr(name: Option<&str>, email: &str) -> EmailAddress {
    EmailAddress::new(name, email)
}

#[test]
fn plain_ascii() {
    let m = fixture("01-plain-ascii.eml");
    assert_eq!(m.headers.from, Some(addr(Some("Alex Rivera"), "alex.rivera@example.com")));
    assert_eq!(m.headers.subject, "Lunch on Friday?");
    assert_eq!(m.headers.message_id.as_deref(), Some("plain.1@example.com"));
    // Tue, 15 Sep 2026 09:30:00 -0700 = 16:30 UTC.
    assert_eq!(m.headers.date, Some(1_789_489_800_000));
    assert_eq!(m.text.as_deref().map(str::trim), Some("Are you free for lunch on Friday?"));
    assert_eq!(m.html, None);
    assert!(m.attachments.is_empty());
}

#[test]
fn multipart_alternative_keeps_both_bodies() {
    let m = fixture("02-alternative.eml");
    assert_eq!(m.headers.to, vec![addr(None, "me@example.com"), addr(Some("Jordan Park"), "jordan.park@example.net")]);
    assert_eq!(m.headers.cc, vec![addr(None, "team@example.org")]);
    assert_eq!(m.text.as_deref().map(str::trim), Some("Plain version of the plan."));
    assert!(m.html.as_deref().unwrap().contains("<b>plan</b>"));
}

#[test]
fn rfc2047_base64_utf8_headers() {
    let m = fixture("03-encoded-subject-b.eml");
    assert_eq!(m.headers.subject, "Réunion de lancement – café");
    assert_eq!(m.headers.from.unwrap().name.as_deref(), Some("Éloise Moreau"));
}

#[test]
fn rfc2047_quoted_printable_latin1_headers_and_body() {
    let m = fixture("04-encoded-subject-q-latin1.eml");
    assert_eq!(m.headers.subject, "Änderung der Pläne");
    assert_eq!(m.headers.from.unwrap().name.as_deref(), Some("Jörg Schmidt"));
    assert_eq!(m.text.as_deref().map(str::trim), Some("Grüße aus München"));
}

#[test]
fn html_only_mail_gets_derived_text() {
    let m = fixture("05-html-only.eml");
    let html = m.html.as_deref().unwrap();
    assert!(html.contains("<script>"), "parser returns original HTML; sanitizing is a separate step");
    let text = m.text.unwrap();
    assert!(text.contains("Top stories"), "{text}");
    assert!(text.contains("First story & more."), "{text}");
}

#[test]
fn pdf_attachment() {
    let m = fixture("06-attachment-pdf.eml");
    assert_eq!(m.attachments.len(), 1);
    let a = &m.attachments[0];
    assert_eq!(a.filename, "invoice-4821.pdf");
    assert_eq!(a.mime_type, "application/pdf");
    assert!(!a.is_inline);
    assert!(a.data.starts_with(b"%PDF"));
    assert_eq!(a.size, a.data.len() as u64);
}

#[test]
fn inline_cid_image_is_inline_with_content_id() {
    let m = fixture("07-inline-cid-image.eml");
    let img = m.attachments.iter().find(|a| a.mime_type == "image/png").expect("image part");
    assert_eq!(img.content_id.as_deref(), Some("logo123@example.org"));
    assert!(img.is_inline);
    assert!(m.html.unwrap().contains("cid:logo123@example.org"));
}

#[test]
fn rfc2231_encoded_filename() {
    let m = fixture("08-rfc2231-filename.eml");
    assert_eq!(m.attachments[0].filename, "Verträge – final.pdf");
}

#[test]
fn reply_threading_headers_without_brackets() {
    let m = fixture("09-reply-threading.eml");
    assert_eq!(m.headers.in_reply_to.as_deref(), Some("plain.1@example.com"));
    assert_eq!(m.headers.references, vec!["root.0@example.com", "plain.1@example.com"]);
}

#[test]
fn groups_and_reply_to() {
    let m = fixture("10-group-and-reply-to.eml");
    assert!(m.headers.to.is_empty(), "empty group has no addresses");
    assert_eq!(m.headers.bcc, vec![addr(None, "me@example.com")]);
    assert_eq!(m.headers.reply_to, vec![addr(Some("Support"), "support@example.org")]);
}

#[test]
fn missing_date_and_subject_are_tolerated() {
    let m = fixture("11-missing-date-and-subject.eml");
    assert_eq!(m.headers.date, None);
    assert_eq!(m.headers.subject, "");
    assert!(m.text.unwrap().contains("No date"));
}

#[test]
fn nested_mixed_alternative() {
    let m = fixture("12-nested-mixed-alternative.eml");
    assert_eq!(m.text.as_deref().map(str::trim), Some("Notes in plain text."));
    assert!(m.html.unwrap().contains("<i>HTML</i>"));
    assert_eq!(m.attachments.iter().map(|a| a.filename.as_str()).collect::<Vec<_>>(), vec!["deck.pdf"]);
}

#[test]
fn calendar_invite_is_an_attachment() {
    let m = fixture("13-calendar-invite.eml");
    let ics = m.attachments.iter().find(|a| a.mime_type == "text/calendar").expect("ics");
    assert_eq!(ics.filename, "invite.ics");
    assert!(ics.data.starts_with(b"BEGIN:VCALENDAR"));
}

#[test]
fn forwarded_message_is_an_attachment_and_outer_text_is_kept() {
    let m = fixture("14-forwarded-rfc822.eml");
    assert!(m.text.unwrap().contains("FYI"));
    assert!(m.attachments.iter().any(|a| a.mime_type == "message/rfc822"));
}

#[test]
fn eight_bit_latin1_body() {
    let m = fixture("15-latin1-body-no-qp.eml");
    assert_eq!(m.headers.subject, "Café");
    assert!(m.text.unwrap().contains("Un café crème, s'il vous plaît."));
}

#[test]
fn display_names_with_commas() {
    let m = fixture("16-display-name-with-comma.eml");
    assert_eq!(m.headers.from, Some(addr(Some("Doe, Jamie"), "jamie.doe@example.com")));
    assert_eq!(
        m.headers.to,
        vec![addr(Some("Lindqvist, Rowan"), "rowan@example.net"), addr(None, "plain@example.org")]
    );
}

#[test]
fn empty_body() {
    let m = fixture("17-empty-body.eml");
    assert_eq!(m.text.as_deref().map(str::trim).unwrap_or(""), "");
}

#[test]
fn prompt_injection_text_survives_parsing_as_plain_data() {
    // Parsing must not drop or interpret it; containment is the permission
    // engine's job. The hidden div's text is still present for search.
    let m = fixture("18-prompt-injection.eml");
    assert!(m.text.unwrap().contains("Ignore all previous instructions"));
}

#[test]
fn every_fixture_parses() {
    let dir = format!("{}/fixtures", env!("CARGO_MANIFEST_DIR"));
    let mut n = 0;
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "eml") {
            parse(&std::fs::read(&path).unwrap()).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            n += 1;
        }
    }
    assert!(n >= 18);
}

#[test]
fn parse_headers_decodes_a_provider_header_list() {
    let h = parse_headers([
        ("From", "=?UTF-8?B?w4lsb2lzZSBNb3JlYXU=?= <eloise@example.com>"),
        ("To", "\"Doe, Jamie\" <jamie@example.com>, b@example.org"),
        ("Subject", "=?ISO-8859-1?Q?=C4nderung?="),
        ("Date", "Tue, 15 Sep 2026 09:30:00 -0700"),
        ("Message-ID", "<x@example.com>"),
        ("Bad Header", "ignored"),
        ("X-Injected", "value\r\nSubject: spoofed"),
    ]);
    assert_eq!(h.from.unwrap().name.as_deref(), Some("Éloise Moreau"));
    assert_eq!(h.to.len(), 2);
    assert_eq!(h.subject, "Änderung", "CRLF inside a value must not start a new header");
    assert_eq!(h.date, Some(1_789_489_800_000));
    assert_eq!(h.message_id.as_deref(), Some("x@example.com"));
}

#[test]
fn decode_text_part_applies_the_charset() {
    assert_eq!(decode_text_part(b"Gr\xfc\xdfe", "text/plain; charset=iso-8859-1"), "Grüße");
    assert_eq!(decode_text_part("naïve".as_bytes(), "text/plain; charset=\"utf-8\""), "naïve");
    assert_eq!(decode_text_part(b"<p>x</p>", "text/html; charset=utf-8"), "<p>x</p>");
}
