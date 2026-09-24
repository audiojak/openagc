//! Gmail client against a wiremock server returning responses shaped like
//! the real API (synthetic content, example.* addresses).

use std::sync::Arc;
use std::time::Duration;

use mail_domain::{LabelId, LabelKind, MessageId, ThreadId};
use provider_api::token::StaticToken;
use provider_api::{
    Change, LabelOp, ListFilter, MailProvider, Priority, ProviderError, RateLimiter, RetryPolicy, SyncCursor,
};
use provider_gmail::{GmailProvider, encode_base64url};
use serde_json::{Value, json};
use wiremock::matchers::{body_json, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn setup() -> (MockServer, GmailProvider) {
    let server = MockServer::start().await;
    let provider = GmailProvider::with_base(
        Arc::new(StaticToken("test-token".into())),
        Arc::new(RateLimiter::new(1_000_000, 0)),
        RetryPolicy { max_attempts: 2, base_delay: Duration::from_millis(1), max_delay: Duration::from_millis(5) },
        &server.uri(),
    )
    .unwrap();
    (server, provider)
}

fn b64(s: &[u8]) -> String {
    encode_base64url(s)
}

fn full_message() -> Value {
    json!({
        "id": "m1", "threadId": "t1",
        "labelIds": ["INBOX", "UNREAD", "Label_3"],
        "snippet": "Here&#39;s the plan &amp; the deck",
        "internalDate": "1789489800000",
        "sizeEstimate": 48213,
        "payload": {
            "partId": "", "mimeType": "multipart/mixed", "filename": "",
            "headers": [
                {"name": "From", "value": "=?UTF-8?B?w4lsb2lzZSBNb3JlYXU=?= <eloise@example.com>"},
                {"name": "To", "value": "Me <me@example.com>, \"Doe, Jamie\" <jamie@example.org>"},
                {"name": "Subject", "value": "Plan & deck"},
                {"name": "Date", "value": "Tue, 15 Sep 2026 09:30:00 -0700"},
                {"name": "Message-ID", "value": "<plan.1@example.com>"},
                {"name": "In-Reply-To", "value": "<root@example.com>"},
                {"name": "References", "value": "<root@example.com>"}
            ],
            "body": {"size": 0},
            "parts": [
                {"partId": "0", "mimeType": "multipart/alternative", "filename": "", "headers": [], "body": {"size": 0}, "parts": [
                    {"partId": "0.0", "mimeType": "text/plain", "filename": "",
                     "headers": [{"name": "Content-Type", "value": "text/plain; charset=UTF-8"}],
                     "body": {"size": 20, "data": b64("Here's the plan.\r\n".as_bytes())}},
                    {"partId": "0.1", "mimeType": "text/html", "filename": "",
                     "headers": [{"name": "Content-Type", "value": "text/html; charset=ISO-8859-1"}],
                     "body": {"size": 40, "data": b64(b"<p>Here's the plan. Gr\xfc\xdfe</p><img src=\"cid:logo@example.com\">")}}
                ]},
                {"partId": "1", "mimeType": "application/pdf", "filename": "deck.pdf",
                 "headers": [{"name": "Content-Disposition", "value": "attachment; filename=\"deck.pdf\""}],
                 "body": {"attachmentId": "ANGjdJ_deck", "size": 45000}},
                {"partId": "2", "mimeType": "image/png", "filename": "logo.png",
                 "headers": [{"name": "Content-ID", "value": "<logo@example.com>"}, {"name": "Content-Disposition", "value": "inline"}],
                 "body": {"attachmentId": "ANGjdJ_logo", "size": 1200}},
                {"partId": "3", "mimeType": "message/rfc822", "filename": "",
                 "headers": [], "body": {"attachmentId": "ANGjdJ_fwd", "size": 900}, "parts": [
                    {"partId": "3.0", "mimeType": "text/plain", "filename": "", "headers": [],
                     "body": {"size": 10, "data": b64(b"forwarded body")}}
                ]}
            ]
        }
    })
}

#[tokio::test]
async fn fetch_walks_a_full_payload() {
    let (server, gmail) = setup().await;
    Mock::given(method("GET"))
        .and(path("/users/me/messages/m1"))
        .and(query_param("format", "full"))
        .respond_with(ResponseTemplate::new(200).set_body_json(full_message()))
        .mount(&server)
        .await;
    let msgs = gmail.fetch_messages(&[MessageId::new("m1")], Priority::Interactive).await.unwrap();
    let m = &msgs[0];
    assert_eq!(m.thread_id, ThreadId::new("t1"));
    assert_eq!(m.label_ids, vec![LabelId::new("INBOX"), LabelId::new("UNREAD"), LabelId::new("Label_3")]);
    assert_eq!(m.snippet, "Here's the plan & the deck", "snippet entities decoded");
    assert_eq!(m.internal_date, 1_789_489_800_000);
    assert_eq!(m.from.as_ref().unwrap().name.as_deref(), Some("Éloise Moreau"));
    assert_eq!(m.to.len(), 2);
    assert_eq!(m.to[1].name.as_deref(), Some("Doe, Jamie"));
    assert_eq!(m.subject, "Plan & deck");
    assert_eq!(m.message_id_header.as_deref(), Some("plan.1@example.com"));
    assert_eq!(m.in_reply_to.as_deref(), Some("root@example.com"));

    let body = m.body.as_ref().expect("full fetch has a body");
    assert_eq!(body.text.as_deref().map(str::trim), Some("Here's the plan."));
    assert!(body.html.as_deref().unwrap().contains("Grüße"), "HTML decoded with its charset");
    assert!(!body.text.as_deref().unwrap().contains("forwarded"), "forwarded parts are not this message's body");

    let names: Vec<&str> = body.attachments.iter().map(|a| a.filename.as_str()).collect();
    assert_eq!(names, vec!["deck.pdf", "logo.png", "forwarded-message.eml"]);
    let logo = &body.attachments[1];
    assert!(logo.is_inline);
    assert_eq!(logo.content_id.as_deref(), Some("logo@example.com"));
    assert_eq!(body.attachments[0].attachment_id.as_deref(), Some("ANGjdJ_deck"));
    assert!(!body.attachments[0].is_inline);
}

#[tokio::test]
async fn fetch_skips_messages_deleted_since_listing_and_keeps_order() {
    let (server, gmail) = setup().await;
    for id in ["a", "c"] {
        let mut m = full_message();
        m["id"] = json!(id);
        Mock::given(path(format!("/users/me/messages/{id}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(m))
            .mount(&server)
            .await;
    }
    Mock::given(path("/users/me/messages/b"))
        .respond_with(
            ResponseTemplate::new(404).set_body_json(json!({"error": {"message": "Requested entity was not found."}})),
        )
        .mount(&server)
        .await;
    let ids: Vec<MessageId> = ["a", "b", "c"].into_iter().map(MessageId::new).collect();
    let msgs = gmail.fetch_messages(&ids, Priority::Background).await.unwrap();
    assert_eq!(msgs.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), vec!["a", "c"]);
}

#[tokio::test]
async fn profile_labels_and_listing() {
    let (server, gmail) = setup().await;
    Mock::given(path("/users/me/profile"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"emailAddress": "me@example.com", "messagesTotal": 1234, "threadsTotal": 900, "historyId": "98765"}),
        ))
        .mount(&server)
        .await;
    Mock::given(path("/users/me/labels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"labels": [
            {"id": "INBOX", "name": "INBOX", "type": "system"},
            {"id": "CATEGORY_PROMOTIONS", "name": "CATEGORY_PROMOTIONS", "type": "system"},
            {"id": "Label_3", "name": "Receipts", "type": "user", "labelListVisibility": "labelShow",
             "color": {"backgroundColor": "#fb4c2f", "textColor": "#ffffff"}},
            {"id": "Label_4", "name": "Hidden", "type": "user", "labelListVisibility": "labelHide"}
        ]})))
        .mount(&server)
        .await;
    Mock::given(path("/users/me/messages"))
        .and(query_param("labelIds", "INBOX"))
        .and(query_param("maxResults", "500"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "messages": [{"id": "m1", "threadId": "t1"}, {"id": "m2", "threadId": "t1"}],
            "nextPageToken": "p2", "resultSizeEstimate": 1200
        })))
        .mount(&server)
        .await;

    let p = gmail.profile().await.unwrap();
    assert_eq!((p.email.as_str(), p.messages_total, p.cursor), ("me@example.com", 1234, SyncCursor("98765".into())));

    let labels = gmail.list_labels().await.unwrap();
    let receipts = labels.iter().find(|l| l.id.as_str() == "Label_3").unwrap();
    assert_eq!(receipts.kind, LabelKind::User);
    assert_eq!(receipts.color.as_ref().unwrap().background, "#fb4c2f");
    assert!(!labels.iter().find(|l| l.id.as_str() == "Label_4").unwrap().visible);
    assert!(!labels.iter().find(|l| l.id.as_str() == "CATEGORY_PROMOTIONS").unwrap().visible);

    let page = gmail
        .list_message_ids(&ListFilter { label_ids: vec![LabelId::new("INBOX")], ..Default::default() }, None)
        .await
        .unwrap();
    assert_eq!(page.ids.len(), 2);
    assert_eq!(page.next.unwrap().0, "p2");
    assert_eq!(page.estimated_total, Some(1200));
}

