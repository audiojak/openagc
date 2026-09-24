//! Parsing with `mail-parser` (RFC 5322, 2045–2049, 2047, 2231, 6532).
//!
//! Two entry points: [`parse`] for a complete RFC 822 message (raw fetches,
//! drafts, fixtures), and the header/part helpers a provider uses when it
//! receives a message already split into parts (Gmail `format=full`). The
//! helpers feed small synthetic snippets through the same parser so there is
//! one decoder for encoded words, address lists, dates and charsets.

use mail_domain::{EmailAddress, Millis};
use mail_parser::{Address, HeaderValue, MessageParser, MimeHeaders, PartType};

/// A message as parsed from MIME. `html` is the original, unsanitized HTML.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedMessage {
    pub headers: ParsedHeaders,
    pub text: Option<String>,
    pub html: Option<String>,
    pub attachments: Vec<ParsedAttachment>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedHeaders {
    /// Without angle brackets.
    pub message_id: Option<String>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub from: Option<EmailAddress>,
    pub to: Vec<EmailAddress>,
    pub cc: Vec<EmailAddress>,
    pub bcc: Vec<EmailAddress>,
    pub reply_to: Vec<EmailAddress>,
    pub subject: String,
    pub date: Option<Millis>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedAttachment {
    pub filename: String,
    pub mime_type: String,
    pub size: u64,
    /// Without angle brackets.
    pub content_id: Option<String>,
    pub is_inline: bool,
    pub data: Vec<u8>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ParseError {
    #[error("not a parseable MIME message")]
    Unparseable,
}

/// Parse a complete RFC 822 message.
pub fn parse(raw: &[u8]) -> Result<ParsedMessage, ParseError> {
    let raw = repair_8bit_headers(raw);
    let msg = MessageParser::default().parse(raw.as_ref()).ok_or(ParseError::Unparseable)?;

    let mut texts = Vec::new();
    let mut htmls = Vec::new();
    for &id in &msg.text_body {
        if let Some(part) = msg.parts.get(id as usize) {
            match &part.body {
                PartType::Text(t) => texts.push(t.to_string()),
                PartType::Html(h) => htmls.push(h.to_string()),
                _ => {}
            }
        }
    }
    // An HTML body counts only if a real text/html part exists; mail-parser
    // otherwise synthesizes HTML from the text part.
    let mut real_html = Vec::new();
    for &id in &msg.html_body {
        if let Some(part) = msg.parts.get(id as usize) {
            if let PartType::Html(h) = &part.body {
                real_html.push(h.to_string());
            }
        }
    }
    let html = if real_html.is_empty() { None } else { Some(real_html.join("\n<hr>\n")) };
    let text = if texts.is_empty() {
        // HTML-only mail: derive text for search and agents.
        msg.body_text(0).map(|t| t.into_owned()).or_else(|| htmls.first().map(|h| html_to_text(h)))
    } else {
        Some(texts.join("\n\n"))
    };

    let attachments = msg
        .attachments()
        .map(|part| {
            let data = part.contents().to_vec();
            let mime_type = part
                .content_type()
                .map(|ct| match &ct.c_subtype {
                    Some(sub) => format!("{}/{}", ct.c_type, sub).to_lowercase(),
                    None => ct.c_type.to_lowercase(),
                })
                .unwrap_or_else(|| "application/octet-stream".into());
            let content_id = part.content_id().map(strip_brackets);
            let is_inline = part.content_disposition().is_some_and(|d| d.is_inline())
                || (content_id.is_some() && part.content_disposition().is_none_or(|d| !d.is_attachment()));
            ParsedAttachment {
                filename: part.attachment_name().map(str::to_owned).unwrap_or_else(|| default_filename(&mime_type)),
                mime_type,
                size: data.len() as u64,
                content_id,
                is_inline,
                data,
            }
        })
        .collect();

    Ok(ParsedMessage { headers: headers_of(&msg), text, html, attachments })
}

/// Decode a provider's raw header list (names and undecoded values, as
/// Gmail's `payload.headers` gives them).
pub fn parse_headers<'a>(headers: impl IntoIterator<Item = (&'a str, &'a str)>) -> ParsedHeaders {
    let mut raw = Vec::new();
    for (name, value) in headers {
        // Header names are tokens; drop anything that could break framing.
        if name.is_empty() || name.bytes().any(|b| b <= b' ' || b == b':' || b >= 127) {
            continue;
        }
        raw.extend_from_slice(name.as_bytes());
        raw.extend_from_slice(b": ");
        raw.extend(value.bytes().filter(|&b| b != b'\r' && b != b'\n'));
        raw.extend_from_slice(b"\r\n");
    }
    raw.extend_from_slice(b"\r\n");
    MessageParser::default().parse(&raw).map(|m| headers_of(&m)).unwrap_or_default()
}

/// Decode one body part's bytes (already transfer-decoded, as Gmail's
/// `body.data` is) using the charset in its `Content-Type` header value.
pub fn decode_text_part(data: &[u8], content_type: &str) -> String {
    let content_type: String = content_type.chars().filter(|c| *c != '\r' && *c != '\n').collect();
    let mut raw = format!("Content-Type: {content_type}\r\nContent-Transfer-Encoding: 8bit\r\n\r\n").into_bytes();
    raw.extend_from_slice(data);
    let Some(msg) = MessageParser::default().parse(&raw) else {
        return String::from_utf8_lossy(data).into_owned();
    };
    match msg.parts.first().map(|p| &p.body) {
        Some(PartType::Text(t)) | Some(PartType::Html(t)) => t.to_string(),
        _ => String::from_utf8_lossy(data).into_owned(),
    }
}

/// Plain text from HTML, for search, agents and the text/plain alternative.
pub fn html_to_text(html: &str) -> String {
    mail_parser::decoders::html::html_to_text(html)
}

/// Headers must be ASCII (or RFC 2047 / RFC 6532 UTF-8), but old mailers
/// send raw 8-bit Latin-1. Bytes in the header block that are not valid
/// UTF-8 are decoded as Latin-1; valid UTF-8 and the body are untouched.
fn repair_8bit_headers(raw: &[u8]) -> std::borrow::Cow<'_, [u8]> {
    let end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .or_else(|| raw.windows(2).position(|w| w == b"\n\n"))
        .unwrap_or(raw.len());
    let (head, body) = raw.split_at(end);
    if std::str::from_utf8(head).is_ok() {
        return std::borrow::Cow::Borrowed(raw);
    }
    let mut fixed = String::with_capacity(head.len() + 16);
    let mut rest = head;
    while !rest.is_empty() {
        match std::str::from_utf8(rest) {
            Ok(valid) => {
                fixed.push_str(valid);
                break;
            }
            Err(e) => {
                let (good, bad) = rest.split_at(e.valid_up_to());
                fixed.push_str(std::str::from_utf8(good).unwrap_or_default());
                let n = e.error_len().unwrap_or(bad.len());
                fixed.extend(bad[..n].iter().map(|&b| char::from(b)));
                rest = &bad[n..];
            }
        }
    }
    let mut out = fixed.into_bytes();
    out.extend_from_slice(body);
    std::borrow::Cow::Owned(out)
}

