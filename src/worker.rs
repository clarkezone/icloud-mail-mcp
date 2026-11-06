use crate::{
    credentials::Credentials,
    error::{ErrorCode as E, MailError, Result},
    mail,
    model::*,
    state::{Cache, Entry, Snapshot, Token, Tokens},
    wire::{Connector, Wire},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::{Semaphore, mpsc, oneshot},
    time::{Instant, timeout_at},
};
use tokio_util::sync::CancellationToken;

struct Job {
    op: Operation,
    deadline: Instant,
    cancel: CancellationToken,
    reply: oneshot::Sender<Result<serde_json::Value>>,
}
#[derive(Clone)]
pub struct Mailbox {
    tx: mpsc::Sender<Job>,
}
struct CancelOnDrop(CancellationToken);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}
impl Mailbox {
    pub fn start(credentials: Credentials, shutdown: CancellationToken) -> Self {
        Self::with_connector(credentials, shutdown, Connector::ICloud)
    }
    /// Explicitly tolerate iCloud's tagged READ-WRITE EXAMINE response.
    /// Selection still uses EXAMINE; command and PEEK restrictions are unchanged.
    pub fn start_with_icloud_examine_compatibility(
        credentials: Credentials,
        shutdown: CancellationToken,
    ) -> Self {
        Self::with_connector(credentials, shutdown, Connector::ICloudExamineCompatibility)
    }
    pub(crate) fn with_connector(
        credentials: Credentials,
        shutdown: CancellationToken,
        connector: Connector,
    ) -> Self {
        let (tx, rx) = mpsc::channel(16);
        tokio::spawn(run(rx, credentials, shutdown, connector));
        Self { tx }
    }
    pub async fn call(
        &self,
        op: Operation,
        deadline: Instant,
        cancel: CancellationToken,
    ) -> Result<serde_json::Value> {
        let (reply, rx) = oneshot::channel();
        let cancel = cancel.child_token();
        let _guard = CancelOnDrop(cancel.clone());
        if deadline <= Instant::now() {
            return Err(MailError(E::DeadlineExceeded));
        }
        self.tx
            .try_send(Job {
                op,
                deadline,
                cancel: cancel.clone(),
                reply,
            })
            .map_err(|e| {
                MailError(match e {
                    mpsc::error::TrySendError::Full(_) => E::Busy,
                    mpsc::error::TrySendError::Closed(_) => E::UpstreamUnavailable,
                })
            })?;
        tokio::select! {
            _=cancel.cancelled()=>Err(MailError(E::Cancelled)),
            result=timeout_at(deadline,rx)=>result.map_err(|_|MailError(E::DeadlineExceeded))?.map_err(|_|MailError(E::UpstreamUnavailable))?,
        }
    }
}
struct SessionState {
    tokens: Tokens,
    cache: Cache,
    decoder: Arc<Semaphore>,
}
impl SessionState {
    fn cursor(&self, entry: &Entry, index: usize) -> Result<String> {
        self.tokens.seal(&Token::Cursor {
            snapshot: entry.id.clone(),
            index,
        })
    }
    fn resume(&mut self, cursor: &str) -> Result<(Arc<Entry>, usize)> {
        let Token::Cursor { snapshot, index } = self.tokens.open(cursor, true)? else {
            return Err(MailError(E::InvalidCursor));
        };
        Ok((self.cache.get(&snapshot)?, index))
    }
    fn folder(&self, handle: &str) -> Result<String> {
        match self.tokens.open(handle, false)? {
            Token::Folder(name) => Ok(name),
            _ => Err(MailError(E::StaleReference)),
        }
    }
    async fn perform(
        &mut self,
        wire: &mut Wire,
        op: Operation,
        cancel: CancellationToken,
    ) -> Result<serde_json::Value> {
        match op {
            Operation::Folders(p) => self.folders(wire, p).await,
            Operation::List(p) => {
                self.messages(
                    wire,
                    p.folder_handle,
                    Filters::default(),
                    p.page_size,
                    p.cursor,
                )
                .await
            }
            Operation::Search(p) => {
                self.messages(wire, p.folder_handle, p.filters, p.page_size, p.cursor)
                    .await
            }
            Operation::Get(p) => self.get(wire, p, cancel).await,
        }
    }
    async fn folders(&mut self, wire: &mut Wire, p: ListFolders) -> Result<serde_json::Value> {
        let (entry, mut index) = if let Some(cursor) = &p.cursor {
            self.resume(cursor)?
        } else {
            self.cache.reserve(12 * 1024 * 1024)?;
            let folders = wire
                .folders()
                .await?
                .into_iter()
                .filter(|f| f.name.starts_with(&p.prefix))
                .collect();
            (
                self.cache.insert(Snapshot::Folders {
                    prefix: p.prefix.clone(),
                    counts: p.include_counts,
                    folders,
                })?,
                0,
            )
        };
        let Snapshot::Folders {
            prefix,
            counts,
            folders,
        } = &entry.snapshot
        else {
            return Err(MailError(E::InvalidCursor));
        };
        if *prefix != p.prefix || *counts != p.include_counts || index > folders.len() {
            return Err(MailError(E::InvalidCursor));
        }
        let mut page = FolderPage {
            folders: Vec::new(),
            snapshot_created_at: entry.created.clone(),
            next_cursor: None,
        };
        while index < folders.len() && page.folders.len() < p.page_size {
            let f = &folders[index];
            let (message_count, unread_count) = if *counts && f.selectable {
                wire.counts(&f.name).await?
            } else {
                (None, None)
            };
            let folder = Folder {
                folder_handle: self.tokens.seal(&Token::Folder(f.name.clone()))?,
                name: f.name.clone(),
                delimiter: f.delimiter.clone(),
                selectable: f.selectable,
                roles: f.roles.clone(),
                message_count,
                unread_count,
                counts_observed_at: if *counts && f.selectable {
                    Some(now())
                } else {
                    None
                },
            };
            page.folders.push(folder);
            page.next_cursor = Some(self.cursor(&entry, index + 1)?);
            if !fits(&page)? {
                page.folders.pop();
                if page.folders.is_empty() {
                    return Err(MailError(E::SizeLimit));
                }
                break;
            }
            index += 1;
        }
        page.next_cursor = if index < folders.len() {
            Some(self.cursor(&entry, index)?)
        } else {
            None
        };
        value(&page)
    }
    async fn messages(
        &mut self,
        wire: &mut Wire,
        handle: String,
        filters: Filters,
        size: usize,
        cursor: Option<String>,
    ) -> Result<serde_json::Value> {
        let folder = self.folder(&handle)?;
        let (validity, next) = wire.examine(&folder).await?;
        let (entry, mut index) = if let Some(cursor) = cursor {
            self.resume(&cursor)?
        } else {
            self.cache.reserve(1024 * 1024)?;
            let uids = wire.search(next - 1, &filters).await?;
            (
                self.cache.insert(Snapshot::Messages {
                    folder: folder.clone(),
                    validity,
                    filters: filters.clone(),
                    uids,
                })?,
                0,
            )
        };
        let Snapshot::Messages {
            folder: original,
            validity: original_validity,
            filters: original_filters,
            uids,
        } = &entry.snapshot
        else {
            return Err(MailError(E::InvalidCursor));
        };
        if *original != folder || *original_filters != filters || index > uids.len() {
            return Err(MailError(E::InvalidCursor));
        }
        if *original_validity != validity {
            return Err(MailError(E::StaleReference));
        }
        let mut page = MessagePage {
            messages: Vec::new(),
            snapshot_created_at: entry.created.clone(),
            skipped_unavailable: 0,
            next_cursor: None,
            consistency: "uid_membership_snapshot_values_live".into(),
        };
        let mut examined = 0;
        while index < uids.len() && page.messages.len() < size && examined < 1000 {
            examined += 1;
            let uid = uids[index];
            let Some(m) = wire.metadata(uid, false).await? else {
                index += 1;
                page.skipped_unavailable += 1;
                continue;
            };
            let item = Summary {
                message_handle: self.tokens.seal(&Token::Message {
                    folder: folder.clone(),
                    validity,
                    uid,
                })?,
                headers: m.headers,
                received_at: m.received,
                size_bytes: m.size,
                flags: m.flags,
                has_attachments: None,
                truncated_fields: m.truncated,
            };
            page.messages.push(item);
            page.next_cursor = Some(self.cursor(&entry, index + 1)?);
            if !fits(&page)? {
                page.messages.pop();
                if page.messages.is_empty() {
                    return Err(MailError(E::SizeLimit));
                }
                break;
            }
            index += 1;
        }
        page.next_cursor = if index < uids.len() {
            Some(self.cursor(&entry, index)?)
        } else {
            None
        };
        value(&page)
    }
    async fn get(
        &mut self,
        wire: &mut Wire,
        p: GetMessage,
        cancel: CancellationToken,
    ) -> Result<serde_json::Value> {
        let Token::Message {
            folder,
            validity,
            uid,
        } = self.tokens.open(&p.message_handle, false)?
        else {
            return Err(MailError(E::StaleReference));
        };
        if wire.examine(&folder).await?.0 != validity {
            return Err(MailError(E::StaleReference));
        }
        let metadata = wire
            .metadata(uid, true)
            .await?
            .ok_or(MailError(E::MessageNotFound))?;
        let structure = metadata
            .structure
            .as_ref()
            .ok_or(MailError(E::UpstreamUnavailable))?;
        let (part, mut attachments) = mail::select_parts(structure)?;
        let (entry, offset) = if let Some(cursor) = &p.body_cursor {
            self.resume(cursor)?
        } else {
            // Reserve before upstream reads, decoding, and allocating immutable text.
            self.cache.reserve(TEXT_LIMIT + 65536)?;
            let (section, text, representation, warnings) = if let Some(part) = part {
                let permit = self
                    .decoder
                    .clone()
                    .try_acquire_owned()
                    .map_err(|_| MailError(E::Busy))?;
                let bytes = wire.body_bytes(uid, &part).await?;
                let section = part.section.clone();
                let representation = if part.html {
                    "html_to_text"
                } else {
                    "plain_text"
                };
                let decoded = tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    mail::decode(&part, bytes)
                })
                .await
                .map_err(|_| MailError(E::InternalError))??;
                if cancel.is_cancelled() {
                    return Err(MailError(E::Cancelled));
                }
                (section, decoded.0, representation.into(), decoded.1)
            } else {
                (String::new(), String::new(), "none".into(), Vec::new())
            };
            (
                self.cache.insert(Snapshot::Body {
                    folder: folder.clone(),
                    validity,
                    uid,
                    section,
                    text,
                    representation,
                    warnings,
                })?,
                0,
            )
        };
        let Snapshot::Body {
            folder: original,
            validity: original_validity,
            uid: original_uid,
            text,
            representation,
            warnings,
            ..
        } = &entry.snapshot
        else {
            return Err(MailError(E::InvalidCursor));
        };
        if *original != folder
            || *original_validity != validity
            || *original_uid != uid
            || offset > text.len()
            || !text.is_char_boundary(offset)
        {
            return Err(MailError(E::InvalidCursor));
        }
        let mut result = Message {
            message_handle: p.message_handle,
            headers: metadata.headers,
            flags: metadata.flags,
            attachments: Vec::new(),
            metadata_truncated: !metadata.truncated.is_empty(),
            truncated_fields: metadata.truncated,
            warnings: warnings.clone(),
            body: Body {
                text: String::new(),
                representation: representation.clone(),
                next_cursor: None,
                snapshot_created_at: entry.created.clone(),
                complete: false,
            },
        };
        for a in attachments.drain(..) {
            result.attachments.push(a);
            if serde_json::to_vec(&result)
                .map_err(|_| MailError(E::InternalError))?
                .len()
                > 32 * 1024
            {
                result.attachments.pop();
                result.metadata_truncated = true;
                result.truncated_fields.push("attachments".into());
                break;
            }
        }
        if serde_json::to_vec(&result.headers)
            .map_err(|_| MailError(E::InternalError))?
            .len()
            > 24 * 1024
        {
            return Err(MailError(E::SizeLimit));
        }
        let mut end = (offset + p.body_chunk_bytes).min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        loop {
            result.body.text = text[offset..end].into();
            result.body.complete = end == text.len();
            result.body.next_cursor = if result.body.complete {
                None
            } else {
                Some(self.cursor(&entry, end)?)
            };
            if fits(&result)? {
                break;
            }
            if end == offset {
                return Err(MailError(E::SizeLimit));
            }
            end = offset + (end - offset) / 2;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
        }
        value(&result)
    }
}
fn fits<T: serde::Serialize>(value: &T) -> Result<bool> {
    // Reserve envelope/summary/request-id space; handler enforces the complete JSON size too.
    Ok(serde_json::to_vec(value)
        .map_err(|_| MailError(E::InternalError))?
        .len()
        <= RESPONSE_LIMIT - 8192)
}
async fn run(
    mut rx: mpsc::Receiver<Job>,
    credentials: Credentials,
    shutdown: CancellationToken,
    connector: Connector,
) {
    let mut session: Option<Wire> = None;
    let mut auth_failed = false;
    let mut failures = 0u32;
    let used = Arc::new(AtomicUsize::new(0));
    let mut state = SessionState {
        tokens: Tokens::new(),
        cache: Cache::new(),
        decoder: Arc::new(Semaphore::new(1)),
    };
    loop {
        let job = tokio::select! {_=shutdown.cancelled()=>break,job=rx.recv()=>match job{Some(j)=>j,None=>break}};
        if job.reply.is_closed() || job.cancel.is_cancelled() || job.deadline <= Instant::now() {
            continue;
        }
        if auth_failed {
            let _ = job.reply.send(Err(MailError(E::AuthenticationFailed)));
            continue;
        }
        used.store(0, Ordering::Relaxed);
        let started = Instant::now();
        let operation_id = uuid::Uuid::new_v4();
        let tool = job.op.name();
        let work = async {
            let mut attempt = 0;
            loop {
                if session.is_none() {
                    if failures > 0 {
                        let base = (1u64 << failures.saturating_sub(1).min(5)).min(30);
                        let jitter = u64::from(uuid::Uuid::new_v4().as_bytes()[0]) * 1000 / 256;
                        tokio::time::sleep(Duration::from_millis(
                            (base * 1000 + jitter).min(30000),
                        ))
                        .await;
                    }
                    let mut wire = tokio::time::timeout(
                        Duration::from_secs(5),
                        connector.connect(used.clone()),
                    )
                    .await
                    .map_err(|_| MailError(E::DeadlineExceeded))??;
                    tokio::time::timeout(Duration::from_secs(5), wire.login(&credentials))
                        .await
                        .map_err(|_| MailError(E::DeadlineExceeded))??;
                    session = Some(wire);
                }
                let result = state
                    .perform(
                        session.as_mut().unwrap(),
                        job.op.clone(),
                        job.cancel.clone(),
                    )
                    .await;
                if matches!(result, Err(MailError(E::UpstreamUnavailable))) && attempt == 0 {
                    session = None;
                    failures = failures.saturating_add(1);
                    attempt += 1;
                    continue;
                }
                return result;
            }
        };
        let result = tokio::select! {
            _=shutdown.cancelled()=>Err(MailError(E::Cancelled)),
            _=job.cancel.cancelled()=>Err(MailError(E::Cancelled)),
            result=timeout_at(job.deadline,work)=>result.unwrap_or(Err(MailError(E::DeadlineExceeded))),
        };
        if let Err(error) = result {
            session = None;
            if error.0 == E::AuthenticationFailed {
                auth_failed = true;
            }
            if matches!(error.0, E::UpstreamUnavailable | E::DeadlineExceeded) {
                failures = failures.saturating_add(1);
            }
        } else {
            failures = 0;
        }
        tracing::info!(target:"icloud_mail_mcp::ops",%operation_id,tool,success=result.is_ok(),elapsed_ms=started.elapsed().as_millis() as u64,"mail request completed");
        let _ = job.reply.send(result);
    }
    // Dropping the socket never sends CLOSE/EXPUNGE. All state and credentials die here.
}
