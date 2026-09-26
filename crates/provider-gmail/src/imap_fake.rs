//! An in-process fake of Gmail's IMAP endpoint for tests (spec §7.4 IMAP
//! amendment, Testing). Plain TCP on 127.0.0.1; speaks only what the
//! backfill client uses: CAPABILITY, AUTHENTICATE XOAUTH2, SELECT/EXAMINE,
//! UID FETCH (UID, FLAGS, X-GM-MSGID, X-GM-THRID, X-GM-LABELS,
//! RFC822.SIZE, BODY.PEEK[]), NOOP and LOGOUT. Nothing here ever connects
//! anywhere.

use std::sync::{Arc, Mutex};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

/// One message in the fake All Mail.
#[derive(Debug, Clone)]
pub struct FakeImapMessage {
    pub uid: u32,
    /// Gmail's message id (decimal here; the API writes it in hex).
    pub msgid: u64,
    pub thrid: u64,
    /// `X-GM-LABELS`: system labels as `\Inbox`, `\Sent`…; user labels by name.
    pub labels: Vec<String>,
    /// `\Seen`, `\Flagged`.
    pub flags: Vec<String>,
    pub raw: Vec<u8>,
}

#[derive(Default)]
struct State {
    messages: Vec<FakeImapMessage>,
    /// Bearer token the server accepts.
    token: String,
    /// Counters for assertions.
    logins: usize,
    body_fetches: usize,
    header_fetches: usize,
    /// Refuse every AUTHENTICATE (an admin disabled IMAP).
    refuse_login: bool,
}

/// A running fake server. Dropping it stops accepting connections.
pub struct FakeImapServer {
    pub addr: std::net::SocketAddr,
    state: Arc<Mutex<State>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for FakeImapServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FakeImapServer {
    /// Start on an ephemeral port, accepting `token` as the OAuth bearer.
    pub async fn start(token: &str) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind the fake IMAP server");
        let addr = listener.local_addr().expect("fake IMAP address");
        let state = Arc::new(Mutex::new(State { token: token.to_owned(), ..Default::default() }));
        let accept_state = state.clone();
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let state = accept_state.clone();
                tokio::spawn(async move {
                    let _ = serve(stream, state).await;
                });
            }
        });
        Self { addr, state, task }
    }

    pub fn add(&self, message: FakeImapMessage) {
        self.state.lock().unwrap().messages.push(message);
    }

    pub fn refuse_logins(&self) {
        self.state.lock().unwrap().refuse_login = true;
    }

    pub fn logins(&self) -> usize {
        self.state.lock().unwrap().logins
    }

    /// Messages whose body was fetched.
    pub fn body_fetches(&self) -> usize {
        self.state.lock().unwrap().body_fetches
    }

    /// Messages whose headers alone were fetched.
    pub fn header_fetches(&self) -> usize {
        self.state.lock().unwrap().header_fetches
    }
}

