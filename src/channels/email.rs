use std::any::Any;
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use chrono::NaiveDate;
use html2text::from_read;
use lettre::message::header::{InReplyTo, References};
use lettre::message::{Mailbox, Message, SinglePart};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{SmtpTransport, Transport};
use mailparse::{MailHeaderMap, ParsedMail, parse_mail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinHandle;

use super::{Channel, ChannelBase};
use crate::storage::{MessageBus, OutboundMessage};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EmailConfig {
    pub enabled: bool,
    #[serde(alias = "consentGranted")]
    pub consent_granted: bool,
    #[serde(alias = "imapHost")]
    pub imap_host: String,
    #[serde(alias = "imapPort")]
    pub imap_port: u16,
    #[serde(alias = "imapUsername")]
    pub imap_username: String,
    #[serde(alias = "imapPassword")]
    pub imap_password: String,
    #[serde(alias = "imapMailbox")]
    pub imap_mailbox: String,
    #[serde(alias = "imapUseSsl")]
    pub imap_use_ssl: bool,
    #[serde(alias = "smtpHost")]
    pub smtp_host: String,
    #[serde(alias = "smtpPort")]
    pub smtp_port: u16,
    #[serde(alias = "smtpUsername")]
    pub smtp_username: String,
    #[serde(alias = "smtpPassword")]
    pub smtp_password: String,
    #[serde(alias = "smtpUseTls")]
    pub smtp_use_tls: bool,
    #[serde(alias = "smtpUseSsl")]
    pub smtp_use_ssl: bool,
    #[serde(alias = "fromAddress")]
    pub from_address: String,
    #[serde(alias = "autoReplyEnabled")]
    pub auto_reply_enabled: bool,
    #[serde(alias = "pollIntervalSeconds")]
    pub poll_interval_seconds: u64,
    #[serde(alias = "markSeen")]
    pub mark_seen: bool,
    #[serde(alias = "maxBodyChars")]
    pub max_body_chars: usize,
    #[serde(alias = "subjectPrefix")]
    pub subject_prefix: String,
    #[serde(alias = "allowFrom")]
    pub allow_from: Vec<String>,
}

impl Default for EmailConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            consent_granted: false,
            imap_host: String::new(),
            imap_port: 993,
            imap_username: String::new(),
            imap_password: String::new(),
            imap_mailbox: "INBOX".to_string(),
            imap_use_ssl: true,
            smtp_host: String::new(),
            smtp_port: 587,
            smtp_username: String::new(),
            smtp_password: String::new(),
            smtp_use_tls: true,
            smtp_use_ssl: false,
            from_address: String::new(),
            auto_reply_enabled: true,
            poll_interval_seconds: 30,
            mark_seen: true,
            max_body_chars: 12_000,
            subject_prefix: "Re: ".to_string(),
            allow_from: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmailSearchCriteria {
    Unseen,
    DateRange { since: NaiveDate, before: NaiveDate },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawEmail {
    pub uid: String,
    pub raw: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutgoingEmail {
    pub from: String,
    pub to: String,
    pub subject: String,
    pub body: String,
    pub in_reply_to: Option<String>,
    pub references: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedInboundEmail {
    pub sender: String,
    pub subject: String,
    pub message_id: String,
    pub content: String,
    pub metadata: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmailBackendErrorKind {
    StaleConnection,
    MissingMailbox,
    Other,
}

#[derive(Debug, Error)]
#[error("{message}")]
pub struct EmailBackendError {
    pub kind: EmailBackendErrorKind,
    pub message: String,
}

impl EmailBackendError {
    pub fn stale(message: impl Into<String>) -> Self {
        Self {
            kind: EmailBackendErrorKind::StaleConnection,
            message: message.into(),
        }
    }

    pub fn missing_mailbox(message: impl Into<String>) -> Self {
        Self {
            kind: EmailBackendErrorKind::MissingMailbox,
            message: message.into(),
        }
    }

    pub fn other(message: impl Into<String>) -> Self {
        Self {
            kind: EmailBackendErrorKind::Other,
            message: message.into(),
        }
    }
}

#[async_trait]
pub trait EmailBackend: Send + Sync {
    async fn fetch_messages(
        &self,
        config: &EmailConfig,
        criteria: EmailSearchCriteria,
        mark_seen: bool,
        limit: usize,
    ) -> std::result::Result<Vec<RawEmail>, EmailBackendError>;

    async fn send_message(
        &self,
        config: &EmailConfig,
        message: OutgoingEmail,
    ) -> std::result::Result<(), EmailBackendError>;
}

#[derive(Default)]
struct ProcessedUids {
    order: VecDeque<String>,
    seen: HashSet<String>,
}

impl ProcessedUids {
    fn contains(&self, uid: &str) -> bool {
        self.seen.contains(uid)
    }

    fn insert(&mut self, uid: String, max: usize) {
        if self.seen.insert(uid.clone()) {
            self.order.push_back(uid);
        }
        if self.order.len() > max {
            let keep = max / 2;
            while self.order.len() > keep {
                if let Some(old) = self.order.pop_front() {
                    self.seen.remove(&old);
                }
            }
        }
    }
}

pub struct SystemEmailBackend;

impl SystemEmailBackend {
    fn format_imap_date(value: NaiveDate) -> String {
        value.format("%d-%b-%Y").to_string()
    }
}

#[async_trait]
impl EmailBackend for SystemEmailBackend {
    async fn fetch_messages(
        &self,
        config: &EmailConfig,
        criteria: EmailSearchCriteria,
        mark_seen: bool,
        limit: usize,
    ) -> std::result::Result<Vec<RawEmail>, EmailBackendError> {
        #[allow(unused)]
        use axum::routing::connect;
        let config = config.clone();
        tokio::task::spawn_blocking(move || {
            let mailbox = if config.imap_mailbox.trim().is_empty() {
                "INBOX".to_string()
            } else {
                config.imap_mailbox.clone()
            };
            if !config.imap_use_ssl {
                return Err(EmailBackendError::other(
                    "plain IMAP is not supported by the current Rust backend",
                ));
            }
            let client = imap::ClientBuilder::new(&config.imap_host, config.imap_port)
                .mode(imap::ConnectionMode::Tls)
                .connect()
                .map_err(|err| EmailBackendError::other(err.to_string()))?;
            let mut session = client
                .login(&config.imap_username, &config.imap_password)
                .map_err(|(err, _)| EmailBackendError::other(err.to_string()))?;

            let selected = session.select(&mailbox).map_err(|err| {
                let message = err.to_string().to_lowercase();
                if [
                    "mailbox doesn't exist",
                    "select failed",
                    "no such mailbox",
                    "can't open mailbox",
                    "does not exist",
                ]
                .iter()
                .any(|marker| message.contains(marker))
                {
                    EmailBackendError::missing_mailbox(err.to_string())
                } else {
                    EmailBackendError::other(err.to_string())
                }
            })?;
            let _ = selected;
            let query = match criteria {
                EmailSearchCriteria::Unseen => "UNSEEN".to_string(),
                EmailSearchCriteria::DateRange { since, before } => format!(
                    "SINCE {} BEFORE {}",
                    Self::format_imap_date(since),
                    Self::format_imap_date(before)
                ),
            };
            let ids = session.search(query).map_err(|err| {
                let message = err.to_string().to_lowercase();
                if [
                    "disconnected for inactivity",
                    "eof occurred in violation of protocol",
                    "socket error",
                    "connection reset",
                    "broken pipe",
                    "bye",
                ]
                .iter()
                .any(|marker| message.contains(marker))
                {
                    EmailBackendError::stale(err.to_string())
                } else {
                    EmailBackendError::other(err.to_string())
                }
            })?;
            let mut ids: Vec<u32> = ids.into_iter().collect::<Vec<_>>();
            ids.sort_unstable();
            if limit > 0 && ids.len() > limit {
                ids = ids[ids.len() - limit..].to_vec();
            }

            let mut out = Vec::new();
            for id in ids {
                let fetches = session
                    .fetch(id.to_string(), "(BODY.PEEK[] UID)")
                    .map_err(|err| EmailBackendError::other(err.to_string()))?;
                for fetch in fetches.iter() {
                    let raw = fetch.body().unwrap_or_default().to_vec();
                    let uid = fetch.uid.map(|uid| uid.to_string()).unwrap_or_default();
                    if raw.is_empty() {
                        continue;
                    }
                    out.push(RawEmail { uid, raw });
                    if mark_seen {
                        let _ = session.store(id.to_string(), "+FLAGS (\\Seen)");
                    }
                }
            }
            let _ = session.logout();
            Ok(out)
        })
        .await
        .map_err(|err| EmailBackendError::other(err.to_string()))?
    }

    async fn send_message(
        &self,
        config: &EmailConfig,
        message: OutgoingEmail,
    ) -> std::result::Result<(), EmailBackendError> {
        let config = config.clone();
        tokio::task::spawn_blocking(move || {
            let from: Mailbox = message
                .from
                .parse()
                .map_err(|err| EmailBackendError::other(format!("invalid from address: {err}")))?;
            let to: Mailbox = message
                .to
                .parse()
                .map_err(|err| EmailBackendError::other(format!("invalid to address: {err}")))?;
            let mut builder = Message::builder()
                .from(from)
                .to(to)
                .subject(&message.subject);
            if let Some(in_reply_to) = &message.in_reply_to {
                builder = builder.header(InReplyTo::from(in_reply_to.clone()));
            }
            if let Some(references) = &message.references {
                builder = builder.header(References::from(references.clone()));
            }
            let built = builder
                .singlepart(SinglePart::plain(message.body))
                .map_err(|err| EmailBackendError::other(err.to_string()))?;
            let creds =
                Credentials::new(config.smtp_username.clone(), config.smtp_password.clone());
            let mailer = if config.smtp_use_ssl {
                SmtpTransport::relay(&config.smtp_host)
                    .map_err(|err| EmailBackendError::other(err.to_string()))?
                    .port(config.smtp_port)
                    .credentials(creds)
                    .build()
            } else if config.smtp_use_tls {
                SmtpTransport::starttls_relay(&config.smtp_host)
                    .map_err(|err| EmailBackendError::other(err.to_string()))?
                    .port(config.smtp_port)
                    .credentials(creds)
                    .build()
            } else {
                SmtpTransport::builder_dangerous(&config.smtp_host)
                    .port(config.smtp_port)
                    .credentials(creds)
                    .build()
            };
            mailer
                .send(&built)
                .map_err(|err| EmailBackendError::other(err.to_string()))?;
            Ok(())
        })
        .await
        .map_err(|err| EmailBackendError::other(err.to_string()))?
    }
}

pub struct EmailChannel {
    base: ChannelBase,
    config: EmailConfig,
    backend: Arc<dyn EmailBackend>,
    poll_task: AsyncMutex<Option<JoinHandle<()>>>,
    last_subject_by_chat: Arc<Mutex<BTreeMap<String, String>>>,
    last_message_id_by_chat: Arc<Mutex<BTreeMap<String, String>>>,
    processed_uids: Arc<Mutex<ProcessedUids>>,
}

impl EmailChannel {
    const MAX_PROCESSED_UIDS: usize = 100_000;

    pub fn new(
        config: Value,
        bus: MessageBus,
        workspace: PathBuf,
        transcription_api_key: String,
    ) -> Result<Self> {
        let config: EmailConfig = serde_json::from_value(config)?;
        Ok(Self {
            base: ChannelBase::new(
                serde_json::to_value(&config)?,
                bus,
                workspace,
                transcription_api_key,
            ),
            config,
            backend: Arc::new(SystemEmailBackend),
            poll_task: AsyncMutex::new(None),
            last_subject_by_chat: Arc::new(Mutex::new(BTreeMap::new())),
            last_message_id_by_chat: Arc::new(Mutex::new(BTreeMap::new())),
            processed_uids: Arc::new(Mutex::new(ProcessedUids::default())),
        })
    }

    pub fn default_config() -> Value {
        serde_json::to_value(EmailConfig::default()).expect("serializable email config")
    }

    pub fn with_backend(mut self, backend: Arc<dyn EmailBackend>) -> Self {
        self.backend = backend;
        self
    }

    pub fn validate_config(&self) -> bool {
        [
            (&self.config.imap_host, "imap_host"),
            (&self.config.imap_username, "imap_username"),
            (&self.config.imap_password, "imap_password"),
            (&self.config.smtp_host, "smtp_host"),
            (&self.config.smtp_username, "smtp_username"),
            (&self.config.smtp_password, "smtp_password"),
        ]
        .iter()
        .all(|(value, _)| !value.trim().is_empty())
    }

    pub async fn fetch_new_messages(&self) -> Result<Vec<ParsedInboundEmail>> {
        self.fetch_messages(EmailSearchCriteria::Unseen, self.config.mark_seen, true, 0)
            .await
    }

    pub async fn fetch_messages_between_dates(
        &self,
        start_date: NaiveDate,
        end_date: NaiveDate,
        limit: usize,
    ) -> Result<Vec<ParsedInboundEmail>> {
        if end_date <= start_date {
            return Ok(Vec::new());
        }
        self.fetch_messages(
            EmailSearchCriteria::DateRange {
                since: start_date,
                before: end_date,
            },
            false,
            false,
            limit.max(1),
        )
        .await
    }

    async fn fetch_messages(
        &self,
        criteria: EmailSearchCriteria,
        mark_seen: bool,
        dedupe: bool,
        limit: usize,
    ) -> Result<Vec<ParsedInboundEmail>> {
        let mut collected = Vec::new();
        for attempt in 0..2 {
            match self
                .backend
                .fetch_messages(&self.config, criteria.clone(), mark_seen, limit)
                .await
            {
                Ok(raws) => {
                    collected.extend(self.parse_raw_messages(raws, dedupe)?);
                    return Ok(collected);
                }
                Err(err) if err.kind == EmailBackendErrorKind::MissingMailbox => {
                    return Ok(collected);
                }
                Err(err) if err.kind == EmailBackendErrorKind::StaleConnection && attempt == 0 => {
                    continue;
                }
                Err(err) => return Err(anyhow!(err)),
            }
        }
        Ok(collected)
    }

    fn parse_raw_messages(
        &self,
        raws: Vec<RawEmail>,
        dedupe: bool,
    ) -> Result<Vec<ParsedInboundEmail>> {
        let mut out = Vec::new();
        for raw in raws {
            if dedupe
                && !raw.uid.is_empty()
                && self
                    .processed_uids
                    .lock()
                    .expect("processed uid lock poisoned")
                    .contains(&raw.uid)
            {
                continue;
            }
            let parsed = parse_mail(&raw.raw)?;
            let sender = Self::extract_sender(parsed.headers.get_first_value("From").as_deref());
            if sender.is_empty() {
                continue;
            }
            let subject = parsed
                .headers
                .get_first_value("Subject")
                .unwrap_or_default();
            let date_value = parsed.headers.get_first_value("Date").unwrap_or_default();
            let message_id = parsed
                .headers
                .get_first_value("Message-ID")
                .unwrap_or_default()
                .trim()
                .to_string();
            let body = Self::extract_text_body(&parsed)
                .unwrap_or_default()
                .trim()
                .to_string();
            let body = if body.is_empty() {
                "(empty email body)".to_string()
            } else {
                body
            };
            let body = body
                .chars()
                .take(self.config.max_body_chars)
                .collect::<String>();
            let content = format!(
                "Email received.\nFrom: {sender}\nSubject: {subject}\nDate: {date_value}\n\n{body}"
            );
            let metadata = BTreeMap::from([
                ("message_id".to_string(), json!(message_id)),
                ("subject".to_string(), json!(subject)),
                ("date".to_string(), json!(date_value)),
                ("sender_email".to_string(), json!(sender)),
                ("uid".to_string(), json!(raw.uid)),
            ]);
            out.push(ParsedInboundEmail {
                sender: sender.clone(),
                subject: subject.clone(),
                message_id: message_id.clone(),
                content,
                metadata,
            });
            if dedupe && !raw.uid.is_empty() {
                self.processed_uids
                    .lock()
                    .expect("processed uid lock poisoned")
                    .insert(raw.uid, Self::MAX_PROCESSED_UIDS);
            }
        }
        Ok(out)
    }

    pub fn extract_text_body(parsed: &ParsedMail<'_>) -> Result<String> {
        let mut plain_parts = Vec::new();
        let mut html_parts = Vec::new();

        fn walk(
            parsed: &ParsedMail<'_>,
            plain_parts: &mut Vec<String>,
            html_parts: &mut Vec<String>,
        ) -> Result<()> {
            let disposition = parsed.get_headers().get_first_value("Content-Disposition");
            if disposition
                .as_deref()
                .unwrap_or_default()
                .to_ascii_lowercase()
                .contains("attachment")
            {
                return Ok(());
            }
            if parsed.subparts.is_empty() {
                let body = parsed.get_body().unwrap_or_default();
                match parsed.ctype.mimetype.as_str() {
                    "text/plain" => plain_parts.push(body),
                    "text/html" => html_parts.push(body),
                    _ => {}
                }
                return Ok(());
            }
            for part in &parsed.subparts {
                walk(part, plain_parts, html_parts)?;
            }
            Ok(())
        }

        walk(parsed, &mut plain_parts, &mut html_parts)?;

        if !plain_parts.is_empty() {
            return Ok(plain_parts.join("\n\n").trim().to_string());
        }
        if !html_parts.is_empty() {
            return Ok(from_read(html_parts.join("\n\n").as_bytes(), 120)?
                .trim()
                .to_string());
        }
        Ok(String::new())
    }

    fn extract_sender(from_header: Option<&str>) -> String {
        let Some(from_header) = from_header else {
            return String::new();
        };
        if let Some(start) = from_header.find('<') {
            if let Some(end) = from_header[start + 1..].find('>') {
                return from_header[start + 1..start + 1 + end]
                    .trim()
                    .to_ascii_lowercase();
            }
        }
        let candidate = from_header.trim().trim_matches('"').to_ascii_lowercase();
        if candidate.contains('@') {
            candidate
        } else {
            String::new()
        }
    }

    fn reply_subject(&self, base_subject: &str) -> String {
        let subject = if base_subject.trim().is_empty() {
            "xbot reply"
        } else {
            base_subject.trim()
        };
        if subject.to_ascii_lowercase().starts_with("re:") {
            subject.to_string()
        } else {
            format!("{}{}", self.config.subject_prefix, subject)
        }
    }

    pub fn remember_inbound_email(
        &self,
        sender: impl Into<String>,
        subject: impl Into<String>,
        message_id: impl Into<String>,
    ) {
        let sender = sender.into();
        self.last_subject_by_chat
            .lock()
            .expect("email subject lock poisoned")
            .insert(sender.clone(), subject.into());
        let message_id = message_id.into();
        if !message_id.is_empty() {
            self.last_message_id_by_chat
                .lock()
                .expect("email message id lock poisoned")
                .insert(sender, message_id);
        }
    }
}

#[async_trait]
impl Channel for EmailChannel {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn base(&self) -> &ChannelBase {
        &self.base
    }

    fn name(&self) -> &'static str {
        "email"
    }

    fn display_name(&self) -> &'static str {
        "Email"
    }

    fn setup_instructions(&self) -> &'static str {
        "Email uses IMAP for receiving and SMTP for sending.\n\
         \n\
         1. Use an email account that supports IMAP/SMTP (Gmail, Outlook, etc.)\n\
         2. For Gmail: enable 'Less secure apps' or generate an App Password\n\
         3. Note the IMAP host/port and SMTP host/port for your provider\n\
         4. Configure xbot:\n\
         \n\
            \"email\": {\n\
              \"enabled\": true,\n\
              \"allowFrom\": [\"user@example.com\"],\n\
              \"imapHost\": \"imap.gmail.com\",\n\
              \"imapPort\": 993,\n\
              \"smtpHost\": \"smtp.gmail.com\",\n\
              \"smtpPort\": 465,\n\
              \"address\": \"bot@gmail.com\",\n\
              \"password\": \"<app-password>\"\n\
            }\n\
         \n\
         5. Run: xbot run"
    }

    async fn start(&self) -> Result<()> {
        if !self.config.consent_granted || !self.validate_config() {
            self.base.set_running(false);
            return Ok(());
        }
        self.base.set_running(true);
        let base = self.base.clone();
        let config = self.config.clone();
        let backend = self.backend.clone();
        let last_subject = self.last_subject_by_chat.clone();
        let last_message_id = self.last_message_id_by_chat.clone();
        let processed_uids = self.processed_uids.clone();
        let handle = tokio::spawn(async move {
            let channel = EmailChannel {
                base,
                config,
                backend,
                poll_task: AsyncMutex::new(None),
                last_subject_by_chat: last_subject,
                last_message_id_by_chat: last_message_id,
                processed_uids,
            };
            let poll_seconds = channel.config.poll_interval_seconds.max(5);
            while channel.base.is_running() {
                if let Ok(items) = channel.fetch_new_messages().await {
                    for item in items {
                        channel
                            .last_subject_by_chat
                            .lock()
                            .expect("email subject lock poisoned")
                            .insert(item.sender.clone(), item.subject.clone());
                        if !item.message_id.is_empty() {
                            channel
                                .last_message_id_by_chat
                                .lock()
                                .expect("email message id lock poisoned")
                                .insert(item.sender.clone(), item.message_id.clone());
                        }
                        let _ = channel
                            .base
                            .handle_message(
                                channel.name(),
                                &item.sender,
                                &item.sender,
                                &item.content,
                                None,
                                Some(item.metadata),
                                None,
                            )
                            .await;
                    }
                }
                tokio::time::sleep(Duration::from_secs(poll_seconds)).await;
            }
        });
        *self.poll_task.lock().await = Some(handle);
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.base.set_running(false);
        if let Some(handle) = self.poll_task.lock().await.take() {
            handle.abort();
        }
        Ok(())
    }

    async fn send(&self, msg: OutboundMessage) -> Result<()> {
        if !self.config.consent_granted || self.config.smtp_host.trim().is_empty() {
            return Ok(());
        }
        let to_addr = msg.chat_id.trim().to_string();
        if to_addr.is_empty() {
            return Ok(());
        }
        let is_reply = self
            .last_subject_by_chat
            .lock()
            .expect("email subject lock poisoned")
            .contains_key(&to_addr);
        let force_send = msg
            .metadata
            .get("force_send")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if is_reply && !self.config.auto_reply_enabled && !force_send {
            return Ok(());
        }
        let base_subject = self
            .last_subject_by_chat
            .lock()
            .expect("email subject lock poisoned")
            .get(&to_addr)
            .cloned()
            .unwrap_or_else(|| "xbot reply".to_string());
        let subject = msg
            .metadata
            .get("subject")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| self.reply_subject(&base_subject));
        let in_reply_to = self
            .last_message_id_by_chat
            .lock()
            .expect("email message id lock poisoned")
            .get(&to_addr)
            .cloned();
        let email = OutgoingEmail {
            from: if self.config.from_address.trim().is_empty() {
                if self.config.smtp_username.trim().is_empty() {
                    self.config.imap_username.clone()
                } else {
                    self.config.smtp_username.clone()
                }
            } else {
                self.config.from_address.clone()
            },
            to: to_addr,
            subject,
            body: msg.content,
            in_reply_to: in_reply_to.clone(),
            references: in_reply_to,
        };
        self.backend
            .send_message(&self.config, email)
            .await
            .map_err(|err| anyhow!(err))
    }
}