#[tokio::test]
async fn history_pages_are_followed_and_mapped_to_changes() {
    let (server, gmail) = setup().await;
    Mock::given(path("/users/me/history"))
        .and(query_param("startHistoryId", "100"))
        .and(query_param("pageToken", "h2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "history": [{"id": "105", "messagesDeleted": [{"message": {"id": "m9", "threadId": "t9"}}]}],
            "historyId": "110"
        })))
        .mount(&server)
        .await;
    Mock::given(path("/users/me/history"))
        .and(query_param("startHistoryId", "100"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "history": [
                {"id": "101", "messagesAdded": [{"message": {"id": "m1", "threadId": "t1", "labelIds": ["INBOX", "UNREAD"]}}]},
                {"id": "102", "labelsRemoved": [{"message": {"id": "m1", "threadId": "t1"}, "labelIds": ["UNREAD"]}]},
                {"id": "103", "labelsAdded": [{"message": {"id": "m2", "threadId": "t2"}, "labelIds": ["STARRED"]}]}
            ],
            "nextPageToken": "h2",
            "historyId": "110"
        })))
        .mount(&server)
        .await;
    let set = gmail.changes_since(&SyncCursor("100".into())).await.unwrap();
    assert_eq!(set.cursor, SyncCursor("110".into()));
    assert_eq!(
        set.changes,
        vec![
            Change::MessageAdded {
                id: MessageId::new("m1"),
                thread_id: ThreadId::new("t1"),
                label_ids: vec![LabelId::new("INBOX"), LabelId::new("UNREAD")]
            },
            Change::LabelsRemoved { id: MessageId::new("m1"), label_ids: vec![LabelId::new("UNREAD")] },
            Change::LabelsAdded { id: MessageId::new("m2"), label_ids: vec![LabelId::new("STARRED")] },
            Change::MessageDeleted { id: MessageId::new("m9") },
        ]
    );
}

