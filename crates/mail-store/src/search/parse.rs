//! A Gmail-compatible search language (spec §8), parsed by hand into a
//! [`SearchExpr`] tree. The same tree is what agents pass as JSON to
//! `mail.search`, so people and agents share one query model.
//!
//! ```text
//! query   := or
//! or      := and ("OR" and)*
//! and     := unary+                      (juxtaposition)
//! unary   := "-" unary | atom
//! atom    := "(" or ")" | field ":" value | "\"phrase\"" | word
//! ```

use serde::{Deserialize, Serialize};

use mail_domain::Millis;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum SearchExpr {
    And {
        all: Vec<SearchExpr>,
    },
    Or {
        any: Vec<SearchExpr>,
    },
    Not {
        expr: Box<SearchExpr>,
    },
    /// A word anywhere in subject, addresses, body or attachment names.
    Text {
        value: String,
    },
    /// An exact phrase anywhere.
    Phrase {
        value: String,
    },
    From {
        value: String,
    },
    To {
        value: String,
    },
    Cc {
        value: String,
    },
    Bcc {
        value: String,
    },
    Subject {
        value: String,
    },
    Label {
        value: String,
    },
    In {
        mailbox: Mailbox,
    },
    Is {
        flag: Flag,
    },
    HasAttachment,
    Filename {
        value: String,
    },
    /// On or after this instant.
    After {
        at: Millis,
    },
    /// Strictly before this instant.
    Before {
        at: Millis,
    },
    NewerThan {
        days: u32,
    },
    OlderThan {
        days: u32,
    },
    Larger {
        bytes: u64,
    },
    Smaller {
        bytes: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mailbox {
    Inbox,
    Archive,
    Sent,
    Drafts,
    Trash,
    Spam,
    Starred,
    /// Everything, including spam and trash.
    Anywhere,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Flag {
    Unread,
    Read,
    Starred,
    Important,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message} (at character {position})")]
pub struct ParseError {
    pub message: String,
    pub position: usize,
}

impl SearchExpr {
    /// True if the query explicitly asks for spam or trash; otherwise
    /// search excludes them, as Gmail does.
    pub fn mentions_spam_or_trash(&self) -> bool {
        match self {
            Self::And { all } => all.iter().any(Self::mentions_spam_or_trash),
            Self::Or { any } => any.iter().any(Self::mentions_spam_or_trash),
            Self::Not { expr } => expr.mentions_spam_or_trash(),
            Self::In { mailbox } => matches!(mailbox, Mailbox::Trash | Mailbox::Spam | Mailbox::Anywhere),
            Self::Label { value } => {
                let v = value.to_ascii_lowercase();
                v == "spam" || v == "trash"
            }
            _ => false,
        }
    }

    pub fn is_empty(&self) -> bool {
        matches!(self, Self::And { all } if all.is_empty())
    }
}

/// Parse a query string. An empty query parses to an empty `And`.
pub fn parse(input: &str) -> Result<SearchExpr, ParseError> {
    let tokens = tokenize(input)?;
    let mut p = Parser { tokens, pos: 0 };
    if p.tokens.is_empty() {
        return Ok(SearchExpr::And { all: vec![] });
    }
    let expr = p.or()?;
    if let Some(t) = p.peek() {
        return Err(ParseError { message: format!("unexpected {}", t.kind.describe()), position: t.at });
    }
    Ok(expr)
}

#[derive(Debug, Clone, PartialEq)]
enum Kind {
    Word(String),
    Quoted(String),
    /// `field:` immediately followed by its value token.
    Field(String),
    Minus,
    Open,
    Close,
    Or,
}

impl Kind {
    fn describe(&self) -> String {
        match self {
            Kind::Word(w) => format!("\"{w}\""),
            Kind::Quoted(q) => format!("\"{q}\""),
            Kind::Field(f) => format!("{f}:"),
            Kind::Minus => "-".into(),
            Kind::Open => "(".into(),
            Kind::Close => ")".into(),
            Kind::Or => "OR".into(),
        }
    }
}

#[derive(Debug, Clone)]
struct Token {
    kind: Kind,
    at: usize,
}

const FIELDS: &[&str] = &[
    "from",
    "to",
    "cc",
    "bcc",
    "subject",
    "label",
    "in",
    "is",
    "has",
    "filename",
    "after",
    "before",
    "older_than",
    "newer_than",
    "larger",
    "smaller",
];

fn tokenize(input: &str) -> Result<Vec<Token>, ParseError> {
    let chars: Vec<(usize, char)> = input.char_indices().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let (at, c) = chars[i];
        match c {
            c if c.is_whitespace() => i += 1,
            '(' => {
                tokens.push(Token { kind: Kind::Open, at });
                i += 1;
            }
            ')' => {
                tokens.push(Token { kind: Kind::Close, at });
                i += 1;
            }
            // A leading `-` negates; inside a word (`e-mail`) it is text.
            '-' if i + 1 < chars.len() && !chars[i + 1].1.is_whitespace() => {
                tokens.push(Token { kind: Kind::Minus, at });
                i += 1;
            }
            '"' => {
                let (value, next) = read_quoted(&chars, i)?;
                tokens.push(Token { kind: Kind::Quoted(value), at });
                i = next;
            }
            _ => {
                let start = i;
                while i < chars.len() && !chars[i].1.is_whitespace() && !matches!(chars[i].1, '(' | ')' | '"') {
                    i += 1;
                }
                let word: String = chars[start..i].iter().map(|(_, c)| *c).collect();
                if let Some((field, rest)) = word.split_once(':')
                    && FIELDS.contains(&field.to_ascii_lowercase().as_str())
                {
                    tokens.push(Token { kind: Kind::Field(field.to_ascii_lowercase()), at });
                    if !rest.is_empty() {
                        tokens.push(Token { kind: Kind::Word(rest.to_owned()), at: at + field.len() + 1 });
                    } else if i < chars.len() && chars[i].1 == '"' {
                        let (value, next) = read_quoted(&chars, i)?;
                        tokens.push(Token { kind: Kind::Quoted(value), at: chars[i].0 });
                        i = next;
                    } else {
                        return Err(ParseError { message: format!("{field}: needs a value"), position: at });
                    }
                } else if word == "OR" || word == "|" {
                    tokens.push(Token { kind: Kind::Or, at });
                } else {
                    tokens.push(Token { kind: Kind::Word(word), at });
                }
            }
        }
    }
    Ok(tokens)
}

fn read_quoted(chars: &[(usize, char)], open: usize) -> Result<(String, usize), ParseError> {
    let mut i = open + 1;
    let mut value = String::new();
    while i < chars.len() && chars[i].1 != '"' {
        value.push(chars[i].1);
        i += 1;
    }
    if i >= chars.len() {
        return Err(ParseError { message: "unclosed quote".into(), position: chars[open].0 });
    }
    Ok((value, i + 1))
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn next(&mut self) -> Option<Token> {
        let t = self.tokens.get(self.pos).cloned();
        self.pos += 1;
        t
    }

    fn or(&mut self) -> Result<SearchExpr, ParseError> {
        let mut any = vec![self.and()?];
        while matches!(self.peek().map(|t| &t.kind), Some(Kind::Or)) {
            self.pos += 1;
            any.push(self.and()?);
        }
        Ok(if any.len() == 1 { any.remove(0) } else { SearchExpr::Or { any } })
    }

    fn and(&mut self) -> Result<SearchExpr, ParseError> {
        let mut all = Vec::new();
        while let Some(t) = self.peek() {
            if matches!(t.kind, Kind::Or | Kind::Close) {
                break;
            }
            all.push(self.unary()?);
        }
        if all.is_empty() {
            let at = self.peek().map(|t| t.at).unwrap_or(0);
            return Err(ParseError { message: "expected a search term".into(), position: at });
        }
        Ok(if all.len() == 1 { all.remove(0) } else { SearchExpr::And { all } })
    }

    fn unary(&mut self) -> Result<SearchExpr, ParseError> {
        if matches!(self.peek().map(|t| &t.kind), Some(Kind::Minus)) {
            self.pos += 1;
            return Ok(SearchExpr::Not { expr: Box::new(self.unary()?) });
        }
        self.atom()
    }

    fn atom(&mut self) -> Result<SearchExpr, ParseError> {
        let t = self.next().ok_or(ParseError { message: "unexpected end of query".into(), position: 0 })?;
        match t.kind {
            Kind::Open => {
                let inner = self.or()?;
                match self.next() {
                    Some(Token { kind: Kind::Close, .. }) => Ok(inner),
                    _ => Err(ParseError { message: "missing )".into(), position: t.at }),
                }
            }
            Kind::Quoted(q) => Ok(SearchExpr::Phrase { value: q }),
            Kind::Word(w) => Ok(SearchExpr::Text { value: w }),
            Kind::Field(field) => {
                let value = match self.next() {
                    Some(Token { kind: Kind::Word(v) | Kind::Quoted(v), .. }) => v,
                    _ => return Err(ParseError { message: format!("{field}: needs a value"), position: t.at }),
                };
                field_expr(&field, &value).map_err(|message| ParseError { message, position: t.at })
            }
            other => Err(ParseError { message: format!("unexpected {}", other.describe()), position: t.at }),
        }
    }
}

fn field_expr(field: &str, value: &str) -> Result<SearchExpr, String> {
    let v = value.to_owned();
    let lower = value.to_ascii_lowercase();
    Ok(match field {
        "from" => SearchExpr::From { value: v },
        "to" => SearchExpr::To { value: v },
        "cc" => SearchExpr::Cc { value: v },
        "bcc" => SearchExpr::Bcc { value: v },
        "subject" => SearchExpr::Subject { value: v },
        "label" => SearchExpr::Label { value: v },
        "filename" => SearchExpr::Filename { value: v },
        "in" => SearchExpr::In {
            mailbox: match lower.as_str() {
                "inbox" => Mailbox::Inbox,
                "archive" | "archived" => Mailbox::Archive,
                "sent" => Mailbox::Sent,
                "drafts" | "draft" => Mailbox::Drafts,
                "trash" => Mailbox::Trash,
                "spam" => Mailbox::Spam,
                "starred" => Mailbox::Starred,
                "anywhere" | "all" => Mailbox::Anywhere,
                _ => return Err(format!("unknown mailbox {value:?}")),
            },
        },
        "is" => match lower.as_str() {
            "unread" => SearchExpr::Is { flag: Flag::Unread },
            "read" => SearchExpr::Is { flag: Flag::Read },
            "starred" => SearchExpr::Is { flag: Flag::Starred },
            "important" => SearchExpr::Is { flag: Flag::Important },
            _ => return Err(format!("unknown is:{value}")),
        },
        "has" => match lower.as_str() {
            "attachment" | "attachments" => SearchExpr::HasAttachment,
            _ => return Err(format!("unknown has:{value}")),
        },
        "after" => SearchExpr::After { at: parse_date(value)? },
        "before" => SearchExpr::Before { at: parse_date(value)? },
        "newer_than" => SearchExpr::NewerThan { days: parse_age(value)? },
        "older_than" => SearchExpr::OlderThan { days: parse_age(value)? },
        "larger" => SearchExpr::Larger { bytes: parse_size(value)? },
        "smaller" => SearchExpr::Smaller { bytes: parse_size(value)? },
        _ => return Err(format!("unknown field {field}")),
    })
}

/// `2026-01-31` or `2026/01/31`, as UTC midnight.
fn parse_date(value: &str) -> Result<Millis, String> {
    let parts: Vec<&str> = value.split(['-', '/']).collect();
    let [y, m, d] = parts.as_slice() else { return Err(format!("dates look like 2026-01-31, not {value:?}")) };
    let (y, m, d): (i64, i64, i64) = (
        y.parse().map_err(|_| format!("bad year in {value:?}"))?,
        m.parse().map_err(|_| format!("bad month in {value:?}"))?,
        d.parse().map_err(|_| format!("bad day in {value:?}"))?,
    );
    if !(1970..=9999).contains(&y) || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return Err(format!("{value:?} is not a date"));
    }
    Ok(days_from_civil(y, m, d) * 86_400_000)
}

/// Days since 1970-01-01 (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `7d`, `2m` (30-day months), `1y`.
fn parse_age(value: &str) -> Result<u32, String> {
    let (n, unit) = value.split_at(value.len().saturating_sub(1));
    let n: u32 = n.parse().map_err(|_| format!("ages look like 7d, 2m or 1y, not {value:?}"))?;
    let days = match unit.to_ascii_lowercase().as_str() {
        "d" => n,
        "m" => n.saturating_mul(30),
        "y" => n.saturating_mul(365),
        _ => return Err(format!("ages look like 7d, 2m or 1y, not {value:?}")),
    };
    Ok(days)
}

/// `5M`, `200K`, `1G`, or bytes.
fn parse_size(value: &str) -> Result<u64, String> {
    let lower = value.to_ascii_lowercase();
    let (n, mult) = match lower.chars().last() {
        Some('k') => (&lower[..lower.len() - 1], 1024),
        Some('m') => (&lower[..lower.len() - 1], 1024 * 1024),
        Some('g') => (&lower[..lower.len() - 1], 1024 * 1024 * 1024),
        _ => (lower.as_str(), 1),
    };
    let n: u64 = n.parse().map_err(|_| format!("sizes look like 5M or 200K, not {value:?}"))?;
    Ok(n.saturating_mul(mult))
}

#[cfg(test)]
mod tests {
    use super::*;
    use SearchExpr::*;

    fn text(s: &str) -> SearchExpr {
        Text { value: s.into() }
    }

    #[test]
    fn words_are_anded() {
        assert_eq!(parse("quarterly report").unwrap(), And { all: vec![text("quarterly"), text("report")] });
        assert_eq!(parse("  ").unwrap(), And { all: vec![] });
    }

    #[test]
    fn fields_phrases_and_negation() {
        assert_eq!(
            parse(r#"from:alice subject:"q3 plan" -is:read has:attachment"#).unwrap(),
            And {
                all: vec![
                    From { value: "alice".into() },
                    Subject { value: "q3 plan".into() },
                    Not { expr: Box::new(Is { flag: Flag::Read }) },
                    HasAttachment,
                ]
            }
        );
        assert_eq!(parse(r#""exact words""#).unwrap(), Phrase { value: "exact words".into() });
        assert_eq!(parse(r#"label:"Big Customers""#).unwrap(), Label { value: "Big Customers".into() });
    }

    #[test]
    fn or_binds_looser_than_and_and_parens_group() {
        assert_eq!(parse("a b OR c").unwrap(), Or { any: vec![And { all: vec![text("a"), text("b")] }, text("c")] });
        assert_eq!(parse("a (b OR c)").unwrap(), And { all: vec![text("a"), Or { any: vec![text("b"), text("c")] }] });
    }

    #[test]
    fn dates_ages_and_sizes() {
        assert_eq!(parse("after:2026-01-01").unwrap(), After { at: 1_767_225_600_000 });
        assert_eq!(parse("before:2026/02/01").unwrap(), Before { at: 1_769_904_000_000 });
        assert_eq!(parse("newer_than:7d").unwrap(), NewerThan { days: 7 });
        assert_eq!(parse("older_than:1y").unwrap(), OlderThan { days: 365 });
        assert_eq!(parse("larger:5M").unwrap(), Larger { bytes: 5 * 1024 * 1024 });
        assert_eq!(parse("smaller:200k").unwrap(), Smaller { bytes: 200 * 1024 });
    }

    #[test]
    fn mailboxes_and_flags() {
        assert_eq!(parse("in:inbox").unwrap(), In { mailbox: Mailbox::Inbox });
        assert_eq!(parse("IN:Trash").unwrap(), In { mailbox: Mailbox::Trash });
        assert!(parse("in:trash").unwrap().mentions_spam_or_trash());
        assert!(!parse("in:inbox is:unread").unwrap().mentions_spam_or_trash());
    }

    #[test]
    fn unknown_prefixes_are_plain_words() {
        // "re:" is not a field; it is searched as text.
        assert_eq!(parse("re:meeting").unwrap(), text("re:meeting"));
        assert_eq!(parse("e-mail").unwrap(), text("e-mail"));
    }

    #[test]
    fn errors_point_at_the_problem() {
        assert_eq!(parse("from:").unwrap_err().position, 0);
        assert!(parse(r#"subject:"unclosed"#).unwrap_err().message.contains("unclosed"));
        assert!(parse("(a b").unwrap_err().message.contains("missing )"));
        assert!(parse("in:nowhere").unwrap_err().message.contains("unknown mailbox"));
        assert!(parse("after:yesterday").is_err());
        assert!(parse("a OR").is_err());
    }

    #[test]
    fn the_tree_round_trips_as_json_for_agents() {
        let q = parse(r#"from:alice (is:unread OR is:starred) -label:newsletters"#).unwrap();
        let json = serde_json::to_string(&q).unwrap();
        assert!(json.contains(r#""op":"and""#), "{json}");
        assert_eq!(serde_json::from_str::<SearchExpr>(&json).unwrap(), q);
    }
}
