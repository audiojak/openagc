//! HTML email sanitization (spec §14.4). Runs at sync time; the result is
//! stored and rendered in a WKWebView with JavaScript off and a strict CSP,
//! so this is one of two independent layers.
//!
//! - Only formatting and table tags survive. Scripts, frames, forms, SVG,
//!   `<style>`, `<meta>`, `<base>` and comments are removed.
//! - Inline `style` is kept but filtered to properties that cannot load a
//!   URL or escape the message frame.
//! - Remote images become `openagc-remote:<original url>`: the reader's
//!   scheme handler decides per message whether to fetch them (without
//!   cookies or referrer) or show nothing, so "Load images" needs no
//!   re-sanitizing. `cid:` becomes `openagc-cid:`; small `data:` images stay.
//! - Links keep only http, https and mailto, open externally, and carry
//!   `rel="noopener noreferrer"`.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use ammonia::{Builder, UrlRelative};

/// Bump when the policy changes; stored bodies with an older version are
/// re-sanitized lazily.
pub const SANITIZER_VERSION: u32 = 1;

pub const REMOTE_SCHEME: &str = "openagc-remote";
pub const CID_SCHEME: &str = "openagc-cid";

/// Largest inline `data:` image kept, in bytes of URL.
const MAX_DATA_URL: usize = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sanitized {
    pub html: String,
    pub has_remote_images: bool,
}

pub fn sanitize_html(html: &str) -> Sanitized {
    let html = SANITIZER.clean(html).to_string();
    let has_remote_images = html.contains(&format!("src=\"{REMOTE_SCHEME}:"));
    Sanitized { html, has_remote_images }
}

/// Plain-text bodies rendered through the same reader: escaped, line breaks
/// preserved, bare http(s) URLs linked.
pub fn text_to_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / 8 + 64);
    out.push_str("<div style=\"white-space: pre-wrap\">");
    let mut rest = text;
    while let Some(start) = find_url(rest) {
        escape_into(&mut out, &rest[..start]);
        let tail = &rest[start..];
        let end = tail.find(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '"' | '\'')).unwrap_or(tail.len());
        // Trailing punctuation usually ends the sentence, not the URL.
        let url = tail[..end].trim_end_matches(['.', ',', ')', ';', ':', '!', '?']);
        out.push_str("<a href=\"");
        escape_into(&mut out, url);
        out.push_str("\" target=\"_blank\" rel=\"noopener noreferrer\">");
        escape_into(&mut out, url);
        out.push_str("</a>");
        rest = &tail[url.len()..];
    }
    escape_into(&mut out, rest);
    out.push_str("</div>");
    out
}