#[tokio::test]
async fn an_expired_history_id_means_full_resync() {
    let (server, gmail) = setup().await;
    Mock::given(path("/users/me/history"))
        .respond_with(
            ResponseTemplate::new(404)
                .set_body_json(json!({"error": {"code": 404, "message": "Requested entity was not found."}})),
        )
        .mount(&server)
        .await;
    assert_eq!(gmail.changes_since(&SyncCursor("1".into())).await.unwrap_err(), ProviderError::CursorExpired);
}

#[tokio::test]
async fn batch_modify_chunks_at_a_thousand_ids() {
    let (server, gmail) = setup().await;
    Mock::given(method("POST"))
        .and(path("/users/me/messages/batchModify"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;
    let ids: Vec<MessageId> = (0..2_500).map(|i| MessageId::new(format!("m{i}"))).collect();
    gmail.modify_labels(&LabelOp { message_ids: ids, add: vec![], remove: vec![LabelId::new("INBOX")] }).await.unwrap();
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 3);
    let first: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(first["ids"].as_array().unwrap().len(), 1_000);
    assert_eq!(first["removeLabelIds"], json!(["INBOX"]));
}

#[tokio::test]
async fn send_posts_base64url_raw_with_the_thread() {
    let (server, gmail) = setup().await;
    let raw = b"From: me@example.com\r\nTo: a@example.com\r\nSubject: Hi\r\n\r\nHello ~?>";
    Mock::given(method("POST"))
        .and(path("/users/me/messages/send"))
        .and(body_json(json!({"raw": encode_base64url(raw), "threadId": "t1"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "sent1", "threadId": "t1"})))
        .mount(&server)
        .await;
    let id = gmail.send(raw, Some(&ThreadId::new("t1"))).await.unwrap();
    assert_eq!(id, MessageId::new("sent1"));
}

#[tokio::test]
async fn drafts_are_created_replaced_and_deleted() {
    let (server, gmail) = setup().await;
    let raw = b"From: me@example.com\r\nSubject: Draft\r\n\r\nWIP";
    Mock::given(method("POST"))
        .and(path("/users/me/drafts"))
        .and(body_json(json!({"message": {"raw": encode_base64url(raw), "threadId": "t1"}})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "r-1", "message": {"id": "m9"}})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/users/me/drafts/r-1"))
        .and(body_json(json!({"id": "r-1", "message": {"raw": encode_base64url(raw)}})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "r-1", "message": {"id": "m10"}})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/users/me/drafts/r-1"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/users/me/drafts/gone"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    assert_eq!(gmail.save_draft(None, raw, Some(&ThreadId::new("t1"))).await.unwrap(), "r-1");
    assert_eq!(gmail.save_draft(Some("r-1"), raw, None).await.unwrap(), "r-1");
    gmail.delete_draft("r-1").await.unwrap();
    assert!(matches!(gmail.delete_draft("gone").await, Err(ProviderError::NotFound(_))));
}

#[tokio::test]
async fn attachments_are_decoded() {
    let (server, gmail) = setup().await;
    Mock::given(path("/users/me/messages/m1/attachments/ANGjdJ_deck"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"size": 8, "data": encode_base64url(b"%PDF-1.4")})),
        )
        .mount(&server)
        .await;
    assert_eq!(gmail.fetch_attachment(&MessageId::new("m1"), "ANGjdJ_deck").await.unwrap(), b"%PDF-1.4");
}

#[test]
fn padded_and_unpadded_base64url_both_decode() {
    assert_eq!(provider_gmail::decode_base64url("aGk").unwrap(), b"hi");
    assert_eq!(provider_gmail::decode_base64url("aGk=").unwrap(), b"hi");
    assert_eq!(provider_gmail::decode_base64url("-_8").unwrap(), vec![0xfb, 0xff]);
}
