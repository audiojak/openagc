//! MIME parsing and building, HTML sanitization, text extraction (spec
//! §7.5, §14.4). Every parser bug fix adds a fixture under `fixtures/`.

mod parse;

pub use parse::{
    ParseError, ParsedAttachment, ParsedHeaders, ParsedMessage, decode_text_part, html_to_text, parse, parse_headers,
};