fn find_url(s: &str) -> Option<usize> {
    let a = s.find("https://");
    let b = s.find("http://");
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

fn escape_into(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
}

static SANITIZER: LazyLock<Builder<'static>> = LazyLock::new(|| {
    let tags: HashSet<&str> = [
        "a",
        "abbr",
        "address",
        "article",
        "aside",
        "b",
        "bdi",
        "bdo",
        "big",
        "blockquote",
        "br",
        "caption",
        "center",
        "cite",
        "code",
        "col",
        "colgroup",
        "dd",
        "del",
        "details",
        "dfn",
        "div",
        "dl",
        "dt",
        "em",
        "figcaption",
        "figure",
        "font",
        "footer",
        "h1",
        "h2",
        "h3",
        "h4",
        "h5",
        "h6",
        "header",
        "hr",
        "i",
        "img",
        "ins",
        "kbd",
        "li",
        "main",
        "mark",
        "nav",
        "ol",
        "p",
        "pre",
        "q",
        "s",
        "samp",
        "section",
        "small",
        "span",
        "strike",
        "strong",
        "sub",
        "summary",
        "sup",
        "table",
        "tbody",
        "td",
        "tfoot",
        "th",
        "thead",
        "time",
        "tr",
        "tt",
        "u",
        "ul",
        "var",
        "wbr",
    ]
    .into();
    let cell: HashSet<&str> = ["colspan", "rowspan", "align", "valign", "width", "height", "bgcolor", "nowrap"].into();
    let tag_attributes: HashMap<&str, HashSet<&str>> = HashMap::from([
        ("a", ["href", "name"].into()),
        ("img", ["src", "alt", "width", "height", "border"].into()),
        ("table", ["width", "height", "border", "cellpadding", "cellspacing", "bgcolor"].into()),
        ("td", cell.clone()),
        ("th", cell),
        ("tr", ["valign", "bgcolor", "height"].into()),
        ("font", ["color", "face", "size"].into()),
        ("col", ["span", "width"].into()),
        ("colgroup", ["span", "width"].into()),
        ("ol", ["start", "type"].into()),
        ("li", ["value"].into()),
        ("time", ["datetime"].into()),
    ]);
    // Properties that cannot fetch a URL or position content outside the
    // message (no background-image, list-style-image, border-image,
    // content, position, z-index, cursor).
    let styles: HashSet<&str> = [
        "color",
        "background-color",
        "font",
        "font-family",
        "font-size",
        "font-style",
        "font-weight",
        "font-variant",
        "text-align",
        "text-decoration",
        "text-indent",
        "text-transform",
        "line-height",
        "letter-spacing",
        "word-spacing",
        "white-space",
        "word-break",
        "word-wrap",
        "overflow-wrap",
        "vertical-align",
        "direction",
        "margin",
        "margin-top",
        "margin-right",
        "margin-bottom",
        "margin-left",
        "padding",
        "padding-top",
        "padding-right",
        "padding-bottom",
        "padding-left",
        "border",
        "border-top",
        "border-right",
        "border-bottom",
        "border-left",
        "border-color",
        "border-style",
        "border-width",
        "border-radius",
        "border-collapse",
        "border-spacing",
        "width",
        "height",
        "max-width",
        "min-width",
        "max-height",
        "min-height",
        "display",
        "float",
        "clear",
        "list-style-type",
        "table-layout",
        "overflow",
    ]
    .into();

    let mut b = Builder::empty();
    b.tags(tags)
        .tag_attributes(tag_attributes)
        .generic_attributes(["style", "dir", "lang", "title", "align"].into())
        .filter_style_properties(styles)
        .clean_content_tags(["script", "style", "title", "noscript", "template", "iframe", "object"].into())
        .url_schemes(["http", "https", "mailto", "cid", "data"].into())
        .url_relative(UrlRelative::Deny)
        .strip_comments(true)
        .link_rel(Some("noopener noreferrer"))
        .set_tag_attribute_value("a", "target", "_blank")
        .attribute_filter(filter_attribute);
    b
});

fn filter_attribute<'u>(element: &str, attribute: &str, value: &'u str) -> Option<Cow<'u, str>> {
    match (element, attribute) {
        ("img", "src") => {
            let v = value.trim();
            let lower = v.to_ascii_lowercase();
            if lower.starts_with("http://") || lower.starts_with("https://") {
                Some(Cow::Owned(format!("{REMOTE_SCHEME}:{v}")))
            } else if let Some(cid) = lower.strip_prefix("cid:").map(|_| &v[4..]) {
                Some(Cow::Owned(format!("{CID_SCHEME}:{}", cid.trim_matches(['<', '>']))))
            } else if is_safe_data_image(&lower, v.len()) {
                Some(Cow::Borrowed(value))
            } else {
                None
            }
        }
        ("a", "href") => {
            let lower = value.trim().to_ascii_lowercase();
            (lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("mailto:"))
                .then_some(Cow::Borrowed(value))
        }
        (_, "style") => {
            let kept: Vec<&str> =
                value.split(';').map(str::trim).filter(|d| !d.is_empty() && safe_declaration(d)).collect();
            if kept.is_empty() { None } else { Some(Cow::Owned(kept.join("; "))) }
        }
        _ => Some(Cow::Borrowed(value)),
    }
}

/// Property names are already allowlisted; this rejects values that could
/// fetch or execute anything. Backslash escapes are refused outright: they
/// are how filters like this one get bypassed (`u\72l(`).
fn safe_declaration(declaration: &str) -> bool {
    let compact: String = declaration.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_ascii_lowercase();
    !compact.contains('\\')
        && ["url(", "expression(", "javascript:", "vbscript:", "@import", "behavior:", "-moz-binding", "image-set("]
            .iter()
            .all(|bad| !compact.contains(bad))
}

fn is_safe_data_image(lower: &str, len: usize) -> bool {
    len <= MAX_DATA_URL
        && ["data:image/png", "data:image/gif", "data:image/jpeg", "data:image/webp"]
            .iter()
            .any(|p| lower.starts_with(p))
}
