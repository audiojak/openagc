//! Bulk backfill over Gmail IMAP (spec §7.4, IMAP amendment). Only message
//! bodies for backfill come through here; listing, history, writes and new
//! mail stay on the REST API, whose ids are the source of truth.
//!
//! How it maps onto the API: Gmail's `X-GM-MSGID` and `X-GM-THRID` are the
//! API's message and thread ids (decimal here, hex there). One cheap
//! `UID FETCH 1:* (UID X-GM-MSGID RFC822.SIZE)` over
//! `[Gmail]/All Mail` maps ids to UIDs; bodies then come in batched
//! `UID FETCH … BODY.PEEK[]`. Anything IMAP cannot or should not serve goes
//! to REST: ids not in All Mail (spam, trash), messages over the size cap
//! (REST never downloads attachment bytes), and everything once the daily
//! byte budget is spent or if Google refuses the IMAP login.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::task::{Context, Poll};

use async_imap::imap_proto::{AttributeValue, Response, Status};
use async_trait::async_trait;
use mail_domain::{LabelId, MessageId, Millis, ThreadId, system_labels};
use provider_api::{
    BackfillSource, FetchedAttachment, FetchedBody, FetchedMessage, MailProvider, Priority, ProviderError,
    ProviderResult, TokenSource,
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio::sync::Mutex;

pub const GMAIL_IMAP_HOST: &str = "imap.gmail.com";
const ALL_MAIL: &str = "[Gmail]/All Mail";
const SNIPPET_CHARS: usize = 160;

/// Where to connect: Gmail over TLS, or a plain local socket (tests).
#[derive(Debug, Clone)]
pub enum ImapEndpoint {
    Tls { host: String, port: u16 },
    Plain(SocketAddr),
}

#[derive(Debug, Clone)]
pub struct ImapConfig {
    /// The account's address, for XOAUTH2.
    pub email: String,
    pub endpoint: ImapEndpoint,
    /// Messages larger than this go through REST.
    pub max_message_bytes: u64,
    /// Bytes per day over IMAP before yielding to REST (Gmail allows about
    /// 2,500 MB; this keeps headroom for the user's other clients).
    pub daily_budget_bytes: u64,
    /// UIDs per body fetch.
    pub batch: usize,
}

impl ImapConfig {
    pub fn gmail(email: &str) -> Self {
        Self {
            email: email.to_owned(),
            endpoint: ImapEndpoint::Tls { host: GMAIL_IMAP_HOST.into(), port: 993 },
            max_message_bytes: 2 * 1024 * 1024,
            daily_budget_bytes: 2_000 * 1024 * 1024,
            batch: 200,
        }
    }
}

/// Maps Gmail label names (as IMAP shows them) to API label ids; the core
/// keeps it in step with the stored label list.
pub type LabelNames = Arc<RwLock<HashMap<String, LabelId>>>;

#[derive(Clone, Copy)]
struct Located {
    uid: u32,
    size: u32,
}

#[derive(Default)]
struct State {
    session: Option<Session>,
    /// X-GM-MSGID → where it is in All Mail.
    map: HashMap<u64, Located>,
}

/// Longest wait for a connection, a login, or any single response line.
/// A half-open connection after sleep must not stall backfill forever.
const IO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

type Session = async_imap::Session<ImapStream>;

pub struct ImapBackfill {
    config: ImapConfig,
    tokens: Arc<dyn TokenSource>,
    rest: Arc<dyn MailProvider>,
    labels: LabelNames,
    state: Mutex<State>,
    /// Google refused the login (IMAP disabled, or the token lacks the
    /// scope): REST only from here on.
    refused: AtomicBool,
    /// Day number (UTC) and bytes fetched on it; atomics so status never
    /// waits on a fetch in progress.
    day: std::sync::atomic::AtomicI64,
    bytes_today: std::sync::atomic::AtomicU64,
}

impl ImapBackfill {
    pub fn new(
        config: ImapConfig,
        tokens: Arc<dyn TokenSource>,
        rest: Arc<dyn MailProvider>,
        labels: LabelNames,
    ) -> Self {
        Self {
            config,
            tokens,
            rest,
            labels,
            state: Mutex::new(State::default()),
            refused: AtomicBool::new(false),
            day: Default::default(),
            bytes_today: Default::default(),
        }
    }

    /// Whether IMAP was refused and everything now goes through REST.
    pub fn is_refused(&self) -> bool {
        self.refused.load(Ordering::Relaxed)
    }

    /// Bytes fetched over IMAP today (diagnostics).
    pub async fn bytes_today(&self) -> u64 {
        if self.day.load(Ordering::Relaxed) != mail_domain_today() {
            return 0;
        }
        self.bytes_today.load(Ordering::Relaxed)
    }

    fn count_bytes(&self, bytes: u64) {
        let today = mail_domain_today();
        if self.day.swap(today, Ordering::Relaxed) != today {
            self.bytes_today.store(0, Ordering::Relaxed);
        }
        self.bytes_today.fetch_add(bytes, Ordering::Relaxed);
    }

    async fn connect(&self) -> ProviderResult<Session> {
        match tokio::time::timeout(IO_TIMEOUT, self.connect_inner()).await {
            Ok(result) => result,
            Err(_) => Err(ProviderError::Network("IMAP connection timed out".into())),
        }
    }

    async fn connect_inner(&self) -> ProviderResult<Session> {
        let stream = match &self.config.endpoint {
            ImapEndpoint::Plain(addr) => ImapStream::Plain(TcpStream::connect(addr).await.map_err(net)?),
            ImapEndpoint::Tls { host, port } => {
                let tcp = TcpStream::connect((host.as_str(), *port)).await.map_err(net)?;
                let roots = tokio_rustls::rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
                let tls =
                    tokio_rustls::rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth();
                let name = tokio_rustls::rustls::pki_types::ServerName::try_from(host.clone())
                    .map_err(|e| ProviderError::Invalid(e.to_string()))?;
                let stream = tokio_rustls::TlsConnector::from(Arc::new(tls)).connect(name, tcp).await.map_err(net)?;
                ImapStream::Tls(Box::new(stream))
            }
        };
        let mut client = async_imap::Client::new(stream);
        client.read_response().await.map_err(net)?.ok_or_else(|| ProviderError::Network("no IMAP greeting".into()))?;
        let token = self.tokens.access_token().await?;
        let auth = XOAuth2(format!("user={}\u{1}auth=Bearer {}\u{1}\u{1}", self.config.email, token.expose()));
        let mut session = match client.authenticate("XOAUTH2", auth).await {
            Ok(session) => session,
            Err((e, _)) => {
                self.refused.store(true, Ordering::Relaxed);
                tracing::warn!(error = %e, "Gmail refused the IMAP login; backfill continues over the API");
                return Err(ProviderError::Forbidden("IMAP login refused".into()));
            }
        };
        session.examine(ALL_MAIL).await.map_err(imap)?;
        Ok(session)
    }

    /// Refresh the id → UID map for All Mail.
    async fn load_map(&self, session: &mut Session) -> ProviderResult<HashMap<u64, Located>> {
        let mut map = HashMap::new();
        for_each_fetch(session, "UID FETCH 1:* (UID X-GM-MSGID RFC822.SIZE)", |attrs| {
            let (mut uid, mut msgid, mut size) = (None, None, 0);
            for a in attrs {
                match a {
                    AttributeValue::Uid(u) => uid = Some(*u),
                    AttributeValue::GmailMsgId(m) => msgid = Some(*m),
                    AttributeValue::Rfc822Size(s) => size = *s,
                    _ => {}
                }
            }
            if let (Some(uid), Some(msgid)) = (uid, msgid) {
                map.insert(msgid, Located { uid, size });
            }
        })
        .await?;
        tracing::debug!(messages = map.len(), "IMAP id map loaded");
        Ok(map)
    }

    async fn fetch_imap(&self, wanted: &[(u64, Located)]) -> ProviderResult<Vec<FetchedMessage>> {
        let mut state = self.state.lock().await;
        let mut session = match state.session.take() {
            Some(s) => s,
            None => self.connect().await?,
        };
        let labels = self.labels.read().unwrap_or_else(|e| e.into_inner()).clone();
        let mut out = Vec::with_capacity(wanted.len());
        let mut bytes = 0u64;
        let result: ProviderResult<()> = async {
            for chunk in wanted.chunks(self.config.batch.max(1)) {
                let set = chunk.iter().map(|(_, l)| l.uid.to_string()).collect::<Vec<_>>().join(",");
                let command =
                    format!("UID FETCH {set} (UID X-GM-MSGID X-GM-THRID X-GM-LABELS FLAGS INTERNALDATE BODY.PEEK[])");
                for_each_fetch(&mut session, &command, |attrs| {
                    if let Some(m) = to_fetched(attrs, &labels) {
                        bytes += m.size_estimate;
                        out.push(m);
                    }
                })
                .await?;
            }
            Ok(())
        }
        .await;
        match result {
            Ok(()) => {
                state.session = Some(session);
                self.count_bytes(bytes);
                Ok(out)
            }
            // A broken session is dropped; the next call reconnects.
            Err(e) => Err(e),
        }
    }
}

#[async_trait]
impl BackfillSource for ImapBackfill {
    async fn fetch(&self, ids: &[MessageId]) -> ProviderResult<Vec<FetchedMessage>> {
        if self.is_refused() || self.over_budget().await {
            return self.rest.fetch_messages(ids, Priority::Background).await;
        }
        // Ids the map does not know yet: reload it once (new mail since).
        let parsed: Vec<(MessageId, Option<u64>)> =
            ids.iter().map(|id| (id.clone(), u64::from_str_radix(id.as_str(), 16).ok())).collect();
        let needs_map = {
            let state = self.state.lock().await;
            parsed.iter().any(|(_, m)| m.is_some_and(|m| !state.map.contains_key(&m)))
        };
        if needs_map {
            let mut state = self.state.lock().await;
            let mut session = match state.session.take() {
                Some(s) => s,
                None => match self.connect().await {
                    Ok(s) => s,
                    Err(_) => {
                        drop(state);
                        return self.rest.fetch_messages(ids, Priority::Background).await;
                    }
                },
            };
            match self.load_map(&mut session).await {
                Ok(map) => {
                    state.map = map;
                    state.session = Some(session);
                }
                Err(e) => {
                    tracing::warn!(error = %e, "IMAP id map failed; this batch goes over the API");
                    drop(state);
                    return self.rest.fetch_messages(ids, Priority::Background).await;
                }
            }
        }
        let (via_imap, via_rest) = {
            let state = self.state.lock().await;
            let mut imap = Vec::new();
            let mut rest = Vec::new();
            for (id, msgid) in &parsed {
                match msgid.and_then(|m| state.map.get(&m).map(|l| (m, *l))) {
                    Some((m, l)) if u64::from(l.size) <= self.config.max_message_bytes => imap.push((m, l)),
                    _ => rest.push(id.clone()),
                }
            }
            (imap, rest)
        };
        let mut out = match self.fetch_imap(&via_imap).await {
            Ok(messages) => messages,
            Err(e) => {
                tracing::warn!(error = %e, "IMAP fetch failed; this batch goes over the API");
                return self.rest.fetch_messages(ids, Priority::Background).await;
            }
        };
        // Anything IMAP did not return (moved to Spam or Trash since the
        // map loaded, or unparseable) goes over the API too; backfill
        // drops every requested id from its queue, so none may be skipped.
        let returned: std::collections::HashSet<&str> = out.iter().map(|m| m.id.as_str()).collect();
        let mut via_rest = via_rest;
        let mut stale = Vec::new();
        for (msgid, _) in &via_imap {
            let id = MessageId(format!("{msgid:x}"));
            if !returned.contains(id.as_str()) {
                stale.push(*msgid);
                via_rest.push(id);
            }
        }
        if !stale.is_empty() {
            let mut state = self.state.lock().await;
            for msgid in stale {
                state.map.remove(&msgid);
            }
        }
        if !via_rest.is_empty() {
            out.extend(self.rest.fetch_messages(&via_rest, Priority::Background).await?);
        }
        Ok(out)
    }

    async fn fetch_headers(&self, ids: &[MessageId]) -> ProviderResult<Option<Vec<FetchedMessage>>> {
        if self.is_refused() || self.over_budget().await {
            return Ok(None);
        }
        let wanted: Vec<u64> = ids.iter().filter_map(|id| u64::from_str_radix(id.as_str(), 16).ok()).collect();
        let mut state = self.state.lock().await;
        let mut session = match state.session.take() {
            Some(s) => s,
            None => match self.connect().await {
                Ok(s) => s,
                Err(_) => return Ok(None),
            },
        };
        if wanted.iter().any(|m| !state.map.contains_key(m)) {
            match self.load_map(&mut session).await {
                Ok(map) => state.map = map,
                Err(_) => return Ok(None),
            }
        }
        let uids: Vec<u32> = wanted.iter().filter_map(|m| state.map.get(m).map(|l| l.uid)).collect();
        let labels = self.labels.read().unwrap_or_else(|e| e.into_inner()).clone();
        let mut out = Vec::with_capacity(uids.len());
        let mut failed = false;
        for chunk in uids.chunks(1000) {
            let set = chunk.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
            let command =
                format!("UID FETCH {set} (UID X-GM-MSGID X-GM-THRID X-GM-LABELS FLAGS INTERNALDATE BODY.PEEK[HEADER])");
            let result = for_each_fetch(&mut session, &command, |attrs| {
                if let Some(mut m) = to_fetched(attrs, &labels) {
                    // Headers only: no body yet, so sync keeps it queued.
                    m.body = None;
                    m.snippet.clear();
                    out.push(m);
                }
            })
            .await;
            if result.is_err() {
                failed = true;
                break;
            }
        }
        if failed {
            return Ok(None);
        }
        state.session = Some(session);
        Ok(Some(out))
    }

    fn name(&self) -> &'static str {
        if self.is_refused() { "imap-refused" } else { "imap" }
    }
}