fn headers_of(msg: &mail_parser::Message<'_>) -> ParsedHeaders {
    ParsedHeaders {
        message_id: msg.message_id().map(strip_brackets),
        in_reply_to: text_list(msg.in_reply_to()).into_iter().next(),
        references: text_list(msg.references()),
        from: msg.from().and_then(|a| addresses(a).into_iter().next()),
        to: msg.to().map(addresses).unwrap_or_default(),
        cc: msg.cc().map(addresses).unwrap_or_default(),
        bcc: msg.bcc().map(addresses).unwrap_or_default(),
        reply_to: msg.reply_to().map(addresses).unwrap_or_default(),
        subject: msg.subject().unwrap_or_default().trim().to_owned(),
        date: msg.date().map(|d| d.to_timestamp() * 1000),
    }
}

fn addresses(a: &Address<'_>) -> Vec<EmailAddress> {
    a.iter()
        .filter_map(|addr| {
            let email = addr.address.as_deref()?.trim();
            (!email.is_empty()).then(|| EmailAddress::new(addr.name.as_deref(), email))
        })
        .collect()
}

fn text_list(v: &HeaderValue<'_>) -> Vec<String> {
    let items: Vec<String> = match v {
        HeaderValue::Text(t) => vec![t.to_string()],
        HeaderValue::TextList(list) => list.iter().map(|t| t.to_string()).collect(),
        _ => vec![],
    };
    items.iter().map(|s| strip_brackets(s)).filter(|s| !s.is_empty()).collect()
}

fn strip_brackets(s: &str) -> String {
    s.trim().trim_start_matches('<').trim_end_matches('>').to_owned()
}

fn default_filename(mime_type: &str) -> String {
    let ext = match mime_type {
        "application/pdf" => "pdf",
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "text/calendar" => "ics",
        "text/plain" => "txt",
        "message/rfc822" => "eml",
        _ => "bin",
    };
    format!("attachment.{ext}")
}
