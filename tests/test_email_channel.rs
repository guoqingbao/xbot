use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::NaiveDate;
use mailparse::parse_mail;
use serde_json::json;
use xbot::channels::{
    Channel, EmailBackend, EmailBackendError, EmailChannel, EmailConfig, EmailSearchCriteria,
    OutgoingEmail, RawEmail,
};
use xbot::storage::{MessageBus, OutboundMessage};

fn make_config() -> serde_json::Value {
    json!({
        "enabled": true,
        "consentGranted": true,
        "imapHost": "imap.example.com",
        "imapPort": 993,
        "imapUsername": "bot@example.com",
        "imapPassword": "secret",
        "smtpHost": "smtp.example.com",
        "smtpPort": 587,
        "smtpUsername": "bot@example.com",
        "smtpPassword": "secret",
        "markSeen": true,
        "allowFrom": ["*"]
    })
}

fn make_raw_email(from_addr: &str, subject: &str, body: &str, uid: &str) -> RawEmail {
    RawEmail {
        uid: uid.to_string(),
        raw: format!(
            "From: {from_addr}\r\nTo: bot@example.com\r\nSubject: {subject}\r\nMessage-ID: <m1@example.com>\r\nDate: Sat, 07 Feb 2026 10:00:00 +0000\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n{body}\r\n"
        )
        .into_bytes(),
    }
}

#[derive(Default)]
struct FakeEmailBackend {
    fetch_responses: Mutex<VecDeque<std::result::Result<Vec<RawEmail>, EmailBackendError>>>,
    fetch_calls: Mutex<Vec<(EmailSearchCriteria, bool, usize)>>,
    sent_messages: Mutex<Vec<OutgoingEmail>>,
}

#[async_trait]
impl EmailBackend for FakeEmailBackend {
    async fn fetch_messages(
        &self,
        _config: &EmailConfig,
        criteria: EmailSearchCriteria,
        mark_seen: bool,
        limit: usize,
    ) -> std::result::Result<Vec<RawEmail>, EmailBackendError> {
        self.fetch_calls
            .lock()
            .unwrap()
            .push((criteria, mark_seen, limit));
        self.fetch_responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Ok(Vec::new()))
    }

    async fn send_message(
        &self,
        _config: &EmailConfig,
        message: OutgoingEmail,
    ) -> std::result::Result<(), EmailBackendError> {
        self.sent_messages.lock().unwrap().push(message);
        Ok(())
    }
}

#[tokio::test]
async fn fetch_new_messages_parses_unseen_and_dedupes_uid() {
    let backend = Arc::new(FakeEmailBackend::default());
    backend
        .fetch_responses
        .lock()
        .unwrap()
        .push_back(Ok(vec![make_raw_email(
            "alice@example.com",
            "Invoice",
            "Please pay",
            "123",
        )]));
    backend
        .fetch_responses
        .lock()
        .unwrap()
        .push_back(Ok(vec![make_raw_email(
            "alice@example.com",
            "Invoice",
            "Please pay",
            "123",
        )]));

    let channel = EmailChannel::new(
        make_config(),
        MessageBus::new(8),
        PathBuf::new(),
        String::new(),
    )
    .unwrap()
    .with_backend(backend.clone());
    let items = channel.fetch_new_messages().await.unwrap();

    assert_eq!(items.len(), 1);
    assert_eq!(items[0].sender, "alice@example.com");
    assert_eq!(items[0].subject, "Invoice");
    assert!(items[0].content.contains("Please pay"));

    let items_again = channel.fetch_new_messages().await.unwrap();
    assert!(items_again.is_empty());
}

