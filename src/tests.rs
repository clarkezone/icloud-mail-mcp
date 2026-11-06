use crate::{
    credentials::Credentials,
    error::{ErrorCode as E, MailError},
    model::*,
    wire::Connector,
    worker::Mailbox,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    time::Instant,
};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
struct TestMail {
    text: String,
    unread: bool,
}
struct Data {
    messages: BTreeMap<u32, TestMail>,
    commands: Vec<String>,
    validity: u32,
    reject: bool,
    bad_literal: bool,
    read_write: bool,
    stall: bool,
}
struct Fixture {
    addr: std::net::SocketAddr,
    data: Arc<Mutex<Data>>,
    mailbox: Mailbox,
    shutdown: CancellationToken,
    arrived: Arc<tokio::sync::Notify>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}
impl Fixture {
    async fn new() -> Self {
        Self::with_compatibility(false).await
    }
    async fn with_compatibility(compatibility: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let data = Arc::new(Mutex::new(Data {
            messages: BTreeMap::from([
                (
                    1,
                    TestMail {
                        text: "hello 世界 🦀\n".repeat(200),
                        unread: true,
                    },
                ),
                (
                    2,
                    TestMail {
                        text: "second message".into(),
                        unread: true,
                    },
                ),
            ]),
            commands: Vec::new(),
            validity: 7,
            reject: false,
            bad_literal: false,
            read_write: false,
            stall: false,
        }));
        let shutdown = CancellationToken::new();
        let stop = shutdown.clone();
        let shared = data.clone();
        let arrived = Arc::new(tokio::sync::Notify::new());
        let signal = arrived.clone();
        tokio::spawn(async move {
            loop {
                let stream =
                    tokio::select! {_=stop.cancelled()=>break,s=listener.accept()=>s.unwrap().0};
                let data = shared.clone();
                let signal = signal.clone();
                let stop = stop.clone();
                tokio::spawn(async move {
                    let (read, mut write) = stream.into_split();
                    let mut read = BufReader::new(read);
                    if write.write_all(b"* OK synthetic server\r\n").await.is_err() {
                        return;
                    }
                    loop {
                        let mut line = String::new();
                        let n = tokio::select! {_=stop.cancelled()=>return,r=read.read_line(&mut line)=>r.unwrap_or(0)};
                        if n == 0 {
                            return;
                        }
                        // Synchronous literals are used for Unicode search arguments.
                        while line.trim_end().ends_with('}') {
                            let start = line.rfind('{').unwrap();
                            let count: usize = line[start + 1..]
                                .trim_end()
                                .trim_end_matches('}')
                                .parse()
                                .unwrap();
                            write.write_all(b"+ continue\r\n").await.unwrap();
                            let mut bytes = vec![0; count];
                            use tokio::io::AsyncReadExt;
                            read.read_exact(&mut bytes).await.unwrap();
                            line.truncate(start);
                            line.push_str(&String::from_utf8_lossy(&bytes));
                            let mut tail = String::new();
                            read.read_line(&mut tail).await.unwrap();
                            line.push_str(&tail);
                        }
                        let tag = line.split_whitespace().next().unwrap().to_string();
                        let command = line[tag.len() + 1..].trim_end().to_string();
                        let (response, stall) = {
                            let mut d = data.lock().unwrap();
                            d.commands.push(if command.starts_with("LOGIN ") {
                                "LOGIN [REDACTED]".into()
                            } else {
                                command.clone()
                            });
                            if command.starts_with("LOGIN ") {
                                (
                                    if d.reject {
                                        format!("{tag} NO rejected provider-secret-canary\r\n")
                                    } else {
                                        format!("{tag} OK login\r\n")
                                    },
                                    false,
                                )
                            } else if command == "CAPABILITY" {
                                (
                                    format!(
                                        "* CAPABILITY IMAP4rev1 UIDPLUS MOVE\r\n{tag} OK capability\r\n"
                                    ),
                                    false,
                                )
                            } else if command.starts_with("LIST ") {
                                (
                                    format!(
                                        "* LIST (\\HasNoChildren) \"/\" \"INBOX\"\r\n* LIST (\\Archive) \"/\" \"Archive\"\r\n* LIST () \"/\" \"{}\"\r\n{tag} OK list\r\n",
                                        crate::wire::encode_mailbox("旅行")
                                    ),
                                    false,
                                )
                            } else if command.starts_with("STATUS ") {
                                (
                                    format!(
                                        "* STATUS \"INBOX\" (MESSAGES {} UNSEEN {})\r\n{tag} OK status\r\n",
                                        d.messages.len(),
                                        d.messages.values().filter(|m| m.unread).count()
                                    ),
                                    false,
                                )
                            } else if command.starts_with("EXAMINE ") {
                                (
                                    format!(
                                        "* {} EXISTS\r\n* OK [UIDVALIDITY {}] identity\r\n* OK [UIDNEXT {}] next\r\n{tag} OK [{}] selected\r\n",
                                        d.messages.len(),
                                        d.validity,
                                        d.messages.keys().next_back().unwrap_or(&0) + 1,
                                        if d.read_write {
                                            "READ-WRITE"
                                        } else {
                                            "READ-ONLY"
                                        }
                                    ),
                                    false,
                                )
                            } else if command.starts_with("UID SEARCH ") {
                                let high: u32 = command
                                    .split("UID 1:")
                                    .nth(1)
                                    .unwrap()
                                    .split_whitespace()
                                    .next()
                                    .unwrap()
                                    .parse()
                                    .unwrap();
                                let ids: Vec<_> = d
                                    .messages
                                    .iter()
                                    .filter(|(uid, m)| {
                                        **uid <= high
                                            && (!command.contains(" UNSEEN") || m.unread)
                                            && (!command.contains(" SEEN") || !m.unread)
                                    })
                                    .map(|(uid, _)| uid.to_string())
                                    .collect();
                                (
                                    format!(
                                        "* SEARCH{}\r\n{tag} OK search\r\n",
                                        if ids.is_empty() {
                                            String::new()
                                        } else {
                                            format!(" {}", ids.join(" "))
                                        }
                                    ),
                                    false,
                                )
                            } else if command.starts_with("UID FETCH ") {
                                let uid: u32 =
                                    command.split_whitespace().nth(2).unwrap().parse().unwrap();
                                let stall = d.stall;
                                d.stall = false;
                                if d.bad_literal {
                                    d.bad_literal = false;
                                    (
                                        format!(
                                            "* 1 FETCH (UID {uid} BODY[HEADER] {{999999999}}\r\n"
                                        ),
                                        false,
                                    )
                                } else if let Some(m) = d.messages.get(&uid) {
                                    let encoded = STANDARD.encode(m.text.as_bytes());
                                    if command.contains("BODY.PEEK[1]<") {
                                        let range = command
                                            .split('<')
                                            .nth(1)
                                            .unwrap()
                                            .split('>')
                                            .next()
                                            .unwrap();
                                        let (offset, count) = range.split_once('.').unwrap();
                                        let offset: usize = offset.parse().unwrap();
                                        let count: usize = count.parse().unwrap();
                                        let end = (offset + count).min(encoded.len());
                                        let body = &encoded[offset..end];
                                        (
                                            format!(
                                                "* 1 FETCH (UID {uid} BODY[1]<{offset}> {{{}}}\r\n{body})\r\n{tag} OK body\r\n",
                                                body.len()
                                            ),
                                            stall,
                                        )
                                    } else {
                                        let header = format!(
                                            "From: Synthetic <sender@example.com>\r\nTo: reader@example.com\r\nSubject: message-{uid}\r\nDate: Thu, 05 Nov 2026 01:00:00 +0000\r\n\r\n"
                                        );
                                        let structure = if command.contains("BODYSTRUCTURE") {
                                            format!(
                                                " BODYSTRUCTURE ((\"TEXT\" \"PLAIN\" (\"CHARSET\" \"UTF-8\") NIL NIL \"BASE64\" {} 1 NIL NIL NIL NIL)(\"APPLICATION\" \"OCTET-STREAM\" (\"NAME\" \"large.bin\") NIL NIL \"BASE64\" 9000000 NIL (\"ATTACHMENT\" (\"FILENAME\" \"large.bin\")) NIL NIL) \"MIXED\")",
                                                encoded.len()
                                            )
                                        } else {
                                            String::new()
                                        };
                                        (
                                            format!(
                                                "* 1 FETCH (UID {uid} FLAGS ({}) INTERNALDATE \"05-Nov-2026 01:00:00 +0000\" RFC822.SIZE 9001000{structure} BODY[HEADER.FIELDS (FROM TO CC SUBJECT DATE)] {{{}}}\r\n{header})\r\n{tag} OK metadata\r\n",
                                                if m.unread { "" } else { "\\Seen" },
                                                header.len()
                                            ),
                                            stall,
                                        )
                                    }
                                } else {
                                    (format!("{tag} OK missing\r\n"), false)
                                }
                            } else {
                                (format!("{tag} BAD unsupported\r\n"), false)
                            }
                        };
                        if stall {
                            signal.notify_one();
                            let mut byte = [0];
                            use tokio::io::AsyncReadExt;
                            let _ = tokio::select! {_=stop.cancelled()=>return,r=read.read(&mut byte)=>r};
                            return;
                        }
                        if write.write_all(response.as_bytes()).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        let credentials =
            Credentials::new("reader@example.com".into(), "password-canary".into()).unwrap();
        let connector = if compatibility {
            Connector::FixtureExamineCompatibility(addr)
        } else {
            Connector::Fixture(addr)
        };
        let mailbox = Mailbox::with_connector(credentials, shutdown.clone(), connector);
        Self {
            addr,
            data,
            mailbox,
            shutdown,
            arrived,
        }
    }
    async fn call(&self, op: Operation) -> crate::error::Result<serde_json::Value> {
        self.mailbox
            .call(
                op,
                Instant::now() + Duration::from_secs(10),
                CancellationToken::new(),
            )
            .await
    }
    async fn folder(&self) -> String {
        let result = self
            .call(Operation::Folders(ListFolders {
                prefix: "INBOX".into(),
                include_counts: false,
                page_size: 50,
                cursor: None,
            }))
            .await
            .unwrap();
        result["folders"][0]["folder_handle"]
            .as_str()
            .unwrap()
            .into()
    }
    async fn messages(&self) -> serde_json::Value {
        let folder = self.folder().await;
        self.call(Operation::List(ListMessages {
            folder_handle: folder,
            page_size: 50,
            cursor: None,
        }))
        .await
        .unwrap()
    }
}

#[tokio::test]
async fn read_only_round_trip_and_no_attachment_download() {
    let fixture = Fixture::new().await;
    let page = fixture.messages().await;
    assert_eq!(page["messages"].as_array().unwrap().len(), 2);
    let handle = page["messages"][1]["message_handle"]
        .as_str()
        .unwrap()
        .to_string();
    let mut cursor = None;
    let mut whole = String::new();
    loop {
        let result = fixture
            .call(Operation::Get(GetMessage {
                message_handle: handle.clone(),
                body_chunk_bytes: if cursor.is_none() { 256 } else { 513 },
                body_cursor: cursor,
            }))
            .await
            .unwrap();
        whole.push_str(result["body"]["text"].as_str().unwrap());
        assert_eq!(result["attachments"][0]["filename"], "large.bin");
        cursor = result["body"]["next_cursor"].as_str().map(str::to_string);
        if cursor.is_none() {
            assert_eq!(result["body"]["complete"], true);
            break;
        }
    }
    assert_eq!(whole, fixture.data.lock().unwrap().messages[&1].text);
    let data = fixture.data.lock().unwrap();
    assert!(data.messages.values().all(|m| m.unread));
    for c in &data.commands {
        assert!(
            !c.starts_with("SELECT")
                && !c.contains("BODY.PEEK[]")
                && !c.contains("BODY.PEEK[2]")
                && !c.contains("BODY.PEEK[TEXT]")
        );
        assert!(
            c.starts_with("LOGIN ")
                || c == "CAPABILITY"
                || c.starts_with("LIST ")
                || c.starts_with("EXAMINE ")
                || c.starts_with("UID SEARCH ")
                || c.starts_with("UID FETCH ")
        );
    }
    assert_eq!(
        data.commands
            .iter()
            .filter(|c| c.contains("BODY.PEEK[1]<"))
            .count(),
        1,
        "decoded body is fetched once, not per chunk"
    );
}

#[tokio::test]
async fn explicit_examine_compatibility_keeps_reads_non_marking_and_blocks_writes() {
    let f = Fixture::with_compatibility(true).await;
    f.data.lock().unwrap().read_write = true;
    let page = f.messages().await;
    let handle = page["messages"][1]["message_handle"]
        .as_str()
        .unwrap()
        .to_owned();
    let result = f
        .call(Operation::Get(GetMessage {
            message_handle: handle,
            body_chunk_bytes: 256,
            body_cursor: None,
        }))
        .await
        .unwrap();
    assert!(!result["body"]["text"].as_str().unwrap().is_empty());
    {
        let data = f.data.lock().unwrap();
        assert!(data.messages.values().all(|m| m.unread));
        assert!(data.commands.iter().any(|c| c.contains("BODY.PEEK[1]<")));
        assert!(
            data.commands
                .iter()
                .all(|c| !c.starts_with("SELECT") && !c.contains("BODY.PEEK[2]"))
        );
    }
    let mut wire = Connector::FixtureExamineCompatibility(f.addr)
        .connect(Arc::new(std::sync::atomic::AtomicUsize::new(0)))
        .await
        .unwrap();
    for forbidden in [
        "SELECT",
        "STORE",
        "UID STORE",
        "APPEND",
        "UID MOVE",
        "UID COPY",
        "EXPUNGE",
        "CLOSE",
    ] {
        assert!(matches!(
            wire.command(vec![crate::wire::raw(forbidden)], E::UpstreamUnavailable)
                .await,
            Err(MailError(E::InternalError))
        ));
    }
    f.data.lock().unwrap().validity = 0;
    assert_eq!(
        f.call(Operation::List(ListMessages {
            folder_handle: f.folder().await,
            page_size: 1,
            cursor: None,
        }))
        .await,
        Err(MailError(E::UpstreamUnavailable))
    );
}

#[tokio::test]
async fn search_membership_is_fixed_while_flags_are_live() {
    let f = Fixture::new().await;
    let folder = f.folder().await;
    let filters = Filters {
        unread: Some(true),
        ..Default::default()
    };
    let first = f
        .call(Operation::Search(SearchMessages {
            folder_handle: folder.clone(),
            filters: filters.clone(),
            page_size: 1,
            cursor: None,
        }))
        .await
        .unwrap();
    assert_eq!(first["messages"][0]["subject"], "message-2");
    {
        let mut d = f.data.lock().unwrap();
        d.messages.get_mut(&1).unwrap().unread = false;
        d.messages.insert(
            3,
            TestMail {
                text: "arrival".into(),
                unread: true,
            },
        );
    }
    let cursor = first["next_cursor"].as_str().unwrap().to_string();
    let second = f
        .call(Operation::Search(SearchMessages {
            folder_handle: folder.clone(),
            filters: filters.clone(),
            page_size: 50,
            cursor: Some(cursor.clone()),
        }))
        .await
        .unwrap();
    assert_eq!(second["messages"][0]["subject"], "message-1");
    assert_eq!(second["messages"][0]["flags"][0], "\\Seen");
    assert!(second["next_cursor"].is_null());
    let changed = f
        .call(Operation::Search(SearchMessages {
            folder_handle: folder,
            filters: Filters::default(),
            page_size: 50,
            cursor: Some(cursor),
        }))
        .await;
    assert_eq!(changed, Err(MailError(E::InvalidCursor)));
}

#[tokio::test]
async fn stale_handles_and_huge_literals_fail_closed() {
    let f = Fixture::new().await;
    let page = f.messages().await;
    let handle = page["messages"][0]["message_handle"]
        .as_str()
        .unwrap()
        .to_string();
    f.data.lock().unwrap().validity = 8;
    assert_eq!(
        f.call(Operation::Get(GetMessage {
            message_handle: handle.clone(),
            body_chunk_bytes: 256,
            body_cursor: None
        }))
        .await,
        Err(MailError(E::StaleReference))
    );
    f.data.lock().unwrap().validity = 7;
    f.data.lock().unwrap().bad_literal = true;
    assert_eq!(
        f.call(Operation::Get(GetMessage {
            message_handle: handle,
            body_chunk_bytes: 256,
            body_cursor: None
        }))
        .await,
        Err(MailError(E::SizeLimit))
    );
    f.data.lock().unwrap().read_write = true;
    assert_eq!(
        f.call(Operation::List(ListMessages {
            folder_handle: f.folder().await,
            page_size: 50,
            cursor: None
        }))
        .await,
        Err(MailError(E::UpstreamUnavailable))
    );
}

#[tokio::test]
async fn rejected_credentials_open_a_cross_request_breaker() {
    let f = Fixture::new().await;
    f.data.lock().unwrap().reject = true;
    for _ in 0..3 {
        assert_eq!(
            f.call(Operation::Folders(ListFolders {
                prefix: String::new(),
                include_counts: false,
                page_size: 1,
                cursor: None
            }))
            .await,
            Err(MailError(E::AuthenticationFailed))
        );
    }
    assert_eq!(
        f.data
            .lock()
            .unwrap()
            .commands
            .iter()
            .filter(|c| c.starts_with("LOGIN "))
            .count(),
        1
    );
}

#[tokio::test]
async fn queued_cancellation_never_sends_a_stale_command() {
    let f = Fixture::new().await;
    let folder = f.folder().await;
    f.data.lock().unwrap().stall = true;
    let mb = f.mailbox.clone();
    let folder1 = folder.clone();
    let first = tokio::spawn(async move {
        mb.call(
            Operation::List(ListMessages {
                folder_handle: folder1,
                page_size: 1,
                cursor: None,
            }),
            Instant::now() + Duration::from_millis(300),
            CancellationToken::new(),
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(2), f.arrived.notified())
        .await
        .unwrap();
    let cancel = CancellationToken::new();
    let token = cancel.clone();
    let mb = f.mailbox.clone();
    let second = tokio::spawn(async move {
        mb.call(
            Operation::Folders(ListFolders {
                prefix: "never-issued".into(),
                include_counts: false,
                page_size: 1,
                cursor: None,
            }),
            Instant::now() + Duration::from_secs(3),
            token,
        )
        .await
    });
    tokio::task::yield_now().await;
    cancel.cancel();
    assert_eq!(second.await.unwrap(), Err(MailError(E::Cancelled)));
    assert_eq!(first.await.unwrap(), Err(MailError(E::DeadlineExceeded)));
    let before = f
        .data
        .lock()
        .unwrap()
        .commands
        .iter()
        .filter(|c| c.starts_with("LIST "))
        .count();
    f.call(Operation::List(ListMessages {
        folder_handle: folder,
        page_size: 1,
        cursor: None,
    }))
    .await
    .unwrap();
    assert_eq!(
        before,
        f.data
            .lock()
            .unwrap()
            .commands
            .iter()
            .filter(|c| c.starts_with("LIST "))
            .count()
    );
}

#[tokio::test]
async fn full_queue_rejects_admission_without_more_imap_work() {
    let f = Fixture::new().await;
    let folder = f.folder().await;
    f.data.lock().unwrap().stall = true;
    let mailbox = f.mailbox.clone();
    let active = tokio::spawn(async move {
        mailbox
            .call(
                Operation::List(ListMessages {
                    folder_handle: folder,
                    page_size: 1,
                    cursor: None,
                }),
                Instant::now() + Duration::from_millis(400),
                CancellationToken::new(),
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(2), f.arrived.notified())
        .await
        .unwrap();
    let mut jobs = Vec::new();
    let mut cancellations = Vec::new();
    for _ in 0..16 {
        let mailbox = f.mailbox.clone();
        let cancel = CancellationToken::new();
        cancellations.push(cancel.clone());
        jobs.push(tokio::spawn(async move {
            mailbox
                .call(
                    Operation::Folders(ListFolders {
                        prefix: String::new(),
                        include_counts: false,
                        page_size: 1,
                        cursor: None,
                    }),
                    Instant::now() + Duration::from_secs(5),
                    cancel,
                )
                .await
        }));
    }
    for _ in 0..3 {
        tokio::task::yield_now().await;
    }
    let busy = f
        .call(Operation::Folders(ListFolders {
            prefix: String::new(),
            include_counts: false,
            page_size: 1,
            cursor: None,
        }))
        .await;
    assert_eq!(busy, Err(MailError(E::Busy)));
    for token in cancellations {
        token.cancel();
    }
    for job in jobs {
        assert_eq!(job.await.unwrap(), Err(MailError(E::Cancelled)));
    }
    assert_eq!(active.await.unwrap(), Err(MailError(E::DeadlineExceeded)));
}

#[test]
fn decode_encodings_and_html_without_remote_content() {
    let part = crate::mail::Part {
        section: "1".into(),
        html: false,
        charset: "windows-1252".into(),
        encoding: "quoted-printable".into(),
        size: 0,
    };
    assert_eq!(
        crate::mail::decode(&part, b"caf=E9\r\n".to_vec())
            .unwrap()
            .0,
        "café\r\n"
    );
    let part = crate::mail::Part {
        html: true,
        charset: "utf-8".into(),
        encoding: "8bit".into(),
        ..part
    };
    let text=crate::mail::decode(&part,b"<html><head>hidden</head><script>secret</script><p>hello &amp; goodbye</p><img src='https://example.com/pixel'></html>".to_vec()).unwrap().0;
    assert!(text.contains("hello & goodbye"));
    assert!(!text.contains("secret") && !text.contains("hidden") && !text.contains("pixel"));
}

#[tokio::test]
async fn response_budget_continuation_does_not_drop_escaped_text() {
    let f = Fixture::new().await;
    let original = "\u{1}".repeat(70000);
    f.data.lock().unwrap().messages.get_mut(&1).unwrap().text = original.clone();
    let page = f.messages().await;
    let handle = page["messages"][1]["message_handle"]
        .as_str()
        .unwrap()
        .to_string();
    let mut cursor = None;
    let mut joined = String::new();
    loop {
        let message = f
            .call(Operation::Get(GetMessage {
                message_handle: handle.clone(),
                body_chunk_bytes: 65536,
                body_cursor: cursor,
            }))
            .await
            .unwrap();
        assert!(serde_json::to_vec(&message).unwrap().len() < RESPONSE_LIMIT);
        joined.push_str(message["body"]["text"].as_str().unwrap());
        cursor = message["body"]["next_cursor"].as_str().map(str::to_string);
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(joined, original);
}

#[tokio::test]
async fn removed_members_are_reported_and_body_cache_revalidates_existence() {
    let f = Fixture::new().await;
    let folder = f.folder().await;
    let first = f
        .call(Operation::List(ListMessages {
            folder_handle: folder.clone(),
            page_size: 1,
            cursor: None,
        }))
        .await
        .unwrap();
    let handle = first["messages"][0]["message_handle"]
        .as_str()
        .unwrap()
        .to_string();
    let body = f
        .call(Operation::Get(GetMessage {
            message_handle: handle.clone(),
            body_chunk_bytes: 256,
            body_cursor: None,
        }))
        .await
        .unwrap();
    assert_eq!(body["body"]["text"], "second message");
    {
        let mut d = f.data.lock().unwrap();
        d.messages.remove(&1);
        d.messages.remove(&2);
    }
    let next = f
        .call(Operation::List(ListMessages {
            folder_handle: folder,
            page_size: 50,
            cursor: Some(first["next_cursor"].as_str().unwrap().into()),
        }))
        .await
        .unwrap();
    assert_eq!(next["skipped_unavailable"], 1);
    assert_eq!(next["messages"].as_array().unwrap().len(), 0);
    assert_eq!(
        f.call(Operation::Get(GetMessage {
            message_handle: handle,
            body_chunk_bytes: 256,
            body_cursor: None
        }))
        .await,
        Err(MailError(E::MessageNotFound))
    );
}

#[tokio::test]
async fn framing_rejects_deep_or_large_responses_before_protocol_parsing() {
    let payloads = [
        b"* 1 FETCH (UID 1 BODY[1] {65537}\r\n".to_vec(),
        format!("* 1 FETCH (BODYSTRUCTURE {}\r\n", "(".repeat(65)).into_bytes(),
        format!("* SEARCH {}\r\n", vec!["1"; 10001].join(" ")).into_bytes(),
        format!("* OK {}\r\n", "x".repeat(256 * 1024)).into_bytes(),
    ];
    for payload in payloads {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = stream.write_all(b"* OK test\r\n").await;
            let _ = stream.write_all(&payload).await;
        });
        let mut wire = Connector::Fixture(addr)
            .connect(Arc::new(std::sync::atomic::AtomicUsize::new(0)))
            .await
            .unwrap();
        assert!(matches!(
            wire.response().await,
            Err(MailError(E::SizeLimit))
        ));
    }
}

#[test]
fn decoding_expansion_and_invalid_transfer_encodings_are_bounded() {
    let part = crate::mail::Part {
        section: "1".into(),
        html: false,
        charset: "windows-1252".into(),
        encoding: "8bit".into(),
        size: 0,
    };
    assert_eq!(
        crate::mail::decode(&part, vec![0x80; TEXT_LIMIT / 2 + 1]).map(|_| ()),
        Err(MailError(E::SizeLimit))
    );
    let part = crate::mail::Part {
        encoding: "base64".into(),
        ..part
    };
    assert!(crate::mail::decode(&part, b"!!!bad-base64!!!".to_vec()).is_err());
}

#[test]
fn malformed_html_tail_is_converted_with_bounded_work() {
    let part = crate::mail::Part {
        section: "1".into(),
        html: true,
        charset: "utf-8".into(),
        encoding: "8bit".into(),
        size: 0,
    };
    let tail = "<".repeat(256 * 1024);
    let started = std::time::Instant::now();
    let (text, _) = crate::mail::decode(&part, tail.as_bytes().to_vec()).unwrap();
    assert_eq!(text, tail);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "malformed HTML must not repeatedly rescan an unterminated tag"
    );
    let (text, _) =
        crate::mail::decode(&part, format!("visible<script>{tail}").into_bytes()).unwrap();
    assert_eq!(text, "visible");
}

#[test]
fn quoted_printable_requires_two_hexadecimal_digits() {
    let part = crate::mail::Part {
        section: "1".into(),
        html: false,
        charset: "utf-8".into(),
        encoding: "quoted-printable".into(),
        size: 0,
    };
    assert_eq!(
        crate::mail::decode(&part, b"=+A".to_vec()).map(|_| ()),
        Err(MailError(E::UnsupportedContent))
    );
    assert_eq!(
        crate::mail::decode(&part, b"=41=0a".to_vec()).unwrap().0,
        "A\n"
    );
}

#[test]
fn inputs_and_discovered_schemas_enforce_contracts() {
    assert!(
        !Filters {
            received_on_or_after: Some("2026-02-30".into()),
            ..Default::default()
        }
        .valid()
    );
    assert!(
        !Filters {
            received_on_or_after: Some("2026-11-05".into()),
            received_before: Some("2026-11-05".into()),
            ..Default::default()
        }
        .valid()
    );
    assert!(
        !Filters {
            subject_contains: Some("test\r\nUID STORE 1 +FLAGS \\Deleted".into()),
            ..Default::default()
        }
        .valid()
    );
    assert!(serde_json::from_value::<ListFolders>(serde_json::json!({"page_size":1.5})).is_err());
    assert!(serde_json::from_value::<ListFolders>(serde_json::json!({"unexpected":true})).is_err());
    let tools = crate::server::tools();
    let list = tools.iter().find(|t| t.name == "list_folders").unwrap();
    assert_eq!(list.input_schema["properties"]["page_size"]["maximum"], 100);
}

#[test]
fn handles_are_signed_and_mailbox_utf7_round_trips() {
    let a = crate::state::Tokens::new();
    let token = a
        .seal(&crate::state::Token::Folder("INBOX".into()))
        .unwrap();
    assert!(a.open(&token, false).is_ok());
    assert!(crate::state::Tokens::new().open(&token, false).is_err());
    let forged = format!("x{}", token);
    assert!(a.open(&forged, false).is_err());
    for name in ["INBOX", "旅行/你好", "a&b", "🦀"] {
        assert_eq!(
            crate::wire::decode_mailbox(&crate::wire::encode_mailbox(name)).unwrap(),
            name
        );
    }
}

#[test]
fn credentials_are_private_atomic_and_redacted() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("private/credentials.json");
    let c = Credentials::new("reader@example.com".into(), "password-canary".into()).unwrap();
    c.save(&path).unwrap();
    assert!(!format!("{c:?}").contains("canary"));
    assert!(Credentials::load(&path).is_ok());
    let checkout = temp.path().join("checkout");
    std::fs::create_dir(&checkout).unwrap();
    std::fs::create_dir(checkout.join(".git")).unwrap();
    assert!(c.save(&checkout.join("private/credentials.json")).is_err());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(Credentials::load(&path).is_err());
    }
}

#[test]
fn logging_never_admits_dependency_payloads() {
    #[derive(Clone)]
    struct Sink(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Sink {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Sink {
        type Writer = Self;
        fn make_writer(&'a self) -> Self {
            self.clone()
        }
    }
    let bytes = Arc::new(Mutex::new(Vec::new()));
    tracing::subscriber::with_default(crate::logging::subscriber(Sink(bytes.clone())), || {
        tracing::debug!(target:"rmcp::service",request="password-canary",result="mail-body-canary");
        tracing::warn!(target:"imap_proto",error="provider-secret-canary");
        tracing::info!(target:"icloud_mail_mcp::ops",success=true,"mail request completed");
    });
    let out = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
    assert!(out.contains("completed"));
    assert!(!out.contains("canary"));
}

#[tokio::test]
async fn real_http_mcp_discovery_and_mail_read() {
    use rmcp::{
        ServiceExt,
        model::{CallToolRequestParams, ClientConfig},
        transport::StreamableHttpClientTransport,
    };
    let f = Fixture::new().await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let stop = CancellationToken::new();
    let shutdown = stop.clone();
    let app = crate::server::application(
        f.mailbox.clone(),
        crate::server::ServerOptions {
            bind: addr.ip(),
            allowed_hosts: Vec::new(),
            allowed_origins: Vec::new(),
        },
        stop.clone(),
    );
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown.cancelled_owned())
            .await
            .unwrap();
    });
    let http = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap();
    let health = http
        .get(format!("http://{addr}/healthz"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        health.json::<serde_json::Value>().await.unwrap()["read_only"],
        true
    );
    assert!(
        f.data.lock().unwrap().commands.is_empty(),
        "health must not authenticate"
    );
    let response = http
        .post(format!("http://{addr}/mcp"))
        .header("Host", "unexpected.example")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 403);
    let response = http
        .post(format!("http://{addr}/mcp"))
        .header("Origin", "https://unexpected.example")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 403);
    let client = ClientConfig::default()
        .serve(StreamableHttpClientTransport::from_uri(format!(
            "http://{addr}/mcp"
        )))
        .await
        .unwrap();
    let tools = client.list_all_tools().await.unwrap();
    assert_eq!(tools.len(), 4);
    assert!(tools.iter().all(|t| t.output_schema.is_some()
        && t.annotations.as_ref().unwrap().read_only_hint == Some(true)));
    let folders = client
        .call_tool(
            CallToolRequestParams::new("list_folders").with_arguments(serde_json::Map::new()),
        )
        .await
        .unwrap();
    assert_ne!(folders.is_error, Some(true));
    let folder = folders.structured_content.unwrap()["folders"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == "INBOX")
        .unwrap()["folder_handle"]
        .clone();
    let mut args = serde_json::Map::new();
    args.insert("folder_handle".into(), folder);
    let messages = client
        .call_tool(CallToolRequestParams::new("list_messages").with_arguments(args))
        .await
        .unwrap();
    let handle = messages.structured_content.unwrap()["messages"][0]["message_handle"].clone();
    let mut args = serde_json::Map::new();
    args.insert("message_handle".into(), handle);
    let message = client
        .call_tool(CallToolRequestParams::new("get_message").with_arguments(args))
        .await
        .unwrap();
    assert_eq!(
        message.structured_content.unwrap()["body"]["text"],
        "second message"
    );
    client.cancel().await.unwrap();
    stop.cancel();
    server.await.unwrap();
}
