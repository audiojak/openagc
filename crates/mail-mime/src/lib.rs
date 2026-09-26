//! MIME parsing and building, HTML sanitization, text extraction (spec
//! §7.5, §14.4). Every parser bug fix adds a fixture under `fixtures/`.

mod build;
pub mod mbox;
mod parse;
mod sanitize;
mod text;

pub use build::{
    BuildError, OutgoingAttachment, OutgoingMessage, build, build_draft, forward_subject, reply_recipients,
    reply_references, reply_subject,
};
pub use parse::{
    ParseError, ParsedAttachment, ParsedHeaders, ParsedMessage, decode_text_part, html_to_text, parse, parse_headers,
};
pub use sanitize::{CID_SCHEME, REMOTE_SCHEME, SANITIZER_VERSION, Sanitized, sanitize_html, text_to_html};
pub use text::{ExtractError, extract_attachment_text, is_pdf, markdown_to_html, strip_quoted, truncate_chars};
