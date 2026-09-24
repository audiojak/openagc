//! Gmail REST client (spec §7). A thin `reqwest` client over the dozen
//! endpoints OpenAGC uses; auth, retries and quota come from
//! `provider_api::HttpClient`.
//!
//! Message fetches go out as individual requests multiplexed over HTTP/2
//! with bounded concurrency rather than through the multipart batch
//! endpoint: batching does not reduce quota cost, which is the real limit.

pub mod oauth;
mod wire;

use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::{FuturesUnordered, StreamExt};
use mail_domain::{Label, LabelColor, LabelId, LabelKind, MessageId, ThreadId};
use provider_api::{
    Change, ChangeSet, FetchedMessage, HttpClient, IdPage, LabelOp, ListFilter, MailProvider, PageToken, Priority,
    Profile, ProviderError, ProviderResult, RateLimiter, RetryPolicy, SyncCursor, TokenSource,
};
use serde_json::json;

pub use wire::{decode_base64url, encode_base64url};

pub const GMAIL_API: &str = "https://gmail.googleapis.com/gmail/v1";

/// Quota units per call (spec §7.2; per-minute quotas since 2026-05-01).
pub mod cost {
    pub const PROFILE: u32 = 1;
    pub const LABELS_LIST: u32 = 1;
    pub const MESSAGES_LIST: u32 = 5;
    pub const MESSAGES_GET: u32 = 20;
    pub const HISTORY_LIST: u32 = 2;
    pub const BATCH_MODIFY: u32 = 50;
    pub const TRASH: u32 = 5;
    pub const SEND: u32 = 100;
    pub const ATTACHMENT_GET: u32 = 20;
    pub const DRAFTS_WRITE: u32 = 10;
}

/// Concurrent `messages.get` calls; the rate limiter paces them.
const FETCH_CONCURRENCY: usize = 8;
/// Gmail's `batchModify` accepts at most 1,000 ids.
const MAX_BATCH_MODIFY: usize = 1_000;

pub struct GmailProvider {
    http: HttpClient,
    base: String,
}

impl GmailProvider {
    pub fn new(tokens: Arc<dyn TokenSource>) -> ProviderResult<Self> {
        Self::with_base(tokens, Arc::new(RateLimiter::gmail_default()), RetryPolicy::default(), GMAIL_API)
    }

    /// For tests and fakes: a different base URL, limiter and retry policy.
    pub fn with_base(
        tokens: Arc<dyn TokenSource>,
        limiter: Arc<RateLimiter>,
        retry: RetryPolicy,
        base: &str,
    ) -> ProviderResult<Self> {
        Ok(Self { http: HttpClient::new(tokens, limiter, retry)?, base: base.trim_end_matches('/').to_owned() })
    }

    fn url(&self, path: &str) -> String {
        format!("{}/users/me/{path}", self.base)
    }

