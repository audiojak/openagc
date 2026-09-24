//! Gmail REST types (only the fields OpenAGC reads) and the `format=full`
//! payload walker.

use base64::Engine;
use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
use mail_domain::{LabelId, MessageId, ThreadId};
use provider_api::{FetchedAttachment, FetchedBody, FetchedMessage};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Profile {
    pub email_address: String,
    #[serde(default)]
    pub messages_total: u64,
    pub history_id: String,
}

#[derive(Debug, Deserialize)]
pub struct LabelList {
    #[serde(default)]
    pub labels: Vec<Label>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Label {
    pub id: String,
    pub name: String,
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub label_list_visibility: Option<String>,
    #[serde(default)]
    pub color: Option<LabelColor>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LabelColor {
    pub background_color: String,
    pub text_color: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageList {
    #[serde(default)]
    pub messages: Vec<MessageRef>,
    #[serde(default)]
    pub next_page_token: Option<String>,
    #[serde(default)]
    pub result_size_estimate: Option<u64>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct MessageRef {
    pub id: String,
    #[serde(default)]
    pub thread_id: String,
    #[serde(default)]
    pub label_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryList {
    #[serde(default)]
    pub history: Vec<History>,
    #[serde(default)]
    pub next_page_token: Option<String>,
    pub history_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct History {
    #[serde(default)]
    pub messages_added: Vec<MessageWrapper>,
    #[serde(default)]
    pub messages_deleted: Vec<MessageWrapper>,
    #[serde(default)]
    pub labels_added: Vec<LabelChange>,
    #[serde(default)]
    pub labels_removed: Vec<LabelChange>,
}

#[derive(Debug, Deserialize)]
pub struct MessageWrapper {
    pub message: MessageRef,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LabelChange {
    pub message: MessageRef,
    #[serde(default)]
    pub label_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    pub id: String,
    pub thread_id: String,
    #[serde(default)]
    pub label_ids: Vec<String>,
    #[serde(default)]
    pub snippet: String,
    #[serde(default)]
    pub internal_date: Option<String>,
    #[serde(default)]
    pub size_estimate: u64,
    #[serde(default)]
    pub payload: Option<Part>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Part {
    #[serde(default)]
    pub part_id: Option<String>,
    #[serde(default)]
    pub mime_type: String,
    #[serde(default)]
    pub filename: String,
    #[serde(default)]
    pub headers: Vec<Header>,
    #[serde(default)]
    pub body: Option<PartBody>,
    #[serde(default)]
    pub parts: Vec<Part>,
}

#[derive(Debug, Deserialize)]
pub struct Header {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PartBody {
    #[serde(default)]
    pub attachment_id: Option<String>,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub data: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AttachmentData {
    pub data: String,
}

#[derive(Debug, Deserialize)]
pub struct SentMessage {
    pub id: String,
}

/// Gmail uses URL-safe base64, sometimes padded, sometimes not.
pub fn decode_base64url(data: &str) -> Option<Vec<u8>> {
    let trimmed = data.trim();
    URL_SAFE_NO_PAD.decode(trimmed.trim_end_matches('=')).ok().or_else(|| URL_SAFE.decode(trimmed).ok())
}

pub fn encode_base64url(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

impl Part {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|h| h.name.eq_ignore_ascii_case(name)).map(|h| h.value.as_str())
    }
}

/// Convert a `format=full` (or `format=metadata`) message.
pub fn to_fetched(m: Message) -> FetchedMessage {
    let payload = m.payload.unwrap_or_default();
    let headers = mail_mime::parse_headers(payload.headers.iter().map(|h| (h.name.as_str(), h.value.as_str())));
    let has_body = has_content(&payload);
    let body = has_body.then(|| {
        let mut walk = Walk::default();
        walk.visit(&payload, false);
        let text = if walk.texts.is_empty() {
            walk.htmls.first().map(|h| mail_mime::html_to_text(h))
        } else {
            Some(walk.texts.join("\n\n"))
        };
        let html = (!walk.htmls.is_empty()).then(|| walk.htmls.join("\n<hr>\n"));
        FetchedBody { text, html, attachments: walk.attachments }
    });
    FetchedMessage {
        id: MessageId(m.id),
        thread_id: ThreadId(m.thread_id),
        label_ids: m.label_ids.into_iter().map(LabelId).collect(),
        snippet: decode_snippet(&m.snippet),
        internal_date: m.internal_date.and_then(|d| d.parse().ok()).unwrap_or_default(),
        size_estimate: m.size_estimate,
        message_id_header: headers.message_id,
        in_reply_to: headers.in_reply_to,
        references: headers.references,
        from: headers.from,
        to: headers.to,
        cc: headers.cc,
        bcc: headers.bcc,
        reply_to: headers.reply_to,
        subject: headers.subject,
        date: headers.date,
        body,
    }
}

/// A metadata fetch has headers but no part bodies.
fn has_content(p: &Part) -> bool {
    p.body.as_ref().is_some_and(|b| b.data.is_some() || b.attachment_id.is_some()) || p.parts.iter().any(has_content)
}

#[derive(Default)]
struct Walk {
    texts: Vec<String>,
    htmls: Vec<String>,
    attachments: Vec<FetchedAttachment>,
}

impl Walk {
    /// `in_attachment`: inside a forwarded message/rfc822 or similar; its
    /// text parts belong to the attachment, not this message's body.
    fn visit(&mut self, part: &Part, in_attachment: bool) {
        let mime = part.mime_type.to_ascii_lowercase();
        let disposition = part.header("Content-Disposition").unwrap_or_default().to_ascii_lowercase();
        let content_id = part.header("Content-ID").map(|c| c.trim().trim_matches(['<', '>']).to_owned());
        let is_attachment_disposition = disposition.starts_with("attachment");
        let body = part.body.as_ref();

        if mime.starts_with("multipart/") {
            for child in &part.parts {
                self.visit(child, in_attachment);
            }
            return;
        }
        let is_body_text = !in_attachment
            && part.filename.is_empty()
            && !is_attachment_disposition
            && (mime == "text/plain" || mime == "text/html");
        if is_body_text {
            if let Some(bytes) = body.and_then(|b| b.data.as_deref()).and_then(decode_base64url) {
                let content_type = part.header("Content-Type").unwrap_or(&part.mime_type);
                let text = mail_mime::decode_text_part(&bytes, content_type);
                if mime == "text/html" { self.htmls.push(text) } else { self.texts.push(text) }
            }
            return;
        }
        if in_attachment {
            return;
        }
        let Some(body) = body else { return };
        if body.attachment_id.is_none() && body.data.is_none() {
            return;
        }
        let is_inline = disposition.starts_with("inline") || (content_id.is_some() && !is_attachment_disposition);
        self.attachments.push(FetchedAttachment {
            part_id: part.part_id.clone(),
            attachment_id: body.attachment_id.clone(),
            filename: if part.filename.is_empty() { default_name(&mime) } else { part.filename.clone() },
            mime_type: if mime.is_empty() { "application/octet-stream".into() } else { mime.clone() },
            size: body.size,
            content_id,
            is_inline,
        });
        // A forwarded message's own parts are not part of this body.
        if mime == "message/rfc822" {
            for child in &part.parts {
                self.visit(child, true);
            }
        }
    }
}

fn default_name(mime: &str) -> String {
    match mime {
        "message/rfc822" => "forwarded-message.eml".into(),
        "text/calendar" => "invite.ics".into(),
        _ => "attachment".into(),
    }
}

/// Gmail snippets are HTML-escaped.
fn decode_snippet(s: &str) -> String {
    s.replace("&#39;", "'").replace("&quot;", "\"").replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&")
}
