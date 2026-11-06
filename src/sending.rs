//! Optional outbound submission. Mail contents never enter the durable journal.
use crate::{
    credentials::{Credentials, outside_checkout, read_private_file, write_private_file},
    error::{ErrorCode as E, MailError, Result},
    model::now,
    smtp,
};
use lettre::{
    Address, Message,
    message::{
        Mailbox,
        header::{ContentTransferEncoding, ContentType},
    },
};
use ring::{
    hmac,
    rand::{SecureRandom, SystemRandom},
};
use rmcp::schemars::{self, JsonSchema};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{
    sync::{mpsc, oneshot},
    time::Instant,
};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

pub const BODY_LIMIT: usize = 32768;
pub const MESSAGE_LIMIT: usize = 128 * 1024;
const JOURNAL_LIMIT: u64 = 16 * 1024 * 1024;
const RECORD_LIMIT: i64 = 10000;

pub(crate) fn valid_address(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 254
        && s.is_ascii()
        && !s
            .chars()
            .any(|c| c.is_ascii_whitespace() || c.is_control() || "<>()[\\]\\\";,".contains(c))
        && s.parse::<Address>().is_ok()
}
pub fn valid_key(s: &str) -> bool {
    uuid::Uuid::parse_str(s).is_ok_and(|id| {
        id.to_string() == s
            && id.get_version_num() == 4
            && id.get_variant() == uuid::Variant::RFC4122
    })
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SendSettings {
    pub(crate) sender_address: String,
    pub(crate) smtp_username: String,
}
impl std::fmt::Debug for SendSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SendSettings([REDACTED])")
    }
}
impl SendSettings {
    pub fn new(sender_address: String, smtp_username: String) -> Result<Self> {
        if !valid_address(&sender_address) || !valid_address(&smtp_username) {
            return Err(MailError(E::ConfigurationError));
        }
        Ok(Self {
            sender_address,
            smtp_username,
        })
    }
    pub fn load(path: &Path) -> Result<Self> {
        let raw: Self = serde_json::from_slice(&read_private_file(path)?)
            .map_err(|_| MailError(E::ConfigurationError))?;
        Self::new(raw.sender_address, raw.smtp_username)
    }
    pub fn save(&self, path: &Path) -> Result<()> {
        let bytes =
            Zeroizing::new(serde_json::to_vec(self).map_err(|_| MailError(E::ConfigurationError))?);
        write_private_file(path, &bytes)
    }
}
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SendMessage {
    pub idempotency_key: String,
    #[serde(default)]
    pub to: Vec<String>,
    #[serde(default)]
    pub cc: Vec<String>,
    #[serde(default)]
    pub bcc: Vec<String>,
    pub subject: String,
    pub body_text: String,
}
impl SendMessage {
    pub fn valid(&self) -> bool {
        let count = self.to.len() + self.cc.len() + self.bcc.len();
        valid_key(&self.idempotency_key)
            && (1..=50).contains(&count)
            && self
                .to
                .iter()
                .chain(&self.cc)
                .chain(&self.bcc)
                .all(|s| valid_address(s))
            && self.subject.len() <= 998
            && !self.subject.chars().any(char::is_control)
            && self.body_text.len() <= BODY_LIMIT
            && !self.body_text.contains('\0')
    }
}
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetSendStatus {
    pub idempotency_key: String,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SendOutcome {
    Accepted,
    NotAccepted,
    OutcomeUnknown,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SendReason {
    InProgress,
    Interrupted,
    SmtpRejected,
    AuthenticationFailed,
    TlsRequired,
    TlsError,
    ConnectionFailed,
    ConnectionLost,
    ProtocolError,
    DeadlineExceeded,
    Cancelled,
    JournalUnavailable,
}
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
pub struct SendStatus {
    pub idempotency_key: String,
    pub message_id: String,
    pub outcome: SendOutcome,
    pub reason: Option<SendReason>,
    pub smtp_code: Option<u16>,
    pub updated_at: String,
}
pub(crate) fn build_message(
    settings: &SendSettings,
    request: &SendMessage,
    message_id: &str,
) -> Result<Message> {
    let mailbox = |s: &str| -> Result<Mailbox> {
        Ok(Mailbox::new(
            None,
            s.parse().map_err(|_| MailError(E::InvalidMessage))?,
        ))
    };
    let mut builder = Message::builder()
        .from(mailbox(&settings.sender_address)?)
        .subject(&request.subject)
        .message_id(Some(message_id.to_owned()))
        .header(ContentType::TEXT_PLAIN)
        .header(ContentTransferEncoding::Base64);
    for to in &request.to {
        builder = builder.to(mailbox(to)?);
    }
    for cc in &request.cc {
        builder = builder.cc(mailbox(cc)?);
    }
    for bcc in &request.bcc {
        builder = builder.bcc(mailbox(bcc)?);
    }
    let message = builder
        .body(request.body_text.clone())
        .map_err(|_| MailError(E::InvalidMessage))?;
    let bytes = message.formatted();
    if bytes.len() > MESSAGE_LIMIT || !bytes.is_ascii() {
        return Err(MailError(E::InvalidMessage));
    }
    Ok(message)
}
fn db_error(_: rusqlite::Error) -> MailError {
    MailError(E::JournalUnavailable)
}
fn enum_string<T: Serialize>(value: T) -> String {
    serde_json::to_value(value)
        .unwrap()
        .as_str()
        .unwrap()
        .to_owned()
}
type JournalRecord = (Vec<u8>, Vec<u8>, SendStatus);
struct Journal {
    db: Connection,
    key: hmac::Key,
    account: Vec<u8>,
}
impl Journal {
    fn open(path: &Path, settings: &SendSettings, initialize: bool) -> Result<Self> {
        let parent = path.parent().ok_or(MailError(E::ConfigurationError))?;
        if !parent.exists() {
            if !initialize {
                return Err(MailError(E::ConfigurationError));
            }
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder
                .create(parent)
                .map_err(|_| MailError(E::ConfigurationError))?;
        }
        outside_checkout(parent)?;
        if !path.exists() {
            if !initialize {
                return Err(MailError(E::ConfigurationError));
            }
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options
                    .mode(0o600)
                    .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32);
            }
            options
                .open(path)
                .map_err(|_| MailError(E::ConfigurationError))?;
        }
        let meta = fs::symlink_metadata(path).map_err(|_| MailError(E::ConfigurationError))?;
        let dir = fs::symlink_metadata(parent).map_err(|_| MailError(E::ConfigurationError))?;
        if !meta.is_file()
            || !dir.is_dir()
            || meta.len() > JOURNAL_LIMIT
            || (!initialize && meta.len() == 0)
        {
            return Err(MailError(E::ConfigurationError));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let uid = rustix::process::geteuid().as_raw();
            if meta.uid() != uid
                || dir.uid() != uid
                || meta.mode() & 0o077 != 0
                || dir.mode() & 0o077 != 0
                || meta.nlink() != 1
            {
                return Err(MailError(E::ConfigurationError));
            }
        }
        let db = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .map_err(db_error)?;
        db.busy_timeout(Duration::from_millis(500))
            .map_err(db_error)?;
        db.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL; PRAGMA locking_mode=EXCLUSIVE; PRAGMA trusted_schema=OFF;").map_err(db_error)?;
        let page_size: i64 = db
            .query_row("PRAGMA page_size", [], |r| r.get(0))
            .map_err(db_error)?;
        db.pragma_update(None, "max_page_count", JOURNAL_LIMIT as i64 / page_size)
            .map_err(db_error)?;
        let version: i64 = db
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .map_err(db_error)?;
        if version != 1 && !(version == 0 && initialize) {
            return Err(MailError(E::ConfigurationError));
        }
        db.execute_batch("BEGIN EXCLUSIVE").map_err(db_error)?;
        if version == 0 {
            db.execute_batch("CREATE TABLE metadata (name TEXT PRIMARY KEY, value BLOB NOT NULL); CREATE TABLE submissions (key TEXT PRIMARY KEY, account BLOB NOT NULL, fingerprint BLOB NOT NULL, message_id TEXT NOT NULL, outcome TEXT NOT NULL, reason TEXT, code INTEGER, updated_at TEXT NOT NULL);").map_err(db_error)?;
            let mut generated = Zeroizing::new(vec![0u8; 32]);
            SystemRandom::new()
                .fill(&mut generated)
                .map_err(|_| MailError(E::InternalError))?;
            db.execute(
                "INSERT INTO metadata (name,value) VALUES ('hmac_key',?1)",
                [&generated[..]],
            )
            .map_err(db_error)?;
            db.pragma_update(None, "user_version", 1)
                .map_err(db_error)?;
        }
        let secret = Zeroizing::new(
            db.query_row(
                "SELECT value FROM metadata WHERE name='hmac_key'",
                [],
                |r| r.get::<_, Vec<u8>>(0),
            )
            .map_err(db_error)?,
        );
        if secret.len() != 32 {
            return Err(MailError(E::ConfigurationError));
        }
        db.execute("UPDATE submissions SET reason='interrupted',updated_at=?1 WHERE outcome='outcome_unknown' AND reason='in_progress'",[now()]).map_err(db_error)?;
        db.execute_batch("COMMIT").map_err(db_error)?;
        let key = hmac::Key::new(hmac::HMAC_SHA256, &secret);
        let scope =
            serde_json::to_vec(&("smtp-v1", &settings.sender_address, &settings.smtp_username))
                .map_err(|_| MailError(E::InternalError))?;
        let account = hmac::sign(&key, &scope).as_ref().to_vec();
        Ok(Self { db, key, account })
    }
    fn fingerprint(&self, request: &SendMessage) -> Result<Vec<u8>> {
        let data = serde_json::to_vec(request).map_err(|_| MailError(E::InternalError))?;
        let mut context = hmac::Context::with_key(&self.key);
        context.update(&self.account);
        context.update(&data);
        Ok(context.sign().as_ref().to_vec())
    }
    fn lookup(&self, key: &str) -> Result<Option<JournalRecord>> {
        let row = self.db.query_row("SELECT account,fingerprint,message_id,outcome,reason,code,updated_at FROM submissions WHERE key=?1", [key], |r| {
            Ok((r.get::<_,Vec<u8>>(0)?,r.get::<_,Vec<u8>>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,Option<String>>(4)?,r.get::<_,Option<u16>>(5)?,r.get::<_,String>(6)?))
        }).optional().map_err(db_error)?;
        row.map(|(account,fingerprint,message_id,outcome,reason,smtp_code,updated_at)| {
            let result = serde_json::from_value(serde_json::json!({"idempotency_key":key,"message_id":message_id,"outcome":outcome,"reason":reason,"smtp_code":smtp_code,"updated_at":updated_at})).map_err(|_| MailError(E::JournalUnavailable))?;
            Ok((account,fingerprint,result))
        }).transpose()
    }
    fn status(&self, key: &str) -> Result<SendStatus> {
        let (scope, _, result) = self.lookup(key)?.ok_or(MailError(E::SubmissionNotFound))?;
        if scope != self.account {
            return Err(MailError(E::SubmissionNotFound));
        }
        Ok(result)
    }
    fn claim(&self, fingerprint: &[u8], result: &SendStatus) -> Result<()> {
        let count: i64 = self
            .db
            .query_row("SELECT COUNT(*) FROM submissions", [], |r| r.get(0))
            .map_err(db_error)?;
        if count >= RECORD_LIMIT {
            return Err(MailError(E::JournalFull));
        }
        self.db.execute("INSERT INTO submissions (key,account,fingerprint,message_id,outcome,reason,code,updated_at) VALUES (?1,?2,?3,?4,?5,?6,NULL,?7)", params![result.idempotency_key,self.account,fingerprint,result.message_id,enum_string(result.outcome),result.reason.map(enum_string),result.updated_at]).map_err(db_error)?;
        Ok(())
    }
    fn finish(&self, result: &SendStatus) -> Result<()> {
        let changed = self.db.execute("UPDATE submissions SET outcome=?1,reason=?2,code=?3,updated_at=?4 WHERE key=?5 AND account=?6", params![enum_string(result.outcome),result.reason.map(enum_string),result.smtp_code,result.updated_at,result.idempotency_key,self.account]).map_err(db_error)?;
        if changed != 1 {
            return Err(MailError(E::JournalUnavailable));
        }
        Ok(())
    }
}
enum Task {
    Send(SendMessage),
    Status(String),
}
struct Job {
    task: Task,
    deadline: Instant,
    cancel: CancellationToken,
    reply: oneshot::Sender<Result<SendStatus>>,
}
#[derive(Clone)]
pub struct SubmissionService {
    tx: mpsc::Sender<Job>,
}
impl SubmissionService {
    /// Explicit local setup only. Never called automatically by serve or MCP.
    pub fn initialize_journal(settings: &SendSettings, path: &Path) -> Result<()> {
        let checked = SendSettings::new(
            settings.sender_address.clone(),
            settings.smtp_username.clone(),
        )?;
        drop(Journal::open(path, &checked, true)?);
        Ok(())
    }
    pub async fn start(
        credentials: Credentials,
        settings: SendSettings,
        journal: PathBuf,
        shutdown: CancellationToken,
    ) -> Result<Self> {
        Self::with_connector(
            credentials,
            settings,
            journal,
            shutdown,
            smtp::Connector::ICloud,
        )
        .await
    }
    pub(crate) async fn with_connector(
        credentials: Credentials,
        settings: SendSettings,
        path: PathBuf,
        shutdown: CancellationToken,
        connector: smtp::Connector,
    ) -> Result<Self> {
        let settings = SendSettings::new(settings.sender_address, settings.smtp_username)?;
        let (tx, rx) = mpsc::channel(16);
        let (ready, started) = oneshot::channel::<Result<()>>();
        std::thread::Builder::new()
            .name("mail-submission".into())
            .spawn(move || {
                let setup = (|| {
                    let initialize = match &connector {
                        smtp::Connector::ICloud => false,
                        #[cfg(test)]
                        _ => true,
                    };
                    let journal = Journal::open(&path, &settings, initialize)?;
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(|_| MailError(E::InternalError))?;
                    Ok((journal, runtime))
                })();
                match setup {
                    Ok((journal, runtime)) => {
                        let _ = ready.send(Ok(()));
                        runtime.block_on(run(
                            rx,
                            journal,
                            credentials,
                            settings,
                            shutdown,
                            connector,
                        ));
                    }
                    Err(error) => {
                        let _ = ready.send(Err(error));
                    }
                }
            })
            .map_err(|_| MailError(E::InternalError))?;
        started.await.map_err(|_| MailError(E::InternalError))??;
        Ok(Self { tx })
    }
    async fn call(
        &self,
        task: Task,
        deadline: Instant,
        cancel: CancellationToken,
    ) -> Result<SendStatus> {
        let (reply, rx) = oneshot::channel();
        let cancel = cancel.child_token();
        struct Cancel(CancellationToken);
        impl Drop for Cancel {
            fn drop(&mut self) {
                self.0.cancel();
            }
        }
        let _guard = Cancel(cancel.clone());
        if deadline <= Instant::now() {
            return Err(MailError(E::DeadlineExceeded));
        }
        self.tx
            .try_send(Job {
                task,
                deadline,
                cancel: cancel.clone(),
                reply,
            })
            .map_err(|e| {
                MailError(match e {
                    mpsc::error::TrySendError::Full(_) => E::Busy,
                    _ => E::JournalUnavailable,
                })
            })?;
        tokio::select! { _=cancel.cancelled()=>Err(MailError(E::Cancelled)), result=tokio::time::timeout_at(deadline,rx)=>result.map_err(|_|MailError(E::DeadlineExceeded))?.map_err(|_|MailError(E::JournalUnavailable))? }
    }
    pub async fn send(
        &self,
        request: SendMessage,
        deadline: Instant,
        cancel: CancellationToken,
    ) -> Result<SendStatus> {
        if !request.valid() {
            return Err(MailError(E::InvalidMessage));
        }
        self.call(Task::Send(request), deadline, cancel).await
    }
    pub async fn status(
        &self,
        key: String,
        deadline: Instant,
        cancel: CancellationToken,
    ) -> Result<SendStatus> {
        if !valid_key(&key) {
            return Err(MailError(E::InvalidMessage));
        }
        self.call(Task::Status(key), deadline, cancel).await
    }
}
async fn run(
    mut rx: mpsc::Receiver<Job>,
    journal: Journal,
    credentials: Credentials,
    settings: SendSettings,
    shutdown: CancellationToken,
    connector: smtp::Connector,
) {
    let mut auth_failed = false;
    loop {
        let job = tokio::select! { _=shutdown.cancelled()=>break, job=rx.recv()=>match job {Some(j)=>j,None=>break} };
        if job.reply.is_closed() || job.cancel.is_cancelled() || job.deadline <= Instant::now() {
            continue;
        }
        let start = Instant::now();
        let operation_id = uuid::Uuid::new_v4();
        let tool = match &job.task {
            Task::Send(_) => "send_message",
            Task::Status(_) => "get_send_status",
        };
        let result = match job.task {
            Task::Status(key) => journal.status(&key),
            Task::Send(request) => {
                submit(
                    &journal,
                    &credentials,
                    &settings,
                    &request,
                    job.deadline,
                    &job.cancel,
                    &shutdown,
                    &connector,
                    &mut auth_failed,
                )
                .await
            }
        };
        tracing::info!(target:"icloud_mail_mcp::ops",%operation_id,tool,success=result.is_ok(),outcome=?result.as_ref().ok().map(|r|r.outcome),error_code=?result.as_ref().err().map(|e|e.0),elapsed_ms=start.elapsed().as_millis() as u64,"Submission operation completed");
        let _ = job.reply.send(result);
    }
}
#[allow(clippy::too_many_arguments)]
async fn submit(
    journal: &Journal,
    credentials: &Credentials,
    settings: &SendSettings,
    request: &SendMessage,
    deadline: Instant,
    cancel: &CancellationToken,
    shutdown: &CancellationToken,
    connector: &smtp::Connector,
    auth_failed: &mut bool,
) -> Result<SendStatus> {
    let fingerprint = journal.fingerprint(request)?;
    if let Some((account, prior, result)) = journal.lookup(&request.idempotency_key)? {
        if account != journal.account || prior != fingerprint {
            return Err(MailError(E::IdempotencyConflict));
        }
        return Ok(result);
    }
    if *auth_failed {
        return Err(MailError(E::AuthenticationFailed));
    }
    let domain = settings
        .sender_address
        .rsplit_once('@')
        .ok_or(MailError(E::ConfigurationError))?
        .1;
    let message_id = format!("<{}@{}>", uuid::Uuid::new_v4(), domain);
    let message = build_message(settings, request, &message_id)?;
    if cancel.is_cancelled() || deadline <= Instant::now() {
        return Err(MailError(E::Cancelled));
    }
    let mut result = SendStatus {
        idempotency_key: request.idempotency_key.clone(),
        message_id,
        outcome: SendOutcome::OutcomeUnknown,
        reason: Some(SendReason::InProgress),
        smtp_code: None,
        updated_at: now(),
    };
    journal.claim(&fingerprint, &result)?;
    if cancel.is_cancelled() || shutdown.is_cancelled() || deadline <= Instant::now() {
        result.outcome = SendOutcome::NotAccepted;
        result.reason = Some(SendReason::Cancelled);
    } else {
        // Once an attempt starts, caller cancellation does not create a second send.
        let bound = deadline.min(Instant::now() + Duration::from_secs(20));
        let wire_result =
            smtp::submit(connector, credentials, settings, &message, bound, shutdown).await;
        result.outcome = wire_result.outcome;
        result.reason = wire_result.reason;
        result.smtp_code = wire_result.code;
        if result.reason == Some(SendReason::AuthenticationFailed) {
            *auth_failed = true;
        }
    }
    result.updated_at = now();
    if journal.finish(&result).is_err() {
        result.outcome = SendOutcome::OutcomeUnknown;
        result.reason = Some(SendReason::JournalUnavailable);
        result.smtp_code = None;
    }
    Ok(result)
}
#[cfg(test)]
mod tests;