    async fn get_message(&self, id: &MessageId, priority: Priority) -> ProviderResult<Option<FetchedMessage>> {
        let url = self.url(&format!("messages/{}", id.as_str()));
        match self
            .http
            .json::<wire::Message>(cost::MESSAGES_GET, priority, |c| c.get(&url).query(&[("format", "full")]))
            .await
        {
            Ok(m) => Ok(Some(wire::to_fetched(m))),
            // Deleted between listing and fetching.
            Err(ProviderError::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

#[async_trait]
impl MailProvider for GmailProvider {
    async fn profile(&self) -> ProviderResult<Profile> {
        let url = self.url("profile");
        let p: wire::Profile = self.http.json(cost::PROFILE, Priority::Interactive, |c| c.get(&url)).await?;
        Ok(Profile { email: p.email_address, messages_total: p.messages_total, cursor: SyncCursor(p.history_id) })
    }

    async fn list_labels(&self) -> ProviderResult<Vec<Label>> {
        let url = self.url("labels");
        let list: wire::LabelList = self.http.json(cost::LABELS_LIST, Priority::Background, |c| c.get(&url)).await?;
        Ok(list
            .labels
            .into_iter()
            .map(|l| Label {
                visible: l.label_list_visibility.as_deref() != Some("labelHide") && !l.id.starts_with("CATEGORY_"),
                kind: if l.kind.as_deref() == Some("user") { LabelKind::User } else { LabelKind::System },
                color: l.color.map(|c| LabelColor { background: c.background_color, text: c.text_color }),
                id: LabelId(l.id),
                name: l.name,
            })
            .collect())
    }

    async fn list_message_ids(&self, filter: &ListFilter, page: Option<PageToken>) -> ProviderResult<IdPage> {
        let url = self.url("messages");
        let mut query: Vec<(&str, String)> = vec![("maxResults", "500".into())];
        for l in &filter.label_ids {
            query.push(("labelIds", l.0.clone()));
        }
        if let Some(q) = &filter.query {
            query.push(("q", q.clone()));
        }
        if filter.include_spam_trash {
            query.push(("includeSpamTrash", "true".into()));
        }
        if let Some(p) = &page {
            query.push(("pageToken", p.0.clone()));
        }
        let list: wire::MessageList =
            self.http.json(cost::MESSAGES_LIST, Priority::Background, |c| c.get(&url).query(&query)).await?;
        Ok(IdPage {
            ids: list.messages.into_iter().map(|m| (MessageId(m.id), ThreadId(m.thread_id))).collect(),
            next: list.next_page_token.map(PageToken),
            estimated_total: list.result_size_estimate,
        })
    }

    async fn fetch_messages(&self, ids: &[MessageId], priority: Priority) -> ProviderResult<Vec<FetchedMessage>> {
        // Keep FETCH_CONCURRENCY requests in flight; the rate limiter paces them.
        let mut pending = FuturesUnordered::new();
        let mut remaining = ids.iter();
        for id in remaining.by_ref().take(FETCH_CONCURRENCY) {
            pending.push(self.get_message(id, priority));
        }
        let mut out = Vec::with_capacity(ids.len());
        while let Some(result) = pending.next().await {
            if let Some(m) = result? {
                out.push(m);
            }
            if let Some(id) = remaining.next() {
                pending.push(self.get_message(id, priority));
            }
        }
        // Keep the caller's order.
        let order: std::collections::HashMap<&str, usize> =
            ids.iter().enumerate().map(|(i, id)| (id.as_str(), i)).collect();
        out.sort_by_key(|m| order.get(m.id.as_str()).copied().unwrap_or(usize::MAX));
        Ok(out)
    }

    async fn changes_since(&self, cursor: &SyncCursor) -> ProviderResult<ChangeSet> {
        let url = self.url("history");
        let mut changes = Vec::new();
        let mut page: Option<String> = None;
        loop {
            let mut query: Vec<(&str, String)> = vec![
                ("startHistoryId", cursor.0.clone()),
                ("maxResults", "500".into()),
                ("historyTypes", "messageAdded".into()),
                ("historyTypes", "messageDeleted".into()),
                ("historyTypes", "labelAdded".into()),
                ("historyTypes", "labelRemoved".into()),
            ];
            if let Some(p) = &page {
                query.push(("pageToken", p.clone()));
            }
            let list: wire::HistoryList =
                match self.http.json(cost::HISTORY_LIST, Priority::Background, |c| c.get(&url).query(&query)).await {
                    Ok(l) => l,
                    // Gmail keeps history for about a week; older cursors 404.
                    Err(ProviderError::NotFound(_)) => return Err(ProviderError::CursorExpired),
                    Err(e) => return Err(e),
                };
            for h in list.history {
                for a in h.messages_added {
                    changes.push(Change::MessageAdded {
                        id: MessageId(a.message.id),
                        thread_id: ThreadId(a.message.thread_id),
                        label_ids: a.message.label_ids.into_iter().map(LabelId).collect(),
                    });
                }
                for d in h.messages_deleted {
                    changes.push(Change::MessageDeleted { id: MessageId(d.message.id) });
                }
                for l in h.labels_added {
                    changes.push(Change::LabelsAdded {
                        id: MessageId(l.message.id),
                        label_ids: l.label_ids.into_iter().map(LabelId).collect(),
                    });
                }
                for l in h.labels_removed {
                    changes.push(Change::LabelsRemoved {
                        id: MessageId(l.message.id),
                        label_ids: l.label_ids.into_iter().map(LabelId).collect(),
                    });
                }
            }
            match list.next_page_token {
                Some(next) => page = Some(next),
                None => return Ok(ChangeSet { changes, cursor: SyncCursor(list.history_id) }),
            }
        }
    }

    async fn modify_labels(&self, op: &LabelOp) -> ProviderResult<()> {
        let url = self.url("messages/batchModify");
        for chunk in op.message_ids.chunks(MAX_BATCH_MODIFY) {
            let body = json!({
                "ids": chunk.iter().map(MessageId::as_str).collect::<Vec<_>>(),
                "addLabelIds": op.add.iter().map(LabelId::as_str).collect::<Vec<_>>(),
                "removeLabelIds": op.remove.iter().map(LabelId::as_str).collect::<Vec<_>>(),
            });
            self.http.empty(cost::BATCH_MODIFY, Priority::Interactive, |c| c.post(&url).json(&body)).await?;
        }
        Ok(())
    }

    async fn move_to_trash(&self, id: &MessageId) -> ProviderResult<()> {
        let url = self.url(&format!("messages/{}/trash", id.as_str()));
        self.http.empty(cost::TRASH, Priority::Interactive, |c| c.post(&url)).await
    }

    async fn send(&self, raw: &[u8], thread: Option<&ThreadId>) -> ProviderResult<MessageId> {
        let url = self.url("messages/send");
        let mut body = json!({ "raw": encode_base64url(raw) });
        if let Some(t) = thread {
            body["threadId"] = json!(t.as_str());
        }
        let sent: wire::SentMessage =
            self.http.json(cost::SEND, Priority::Interactive, |c| c.post(&url).json(&body)).await?;
        Ok(MessageId(sent.id))
    }

    async fn fetch_attachment(&self, message: &MessageId, attachment_id: &str) -> ProviderResult<Vec<u8>> {
        let url = self.url(&format!("messages/{}/attachments/{attachment_id}", message.as_str()));
        let data: wire::AttachmentData =
            self.http.json(cost::ATTACHMENT_GET, Priority::Interactive, |c| c.get(&url)).await?;
        decode_base64url(&data.data).ok_or_else(|| ProviderError::Invalid("attachment data is not base64url".into()))
    }

    async fn save_draft(
        &self,
        existing: Option<&str>,
        raw: &[u8],
        thread: Option<&ThreadId>,
    ) -> ProviderResult<String> {
        let mut message = json!({ "raw": encode_base64url(raw) });
        if let Some(t) = thread {
            message["threadId"] = json!(t.as_str());
        }
        let draft: wire::Draft = match existing {
            Some(id) => {
                let url = self.url(&format!("drafts/{id}"));
                let body = json!({ "id": id, "message": message });
                self.http.json(cost::DRAFTS_WRITE, Priority::Interactive, |c| c.put(&url).json(&body)).await?
            }
            None => {
                let url = self.url("drafts");
                let body = json!({ "message": message });
                self.http.json(cost::DRAFTS_WRITE, Priority::Interactive, |c| c.post(&url).json(&body)).await?
            }
        };
        Ok(draft.id)
    }

    async fn delete_draft(&self, draft_id: &str) -> ProviderResult<()> {
        let url = self.url(&format!("drafts/{draft_id}"));
        self.http.empty(cost::DRAFTS_WRITE, Priority::Interactive, |c| c.delete(&url)).await
    }
}