async fn serve(stream: TcpStream, state: Arc<Mutex<State>>) -> std::io::Result<()> {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read);
    write.write_all(b"* OK Gimap ready (fake)\r\n").await?;
    let mut authenticated = false;
    let mut line = String::new();
    loop {
        line.clear();
        if lines.read_line(&mut line).await? == 0 {
            return Ok(());
        }
        let trimmed = line.trim_end();
        let (tag, rest) = trimmed.split_once(' ').unwrap_or((trimmed, ""));
        let (command, args) = rest.split_once(' ').unwrap_or((rest, ""));
        match command.to_ascii_uppercase().as_str() {
            "CAPABILITY" => {
                write.write_all(b"* CAPABILITY IMAP4rev1 UIDPLUS X-GM-EXT-1 AUTH=XOAUTH2 AUTH=PLAIN\r\n").await?;
                write.write_all(format!("{tag} OK Thats all she wrote!\r\n").as_bytes()).await?;
            }
            "AUTHENTICATE" => {
                // SASL: an empty challenge, then the client's response.
                let initial = args.split_once(' ').map(|(_, ir)| ir.to_owned());
                let response = match initial {
                    Some(ir) => ir,
                    None => {
                        write.write_all(b"+ \r\n").await?;
                        let mut answer = String::new();
                        lines.read_line(&mut answer).await?;
                        answer.trim_end().to_owned()
                    }
                };
                let decoded = STANDARD.decode(response.as_bytes()).unwrap_or_default();
                let text = String::from_utf8_lossy(&decoded).into_owned();
                let (ok, refused) = {
                    let s = state.lock().unwrap();
                    (
                        text.contains(&format!("auth=Bearer {}\u{1}", s.token)) && text.starts_with("user="),
                        s.refuse_login,
                    )
                };
                if ok && !refused {
                    authenticated = true;
                    state.lock().unwrap().logins += 1;
                    write.write_all(format!("{tag} OK me@example.com authenticated (Success)\r\n").as_bytes()).await?;
                } else {
                    write
                        .write_all(
                            format!("{tag} NO [AUTHENTICATIONFAILED] Invalid credentials (Failure)\r\n").as_bytes(),
                        )
                        .await?;
                }
            }
            "SELECT" | "EXAMINE" if authenticated => {
                let count = state.lock().unwrap().messages.len();
                write.write_all(b"* FLAGS (\\Answered \\Flagged \\Draft \\Deleted \\Seen)\r\n").await?;
                write.write_all(format!("* {count} EXISTS\r\n* 0 RECENT\r\n").as_bytes()).await?;
                write.write_all(b"* OK [UIDVALIDITY 7] UIDs valid.\r\n").await?;
                let mode = if command.eq_ignore_ascii_case("EXAMINE") { "READ-ONLY" } else { "READ-WRITE" };
                write
                    .write_all(format!("{tag} OK [{mode}] [Gmail]/All Mail selected. (Success)\r\n").as_bytes())
                    .await?;
            }
            "UID" if authenticated => {
                let (sub, rest) = args.split_once(' ').unwrap_or((args, ""));
                if !sub.eq_ignore_ascii_case("FETCH") {
                    write.write_all(format!("{tag} BAD unsupported\r\n").as_bytes()).await?;
                    continue;
                }
                let (set, items) = rest.split_once(' ').unwrap_or((rest, ""));
                let items = items.to_ascii_uppercase();
                let messages: Vec<FakeImapMessage> = {
                    let s = state.lock().unwrap();
                    let max = s.messages.iter().map(|m| m.uid).max().unwrap_or(0);
                    s.messages.iter().filter(|m| in_set(set, m.uid, max)).cloned().collect()
                };
                let with_body = items.contains("BODY.PEEK[]") || items.contains("BODY[]");
                let with_header = items.contains("BODY.PEEK[HEADER]");
                if with_body {
                    state.lock().unwrap().body_fetches += messages.len();
                }
                if with_header {
                    state.lock().unwrap().header_fetches += messages.len();
                }
                for (seq, m) in messages.iter().enumerate() {
                    let mut parts = vec![format!("UID {}", m.uid)];
                    if items.contains("X-GM-MSGID") {
                        parts.push(format!("X-GM-MSGID {}", m.msgid));
                    }
                    if items.contains("X-GM-THRID") {
                        parts.push(format!("X-GM-THRID {}", m.thrid));
                    }
                    if items.contains("X-GM-LABELS") {
                        let labels: Vec<String> = m
                            .labels
                            .iter()
                            .map(|l| if l.starts_with('\\') { l.clone() } else { format!("\"{l}\"") })
                            .collect();
                        parts.push(format!("X-GM-LABELS ({})", labels.join(" ")));
                    }
                    if items.contains("FLAGS") {
                        parts.push(format!("FLAGS ({})", m.flags.join(" ")));
                    }
                    if items.contains("INTERNALDATE") {
                        parts.push("INTERNALDATE \"01-Sep-2025 10:00:00 +0000\"".to_owned());
                    }
                    if items.contains("RFC822.SIZE") {
                        parts.push(format!("RFC822.SIZE {}", m.raw.len()));
                    }
                    let head = format!("* {} FETCH ({}", seq + 1, parts.join(" "));
                    write.write_all(head.as_bytes()).await?;
                    if with_body {
                        write.write_all(format!(" BODY[] {{{}}}\r\n", m.raw.len()).as_bytes()).await?;
                        write.write_all(&m.raw).await?;
                    } else if with_header {
                        let end = m.raw.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4).unwrap_or(m.raw.len());
                        write.write_all(format!(" BODY[HEADER] {{{end}}}\r\n").as_bytes()).await?;
                        write.write_all(&m.raw[..end]).await?;
                    }
                    write.write_all(b")\r\n").await?;
                }
                write.write_all(format!("{tag} OK Success\r\n").as_bytes()).await?;
            }
            "NOOP" => write.write_all(format!("{tag} OK Success\r\n").as_bytes()).await?,
            "LOGOUT" => {
                write.write_all(b"* BYE LOGOUT Requested\r\n").await?;
                write.write_all(format!("{tag} OK 73 GoodBye\r\n").as_bytes()).await?;
                return Ok(());
            }
            _ => write.write_all(format!("{tag} BAD Unknown or unauthenticated command\r\n").as_bytes()).await?,
        }
    }
}