impl ImapBackfill {
    async fn over_budget(&self) -> bool {
        self.bytes_today().await >= self.config.daily_budget_bytes
    }
}

/// Run a FETCH command and call `each` with every FETCH response's
/// attributes, until the tagged completion.
async fn for_each_fetch(
    session: &mut Session,
    command: &str,
    mut each: impl FnMut(&[AttributeValue<'_>]),
) -> ProviderResult<()> {
    let tag = match tokio::time::timeout(IO_TIMEOUT, session.run_command(command)).await {
        Ok(result) => result.map_err(imap)?,
        Err(_) => return Err(ProviderError::Network("IMAP command timed out".into())),
    };
    loop {
        let read = match tokio::time::timeout(IO_TIMEOUT, session.read_response()).await {
            Ok(read) => read.map_err(net)?,
            Err(_) => return Err(ProviderError::Network("IMAP response timed out".into())),
        };
        let Some(response) = read else {
            return Err(ProviderError::Network("IMAP connection closed".into()));
        };
        match response.parsed() {
            Response::Fetch(_, attrs) => each(attrs),
            Response::Done { tag: done, status, information, .. } if *done == tag => {
                return match status {
                    Status::Ok => Ok(()),
                    _ => Err(ProviderError::Invalid(format!(
                        "IMAP {status:?}: {}",
                        information.as_deref().unwrap_or("")
                    ))),
                };
            }
            _ => {}
        }
    }
}

/// One FETCH response → a provider message, parsed exactly like the other
/// paths (headers, text, HTML, attachments with their bytes).
fn to_fetched(attrs: &[AttributeValue<'_>], labels: &HashMap<String, LabelId>) -> Option<FetchedMessage> {
    let (mut msgid, mut thrid, mut raw, mut internal) = (None, None, None, None);
    let mut label_ids: Vec<LabelId> = Vec::new();
    let (mut seen, mut flagged) = (false, false);
    for a in attrs {
        match a {
            AttributeValue::GmailMsgId(m) => msgid = Some(*m),
            AttributeValue::GmailThrId(t) => thrid = Some(*t),
            AttributeValue::BodySection { data: Some(d), .. } | AttributeValue::Rfc822(Some(d)) => {
                raw = Some(d.to_vec());
            }
            AttributeValue::InternalDate(d) => internal = parse_internal_date(d),
            AttributeValue::Flags(flags) => {
                seen = flags.iter().any(|f| f.eq_ignore_ascii_case("\\Seen"));
                flagged = flags.iter().any(|f| f.eq_ignore_ascii_case("\\Flagged"));
            }
            AttributeValue::GmailLabels(names) => {
                for name in names {
                    match system_label(name) {
                        Some(id) => label_ids.push(LabelId::new(id)),
                        None => match labels.get(name.as_ref()) {
                            Some(id) => label_ids.push(id.clone()),
                            None => tracing::debug!("IMAP label not in the label list yet; skipped"),
                        },
                    }
                }
            }
            _ => {}
        }
    }
    let (msgid, thrid, raw) = (msgid?, thrid?, raw?);
    if !seen {
        label_ids.push(LabelId::new(system_labels::UNREAD));
    }
    if flagged {
        label_ids.push(LabelId::new(system_labels::STARRED));
    }
    label_ids.sort();
    label_ids.dedup();
    let parsed = mail_mime::parse(&raw).ok()?;
    let h = parsed.headers;
    let text = parsed.text.clone();
    let snippet = snippet(text.as_deref().or(parsed.html.as_deref()).unwrap_or(""));
    Some(FetchedMessage {
        id: MessageId(format!("{msgid:x}")),
        thread_id: ThreadId(format!("{thrid:x}")),
        label_ids,
        snippet,
        internal_date: internal.or(h.date).unwrap_or(0),
        size_estimate: raw.len() as u64,
        message_id_header: h.message_id,
        in_reply_to: h.in_reply_to,
        references: h.references,
        from: h.from,
        to: h.to,
        cc: h.cc,
        bcc: h.bcc,
        reply_to: h.reply_to,
        subject: h.subject,
        date: h.date,
        body: Some(FetchedBody {
            text: parsed.text,
            html: parsed.html,
            attachments: parsed
                .attachments
                .into_iter()
                .enumerate()
                .map(|(i, a)| FetchedAttachment {
                    part_id: Some((i + 1).to_string()),
                    attachment_id: None,
                    filename: a.filename,
                    mime_type: a.mime_type,
                    size: a.size,
                    content_id: a.content_id,
                    is_inline: a.is_inline,
                    // Here with the message: nothing to fetch later.
                    data: Some(a.data),
                })
                .collect(),
        }),
    })
}

/// IMAP's names for Gmail's system labels.
fn system_label(name: &str) -> Option<&'static str> {
    Some(match name {
        "\\Inbox" => system_labels::INBOX,
        "\\Sent" => system_labels::SENT,
        "\\Important" => system_labels::IMPORTANT,
        "\\Starred" => system_labels::STARRED,
        "\\Draft" => system_labels::DRAFT,
        "\\Spam" => system_labels::SPAM,
        "\\Trash" => system_labels::TRASH,
        _ => return None,
    })
}

fn snippet(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(SNIPPET_CHARS).collect()
}

/// `17-Jul-1996 02:44:25 -0700` → epoch millis.
fn parse_internal_date(s: &str) -> Option<Millis> {
    let s = s.trim().trim_matches('"');
    let (date, rest) = s.split_once(' ')?;
    let (time, zone) = rest.trim().split_once(' ')?;
    let mut d = date.trim().split('-');
    let day: i64 = d.next()?.trim().parse().ok()?;
    let month_name = d.next()?;
    let month = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"]
        .iter()
        .position(|m| m.eq_ignore_ascii_case(month_name))? as i64
        + 1;
    let year: i64 = d.next()?.parse().ok()?;
    let mut t = time.split(':').map(|x| x.parse::<i64>().ok());
    let (hh, mm, ss) = (t.next()??, t.next()??, t.next()??);
    let sign = if zone.starts_with('-') { -1 } else { 1 };
    let z: i64 = zone[1..].parse().ok()?;
    let offset = sign * ((z / 100) * 3600 + (z % 100) * 60);
    // Days from civil (Howard Hinnant's algorithm).
    let (y, m) = if month <= 2 { (year - 1, month + 9) } else { (year, month - 3) };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * m + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some((days * 86_400 + hh * 3600 + mm * 60 + ss - offset) * 1000)
}

fn mail_domain_today() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64 / 86_400).unwrap_or(0)
}