#[tokio::test]
async fn fetch_new_messages_retries_once_when_backend_goes_stale() {
    let backend = Arc::new(FakeEmailBackend::default());
    backend
        .fetch_responses
        .lock()
        .unwrap()
        .push_back(Err(EmailBackendError::stale("socket error")));
    backend
        .fetch_responses
        .lock()
        .unwrap()
        .push_back(Ok(vec![make_raw_email(
            "alice@example.com",
            "Invoice",
            "Please pay",
            "123",
        )]));

    let channel = EmailChannel::new(
        make_config(),
        MessageBus::new(8),
        PathBuf::new(),
        String::new(),
    )
    .unwrap()
    .with_backend(backend.clone());
    let items = channel.fetch_new_messages().await.unwrap();

    assert_eq!(items.len(), 1);
    assert_eq!(backend.fetch_calls.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn fetch_new_messages_skips_missing_mailbox() {
    let backend = Arc::new(FakeEmailBackend::default());
    backend
        .fetch_responses
        .lock()
        .unwrap()
        .push_back(Err(EmailBackendError::missing_mailbox(
            "Mailbox doesn't exist",
        )));

    let channel = EmailChannel::new(
        make_config(),
        MessageBus::new(8),
        PathBuf::new(),
        String::new(),
    )
    .unwrap()
    .with_backend(backend);
    let items = channel.fetch_new_messages().await.unwrap();
    assert!(items.is_empty());
}

#[test]
fn extract_text_body_falls_back_to_html() {
    let raw = b"From: alice@example.com\r\nTo: bot@example.com\r\nSubject: HTML only\r\nContent-Type: multipart/alternative; boundary=\"b\"\r\n\r\n--b\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<p>Hello<br>world</p>\r\n--b--\r\n";
    let parsed = parse_mail(raw).unwrap();
    let text = EmailChannel::extract_text_body(&parsed).unwrap();
    assert!(text.contains("Hello"));
    assert!(text.contains("world"));
}

#[tokio::test]
async fn start_returns_immediately_without_consent() {
    let backend = Arc::new(FakeEmailBackend::default());
    let mut cfg = make_config();
    cfg["consentGranted"] = json!(false);
    let channel = EmailChannel::new(cfg, MessageBus::new(8), PathBuf::new(), String::new())
        .unwrap()
        .with_backend(backend.clone());
    channel.start().await.unwrap();
    assert!(backend.fetch_calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn send_uses_reply_subject_and_in_reply_to() {
    let backend = Arc::new(FakeEmailBackend::default());
    let channel = EmailChannel::new(
        make_config(),
        MessageBus::new(8),
        PathBuf::new(),
        String::new(),
    )
    .unwrap()
    .with_backend(backend.clone());
    channel.remember_inbound_email("alice@example.com", "Invoice #42", "<m1@example.com>");

    channel
        .send(OutboundMessage {
            channel: "email".to_string(),
            chat_id: "alice@example.com".to_string(),
            content: "Acknowledged.".to_string(),
            reply_to: None,
            media: Vec::new(),
            reasoning_content: None,
            metadata: BTreeMap::new(),
        })
        .await
        .unwrap();

    let sent = &backend.sent_messages.lock().unwrap()[0];
    assert_eq!(sent.subject, "Re: Invoice #42");
    assert_eq!(sent.to, "alice@example.com");
    assert_eq!(sent.in_reply_to.as_deref(), Some("<m1@example.com>"));
}

#[tokio::test]
async fn send_skips_reply_when_auto_reply_disabled_unless_forced() {
    let backend = Arc::new(FakeEmailBackend::default());
    let mut cfg = make_config();
    cfg["autoReplyEnabled"] = json!(false);
    let channel = EmailChannel::new(cfg, MessageBus::new(8), PathBuf::new(), String::new())
        .unwrap()
        .with_backend(backend.clone());
    channel.remember_inbound_email("alice@example.com", "Previous email", "<m1@example.com>");

    channel
        .send(OutboundMessage {
            channel: "email".to_string(),
            chat_id: "alice@example.com".to_string(),
            content: "Should not send.".to_string(),
            reply_to: None,
            media: Vec::new(),
            reasoning_content: None,
            metadata: BTreeMap::new(),
        })
        .await
        .unwrap();
    assert!(backend.sent_messages.lock().unwrap().is_empty());

    channel
        .send(OutboundMessage {
            channel: "email".to_string(),
            chat_id: "alice@example.com".to_string(),
            content: "Force send.".to_string(),
            reply_to: None,
            media: Vec::new(),
            reasoning_content: None,
            metadata: BTreeMap::from([("force_send".to_string(), json!(true))]),
        })
        .await
        .unwrap();
    assert_eq!(backend.sent_messages.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn send_proactive_email_when_auto_reply_disabled() {
    let backend = Arc::new(FakeEmailBackend::default());
    let mut cfg = make_config();
    cfg["autoReplyEnabled"] = json!(false);
    let channel = EmailChannel::new(cfg, MessageBus::new(8), PathBuf::new(), String::new())
        .unwrap()
        .with_backend(backend.clone());

    channel
        .send(OutboundMessage {
            channel: "email".to_string(),
            chat_id: "bob@example.com".to_string(),
            content: "Hello, this is a proactive email.".to_string(),
            reply_to: None,
            media: Vec::new(),
            reasoning_content: None,
            metadata: BTreeMap::new(),
        })
        .await
        .unwrap();
    assert_eq!(backend.sent_messages.lock().unwrap().len(), 1);
    assert_eq!(
        backend.sent_messages.lock().unwrap()[0].to,
        "bob@example.com"
    );
}

#[tokio::test]
async fn fetch_messages_between_dates_uses_date_range_without_mark_seen() {
    let backend = Arc::new(FakeEmailBackend::default());
    backend
        .fetch_responses
        .lock()
        .unwrap()
        .push_back(Ok(vec![make_raw_email(
            "alice@example.com",
            "Status",
            "Yesterday update",
            "999",
        )]));
    let channel = EmailChannel::new(
        make_config(),
        MessageBus::new(8),
        PathBuf::new(),
        String::new(),
    )
    .unwrap()
    .with_backend(backend.clone());

    let items = channel
        .fetch_messages_between_dates(
            NaiveDate::from_ymd_opt(2026, 2, 6).unwrap(),
            NaiveDate::from_ymd_opt(2026, 2, 7).unwrap(),
            10,
        )
        .await
        .unwrap();

    assert_eq!(items.len(), 1);
    assert_eq!(
        backend.fetch_calls.lock().unwrap()[0],
        (
            EmailSearchCriteria::DateRange {
                since: NaiveDate::from_ymd_opt(2026, 2, 6).unwrap(),
                before: NaiveDate::from_ymd_opt(2026, 2, 7).unwrap(),
            },
            false,
            10
        )
    );
}
