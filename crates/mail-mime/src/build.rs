//! Outgoing mail (spec §7.5): RFC 5322 bytes from a composed message.
//!
//! Structure: `multipart/alternative` (text/plain + text/html), wrapped in
//! `multipart/mixed` when there are attachments. Replies carry
//! `In-Reply-To` and `References` so Gmail (and every other client) threads
//! them; Gmail also needs the thread id and a matching subject, which the
//! caller supplies to the provider.

use mail_builder::MessageBuilder;
use mail_builder::headers::address::Address;
use mail_domain::{EmailAddress, Millis};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OutgoingMessage {
    pub from: EmailAddress,
    pub to: Vec<EmailAddress>,
    pub cc: Vec<EmailAddress>,
    pub bcc: Vec<EmailAddress>,
    pub subject: String,
    pub html: String,
    /// Plain-text alternative; derived from `html` when `None`.
    pub text: Option<String>,
    /// Without angle brackets.
    pub message_id: String,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub attachments: Vec<OutgoingAttachment>,
    pub date: Millis,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutgoingAttachment {
    pub filename: String,
    pub mime_type: String,
    pub data: Vec<u8>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BuildError {
    #[error("a message needs at least one recipient")]
    NoRecipients,
    #[error("invalid address {0:?}")]
    InvalidAddress(String),
    #[error("could not serialize the message: {0}")]
    Serialize(String),
}

fn address(a: &EmailAddress) -> Result<Address<'static>, BuildError> {
    let email = a.email.trim();
    // One @, something on each side, no whitespace or header-breaking chars.
    let valid = email.matches('@').count() == 1
        && !email.starts_with('@')
        && !email.ends_with('@')
        && !email.chars().any(|c| c.is_whitespace() || matches!(c, '<' | '>' | ',' | ';' | '"'));
    if !valid {
        return Err(BuildError::InvalidAddress(a.email.clone()));
    }
    Ok(match &a.name {
        Some(name) => Address::new_address(Some(name.clone()), email.to_owned()),
        None => Address::new_address(None::<String>, email.to_owned()),
    })
}

fn list(addrs: &[EmailAddress]) -> Result<Address<'static>, BuildError> {
    Ok(Address::new_list(addrs.iter().map(address).collect::<Result<_, _>>()?))
}

pub fn build(m: &OutgoingMessage) -> Result<Vec<u8>, BuildError> {
    if m.to.is_empty() && m.cc.is_empty() && m.bcc.is_empty() {
        return Err(BuildError::NoRecipients);
    }
    build_draft(m)
}

/// Like [`build`], but a draft may have no recipients yet.
pub fn build_draft(m: &OutgoingMessage) -> Result<Vec<u8>, BuildError> {
    let text = m.text.clone().unwrap_or_else(|| crate::html_to_text(&m.html));
    let mut b = MessageBuilder::new()
        .from(address(&m.from)?)
        .subject(m.subject.clone())
        .message_id(m.message_id.clone())
        .date(m.date / 1000)
        .text_body(text)
        .html_body(m.html.clone());
    if !m.to.is_empty() {
        b = b.to(list(&m.to)?);
    }
    if !m.cc.is_empty() {
        b = b.cc(list(&m.cc)?);
    }
    if !m.bcc.is_empty() {
        b = b.bcc(list(&m.bcc)?);
    }
    if let Some(parent) = &m.in_reply_to {
        b = b.in_reply_to(parent.clone());
    }
    if !m.references.is_empty() {
        b = b.references(m.references.clone());
    }
    for a in &m.attachments {
        b = b.attachment(a.mime_type.clone(), a.filename.clone(), a.data.clone());
    }
    b.write_to_vec().map_err(|e| BuildError::Serialize(e.to_string()))
}

/// "Re: Subject", without stacking prefixes.
pub fn reply_subject(subject: &str) -> String {
    let s = subject.trim();
    if strip_prefix_ci(s, "re:").is_some() { s.to_owned() } else { format!("Re: {s}") }
}