fn net(e: impl std::fmt::Display) -> ProviderError {
    ProviderError::Network(e.to_string())
}

fn imap(e: async_imap::error::Error) -> ProviderError {
    ProviderError::Network(format!("IMAP: {e}"))
}

struct XOAuth2(String);

impl async_imap::Authenticator for XOAuth2 {
    type Response = String;
    fn process(&mut self, _challenge: &[u8]) -> String {
        // Gmail sends an error as a second challenge; an empty answer lets
        // it finish with NO.
        std::mem::take(&mut self.0)
    }
}

/// TLS to Gmail, or plain TCP to the local fake.
#[derive(Debug)]
pub enum ImapStream {
    Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
    Plain(TcpStream),
}

impl AsyncRead for ImapStream {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Tls(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
            Self::Plain(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for ImapStream {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Self::Tls(s) => Pin::new(s.as_mut()).poll_write(cx, buf),
            Self::Plain(s) => Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Tls(s) => Pin::new(s.as_mut()).poll_flush(cx),
            Self::Plain(s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Tls(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
            Self::Plain(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn internal_dates_parse_to_epoch_millis() {
        assert_eq!(parse_internal_date("17-Jul-1996 02:44:25 -0700"), Some(837_596_665_000));
        assert_eq!(parse_internal_date("\"01-Jan-1970 00:00:00 +0000\""), Some(0));
        assert_eq!(parse_internal_date(" 1-Mar-2024 12:00:00 +0100"), Some(1_709_290_800_000));
        assert_eq!(parse_internal_date("nonsense"), None);
    }

    #[test]
    fn system_labels_use_the_api_names() {
        assert_eq!(system_label("\\Inbox"), Some("INBOX"));
        assert_eq!(system_label("\\Sent"), Some("SENT"));
        assert_eq!(system_label("Clients/Acme"), None);
    }
}