/// Whether `uid` is in an IMAP sequence set like `1:*`, `3,5:7`.
fn in_set(set: &str, uid: u32, max: u32) -> bool {
    set.split(',').any(|part| {
        let bound = |s: &str| if s == "*" { Some(max) } else { s.parse::<u32>().ok() };
        match part.split_once(':') {
            Some((a, b)) => match (bound(a), bound(b)) {
                (Some(a), Some(b)) => (a.min(b)..=a.max(b)).contains(&uid),
                _ => false,
            },
            None => bound(part) == Some(uid),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct XOAuth2(String);
    impl async_imap::Authenticator for XOAuth2 {
        type Response = String;
        fn process(&mut self, _challenge: &[u8]) -> String {
            self.0.clone()
        }
    }

    #[tokio::test]
    async fn a_real_imap_client_can_log_in_select_and_fetch() {
        let server = FakeImapServer::start("tok").await;
        server.add(FakeImapMessage {
            uid: 4,
            msgid: 1_700_000_000_000_000_001,
            thrid: 1_700_000_000_000_000_000,
            labels: vec!["\\Inbox".into(), "Clients/Acme".into()],
            flags: vec!["\\Seen".into()],
            raw: b"From: a@example.com\r\nSubject: Hi\r\n\r\nHello\r\n".to_vec(),
        });
        let stream = TcpStream::connect(server.addr).await.unwrap();
        let mut client = async_imap::Client::new(stream);
        client.read_response().await.unwrap().unwrap(); // greeting
        let refused =
            client.authenticate("XOAUTH2", XOAuth2("user=me@example.com\u{1}auth=Bearer nope\u{1}\u{1}".into())).await;
        let client = match refused {
            Err((_, client)) => client,
            Ok(_) => panic!("a wrong token must be refused"),
        };
        let mut session = client
            .authenticate("XOAUTH2", XOAuth2("user=me@example.com\u{1}auth=Bearer tok\u{1}\u{1}".into()))
            .await
            .map_err(|(e, _)| e)
            .unwrap();
        let mailbox = session.examine("[Gmail]/All Mail").await.unwrap();
        assert_eq!(mailbox.exists, 1);
        use futures::TryStreamExt;
        let fetched: Vec<_> = session
            .uid_fetch("1:*", "(UID X-GM-MSGID X-GM-LABELS FLAGS BODY.PEEK[])")
            .await
            .unwrap()
            .try_collect()
            .await
            .unwrap();
        assert_eq!(fetched.len(), 1);
        assert_eq!(fetched[0].uid, Some(4));
        assert_eq!(fetched[0].gmail_msg_id(), Some(&1_700_000_000_000_000_001));
        let labels: Vec<String> = fetched[0].gmail_labels().unwrap().iter().map(|l| l.to_string()).collect();
        assert_eq!(labels, vec!["\\Inbox".to_owned(), "Clients/Acme".to_owned()]);
        assert!(fetched[0].body().unwrap().ends_with(b"Hello\r\n"));
        assert_eq!(server.logins(), 1);
        assert_eq!(server.body_fetches(), 1);
        session.logout().await.unwrap();
    }

    #[test]
    fn sequence_sets() {
        assert!(in_set("1:*", 9, 9));
        assert!(in_set("3,5:7", 6, 9));
        assert!(!in_set("3,5:7", 4, 9));
        assert!(in_set("7:5", 6, 9), "reversed ranges are ranges");
        assert!(!in_set("x", 1, 9));
    }
}