/// "Fwd: Subject", without stacking prefixes.
pub fn forward_subject(subject: &str) -> String {
    let s = subject.trim();
    if strip_prefix_ci(s, "fwd:").is_some() || strip_prefix_ci(s, "fw:").is_some() {
        s.to_owned()
    } else {
        format!("Fwd: {s}")
    }
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    (s.len() >= prefix.len() && s[..prefix.len()].eq_ignore_ascii_case(prefix)).then(|| &s[prefix.len()..])
}

/// The References chain for a reply: the parent's references plus the
/// parent itself, capped the way long threads need (first + last 19).
pub fn reply_references(parent_references: &[String], parent_message_id: Option<&str>) -> Vec<String> {
    let mut refs: Vec<String> = parent_references.to_vec();
    if let Some(id) = parent_message_id
        && !refs.iter().any(|r| r == id)
    {
        refs.push(id.to_owned());
    }
    if refs.len() > 20 {
        let first = refs[0].clone();
        let tail = refs.split_off(refs.len() - 19);
        refs = std::iter::once(first).chain(tail).collect();
    }
    refs
}

/// Recipients for a reply: Reply-To if set, else the sender; for reply-all,
/// also everyone on To and Cc. The user's own addresses are removed.
pub fn reply_recipients(
    from: Option<&EmailAddress>,
    reply_to: &[EmailAddress],
    to: &[EmailAddress],
    cc: &[EmailAddress],
    me: &[String],
    all: bool,
) -> (Vec<EmailAddress>, Vec<EmailAddress>) {
    let mine = |a: &EmailAddress| me.iter().any(|m| m.eq_ignore_ascii_case(&a.email));
    let mut seen = std::collections::HashSet::new();
    let mut keep = |a: &EmailAddress| !mine(a) && seen.insert(a.normalized());
    let primary: Vec<EmailAddress> =
        if reply_to.is_empty() { from.into_iter().cloned().collect() } else { reply_to.to_vec() };
    // Replying to your own message: go back to its recipients.
    let primary = if !primary.is_empty() && primary.iter().all(mine) { to.to_vec() } else { primary };
    let reply_to_list: Vec<EmailAddress> = primary.into_iter().filter(|a| keep(a)).collect();
    let cc_list: Vec<EmailAddress> =
        if all { to.iter().chain(cc).filter(|a| keep(a)).cloned().collect() } else { Vec::new() };
    (reply_to_list, cc_list)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(name: Option<&str>, email: &str) -> EmailAddress {
        EmailAddress::new(name, email)
    }

    fn sample() -> OutgoingMessage {
        OutgoingMessage {
            from: a(Some("Me"), "me@example.com"),
            to: vec![a(Some("Alex Rivera"), "alex@example.com")],
            cc: vec![a(None, "sam@example.org")],
            bcc: vec![a(None, "hidden@example.net")],
            subject: "Re: Réunion – café".into(),
            html: "<p>Thursday <b>works</b>.</p>".into(),
            text: None,
            message_id: "reply.42@example.com".into(),
            in_reply_to: Some("plan.1@example.com".into()),
            references: vec!["root@example.com".into(), "plan.1@example.com".into()],
            attachments: vec![OutgoingAttachment {
                filename: "notes.pdf".into(),
                mime_type: "application/pdf".into(),
                data: b"%PDF-1.4 x".to_vec(),
            }],
            date: 1_789_489_800_000,
        }
    }

    #[test]
    fn a_built_message_parses_back_identically() {
        let raw = build(&sample()).unwrap();
        let parsed = crate::parse(&raw).unwrap();
        let h = &parsed.headers;
        assert_eq!(h.from, Some(a(Some("Me"), "me@example.com")));
        assert_eq!(h.to, vec![a(Some("Alex Rivera"), "alex@example.com")]);
        assert_eq!(h.cc, vec![a(None, "sam@example.org")]);
        assert_eq!(h.bcc, vec![a(None, "hidden@example.net")], "Gmail needs Bcc in the raw to deliver it");
        assert_eq!(h.subject, "Re: Réunion – café", "non-ASCII subject survives encoding");
        assert_eq!(h.message_id.as_deref(), Some("reply.42@example.com"));
        assert_eq!(h.in_reply_to.as_deref(), Some("plan.1@example.com"));
        assert_eq!(h.references, vec!["root@example.com", "plan.1@example.com"]);
        assert_eq!(h.date, Some(1_789_489_800_000));
        assert!(parsed.html.unwrap().contains("<b>works</b>"));
        assert_eq!(parsed.text.unwrap().trim(), "Thursday works.", "text alternative derived from HTML");
        assert_eq!(parsed.attachments.len(), 1);
        assert_eq!(parsed.attachments[0].filename, "notes.pdf");
        assert_eq!(parsed.attachments[0].data, b"%PDF-1.4 x");
    }

    #[test]
    fn bad_input_is_rejected_before_anything_is_sent() {
        let mut m = sample();
        m.to = vec![a(None, "not an address")];
        assert_eq!(build(&m).unwrap_err(), BuildError::InvalidAddress("not an address".into()));
        m.to = vec![a(None, "evil@example.com>\r\nBcc: attacker@example.net")];
        assert!(matches!(build(&m).unwrap_err(), BuildError::InvalidAddress(_)));
        let mut m = sample();
        m.to.clear();
        m.cc.clear();
        m.bcc.clear();
        assert_eq!(build(&m).unwrap_err(), BuildError::NoRecipients);
        let draft = crate::parse(&build_draft(&m).unwrap()).unwrap();
        assert!(draft.headers.to.is_empty(), "a draft may have no recipients yet");
    }

    #[test]
    fn subjects_do_not_stack_prefixes() {
        assert_eq!(reply_subject("Q3 planning"), "Re: Q3 planning");
        assert_eq!(reply_subject("RE: Q3 planning"), "RE: Q3 planning");
        assert_eq!(forward_subject("Fw: deck"), "Fw: deck");
        assert_eq!(forward_subject("deck"), "Fwd: deck");
    }

    #[test]
    fn references_chain_and_cap() {
        assert_eq!(reply_references(&["a".into()], Some("b")), vec!["a", "b"]);
        assert_eq!(reply_references(&["a".into(), "b".into()], Some("b")), vec!["a", "b"]);
        let long: Vec<String> = (0..30).map(|i| format!("m{i}")).collect();
        let capped = reply_references(&long, Some("m30"));
        assert_eq!(capped.len(), 20);
        assert_eq!(capped[0], "m0");
        assert_eq!(capped.last().unwrap(), "m30");
    }

    #[test]
    fn reply_and_reply_all_recipients() {
        let me = vec!["me@example.com".to_owned()];
        let from = a(Some("Alex"), "alex@example.com");
        let to = vec![a(None, "me@example.com"), a(None, "sam@example.org")];
        let cc = vec![a(None, "jo@example.net"), a(None, "ALEX@example.com")];
        let (r, c) = reply_recipients(Some(&from), &[], &to, &cc, &me, false);
        assert_eq!((r, c.len()), (vec![from.clone()], 0));
        let (r, c) = reply_recipients(Some(&from), &[], &to, &cc, &me, true);
        assert_eq!(r, vec![from.clone()]);
        assert_eq!(c, vec![a(None, "sam@example.org"), a(None, "jo@example.net")], "me and duplicates removed");
        let list = vec![a(Some("List"), "list@example.org")];
        let (r, _) = reply_recipients(Some(&from), &list, &to, &cc, &me, false);
        assert_eq!(r, list, "Reply-To wins");
        let mine = a(None, "me@example.com");
        let (r, _) = reply_recipients(Some(&mine), &[], &[a(None, "alex@example.com")], &[], &me, false);
        assert_eq!(r, vec![a(None, "alex@example.com")], "replying to my own sent message goes to its recipients");
    }
}
