use super::*;
use crate::smtp::Connector;
use std::sync::{Arc, Mutex};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
};

#[derive(Clone, Copy)]
enum Mode {
    Accept,
    RejectRecipient,
    RejectFinal,
    DropAfterData,
    HoldAfterData,
    RejectAuth,
    NoStarttls,
    BadTls,
    OversizedGreeting,
    HoldGreeting,
}
#[derive(Default)]
struct Trace {
    commands: Vec<String>,
    messages: Vec<Vec<u8>>,
    auth: usize,
}
struct Fixture {
    addr: std::net::SocketAddr,
    trace: Arc<Mutex<Trace>>,
    arrived: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
    stop: CancellationToken,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}
impl Fixture {
    async fn new(mode: Mode) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let trace = Arc::new(Mutex::new(Trace::default()));
        let shared = trace.clone();
        let arrived = Arc::new(tokio::sync::Notify::new());
        let signal = arrived.clone();
        let release = Arc::new(tokio::sync::Notify::new());
        let unhold = release.clone();
        let stop = CancellationToken::new();
        let stopped = stop.clone();
        tokio::spawn(async move {
            loop {
                let stream = tokio::select! {_=stopped.cancelled()=>return,s=listener.accept()=>s.unwrap().0};
                let trace = shared.clone();
                let arrived = signal.clone();
                let release = unhold.clone();
                let stop = stopped.clone();
                tokio::spawn(async move {
                    let (read, mut write) = stream.into_split();
                    let mut read = BufReader::new(read);
                    if matches!(mode, Mode::HoldGreeting) {
                        stop.cancelled().await;
                        return;
                    }
                    let greeting = if matches!(mode, Mode::OversizedGreeting) {
                        format!("220 {}", "x".repeat(8192))
                    } else {
                        "220 synthetic SMTP\r\n".into()
                    };
                    if write.write_all(greeting.as_bytes()).await.is_err() {
                        return;
                    }
                    let mut recipients = 0;
                    loop {
                        let mut line = String::new();
                        let n = tokio::select! {_=stop.cancelled()=>return,n=read.read_line(&mut line)=>n.unwrap_or(0)};
                        if n == 0 {
                            return;
                        }
                        let command = line.trim_end().to_owned();
                        if command.starts_with("AUTH ") {
                            trace.lock().unwrap().auth += 1;
                        } else {
                            trace.lock().unwrap().commands.push(command.clone());
                        }
                        let reply = if command.starts_with("EHLO ") {
                            if matches!(mode, Mode::BadTls) {
                                "250-fixture\r\n250-STARTTLS\r\n250 AUTH PLAIN\r\n"
                            } else {
                                "250-fixture\r\n250 AUTH PLAIN\r\n"
                            }
                        } else if command == "STARTTLS" {
                            let _ = write.write_all(b"220 start TLS\r\n").await;
                            let _ = write.write_all(b"invalid TLS record\r\n").await;
                            return;
                        } else if command.starts_with("AUTH PLAIN ") {
                            if matches!(mode, Mode::RejectAuth) {
                                "535 provider-secret-canary\r\n"
                            } else {
                                "235 authenticated\r\n"
                            }
                        } else if command.starts_with("MAIL FROM:") {
                            "250 sender\r\n"
                        } else if command.starts_with("RCPT TO:") {
                            recipients += 1;
                            if matches!(mode, Mode::RejectRecipient) && recipients == 2 {
                                "550 recipient-secret-canary\r\n"
                            } else {
                                "250 recipient\r\n"
                            }
                        } else if command == "DATA" {
                            if write.write_all(b"354 continue\r\n").await.is_err() {
                                return;
                            }
                            let mut body = Vec::new();
                            loop {
                                let mut data = Vec::new();
                                let n = tokio::select! {_=stop.cancelled()=>return,n=read.read_until(b'\n',&mut data)=>n.unwrap_or(0)};
                                if n == 0 {
                                    return;
                                }
                                if data == b".\r\n" {
                                    break;
                                }
                                if data.starts_with(b"..") {
                                    data.remove(0);
                                }
                                body.extend(data);
                            }
                            trace.lock().unwrap().messages.push(body);
                            arrived.notify_one();
                            if matches!(mode, Mode::DropAfterData) {
                                return;
                            }
                            if matches!(mode, Mode::HoldAfterData) {
                                tokio::select! {_=stop.cancelled()=>return,_=release.notified()=>{}}
                            }
                            if matches!(mode, Mode::RejectFinal) {
                                "554 provider-secret-canary\r\n"
                            } else {
                                "250 accepted\r\n"
                            }
                        } else {
                            "500 unexpected\r\n"
                        };
                        if write.write_all(reply.as_bytes()).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        Self {
            addr,
            trace,
            arrived,
            release,
            stop,
        }
    }
    async fn service(&self, path: &Path, stop: CancellationToken) -> SubmissionService {
        let connector = Connector::Fixture(self.addr);
        SubmissionService::with_connector(
            credentials(),
            settings(),
            path.to_owned(),
            stop,
            connector,
        )
        .await
        .unwrap()
    }
}
fn credentials() -> Credentials {
    Credentials::new("reader@example.com".into(), "password-canary".into()).unwrap()
}
fn settings() -> SendSettings {
    SendSettings::new("sender@example.com".into(), "login@example.com".into()).unwrap()
}
fn request() -> SendMessage {
    SendMessage {
        idempotency_key: uuid::Uuid::new_v4().to_string(),
        to: vec!["recipient@example.com".into()],
        cc: vec!["copy@example.com".into()],
        bcc: vec!["hidden@example.com".into()],
        subject: "Subject 世界".into(),
        body_text: "body-secret-canary\n.terminator\n世界 🦀".into(),
    }
}
async fn send(service: &SubmissionService, r: SendMessage) -> Result<SendStatus> {
    service
        .send(
            r,
            Instant::now() + Duration::from_secs(4),
            CancellationToken::new(),
        )
        .await
}

#[tokio::test]
async fn accepted_submission_is_durable_deduplicated_and_hides_bcc() {
    let f = Fixture::new(Mode::Accept).await;
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("private/send.sqlite3");
    let stop = CancellationToken::new();
    let service = f.service(&path, stop.clone()).await;
    let r = request();
    let (first, repeat) = tokio::join!(send(&service, r.clone()), send(&service, r.clone()));
    let first = first.unwrap();
    let repeat = repeat.unwrap();
    assert_eq!(first.outcome, SendOutcome::Accepted);
    assert_eq!(first.message_id, repeat.message_id);
    let mut conflicting = r.clone();
    conflicting.subject = "different".into();
    assert!(matches!(
        send(&service, conflicting).await,
        Err(MailError(E::IdempotencyConflict))
    ));
    {
        let trace = f.trace.lock().unwrap();
        assert_eq!(trace.messages.len(), 1);
        assert_eq!(trace.auth, 1);
        assert!(
            trace
                .commands
                .contains(&"RCPT TO:<hidden@example.com>".into())
        );
        let wire = String::from_utf8(trace.messages[0].clone()).unwrap();
        assert!(!wire.contains("Bcc:"));
        assert!(!wire.contains("hidden@example.com"));
        assert!(wire.contains("Content-Transfer-Encoding: base64"));
        let parsed = mail_parser::MessageParser::default()
            .parse(wire.as_bytes())
            .unwrap();
        assert!(parsed.body_text(0).unwrap().contains("世界 🦀"));
    }
    stop.cancel();
    service.tx.closed().await;
    drop(service);
    let next_stop = CancellationToken::new();
    let next = f.service(&path, next_stop.clone()).await;
    let replay = send(&next, r.clone()).await.unwrap();
    assert_eq!(replay.outcome, SendOutcome::Accepted);
    assert_eq!(replay.message_id, first.message_id);
    assert_eq!(f.trace.lock().unwrap().messages.len(), 1);
    let status = next
        .status(
            r.idempotency_key,
            Instant::now() + Duration::from_secs(1),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(status.outcome, SendOutcome::Accepted);
    next_stop.cancel();
    next.tx.closed().await;
    drop(next);
    let disk = std::fs::read(&path).unwrap();
    for secret in [
        "body-secret-canary",
        "recipient@example.com",
        "hidden@example.com",
        "Subject 世界",
        "password-canary",
        "sender@example.com",
        "login@example.com",
    ] {
        assert!(!disk.windows(secret.len()).any(|w| w == secret.as_bytes()));
    }
}
#[tokio::test]
async fn recipient_rejection_never_sends_data_and_final_rejection_is_definite() {
    for mode in [Mode::RejectRecipient, Mode::RejectFinal] {
        let f = Fixture::new(mode).await;
        let tmp = tempfile::tempdir().unwrap();
        let stop = CancellationToken::new();
        let s = f
            .service(&tmp.path().join("private/send.sqlite3"), stop.clone())
            .await;
        let result = send(&s, request()).await.unwrap();
        assert_eq!(result.outcome, SendOutcome::NotAccepted);
        assert_eq!(result.reason, Some(SendReason::SmtpRejected));
        if matches!(mode, Mode::RejectRecipient) {
            assert!(!f.trace.lock().unwrap().commands.contains(&"DATA".into()));
            assert!(f.trace.lock().unwrap().messages.is_empty());
        }
        stop.cancel();
        s.tx.closed().await;
    }
}
#[tokio::test]
async fn lost_final_ack_and_interrupted_attempts_are_never_retried() {
    let f = Fixture::new(Mode::DropAfterData).await;
    let tmp = tempfile::tempdir().unwrap();
    let stop = CancellationToken::new();
    let path = tmp.path().join("private/send.sqlite3");
    let s = f.service(&path, stop.clone()).await;
    let r = request();
    let result = send(&s, r.clone()).await.unwrap();
    assert_eq!(result.outcome, SendOutcome::OutcomeUnknown);
    assert_eq!(
        send(&s, r).await.unwrap().outcome,
        SendOutcome::OutcomeUnknown
    );
    assert_eq!(f.trace.lock().unwrap().messages.len(), 1);
    stop.cancel();
    s.tx.closed().await;
    drop(s);
    let j = Journal::open(&path, &settings(), true).unwrap();
    let pending = request();
    let fp = j.fingerprint(&pending).unwrap();
    j.claim(
        &fp,
        &SendStatus {
            idempotency_key: pending.idempotency_key.clone(),
            message_id: "<synthetic@example.com>".into(),
            outcome: SendOutcome::OutcomeUnknown,
            reason: Some(SendReason::InProgress),
            smtp_code: None,
            updated_at: now(),
        },
    )
    .unwrap();
    drop(j);
    let stop = CancellationToken::new();
    let s = f.service(&path, stop.clone()).await;
    let result = send(&s, pending).await.unwrap();
    assert_eq!(result.reason, Some(SendReason::Interrupted));
    assert_eq!(f.trace.lock().unwrap().messages.len(), 1);
    stop.cancel();
    s.tx.closed().await;
}
#[tokio::test]
async fn caller_disconnect_keeps_one_attempt_and_cancelled_queue_work_is_skipped() {
    let f = Fixture::new(Mode::HoldAfterData).await;
    let tmp = tempfile::tempdir().unwrap();
    let stop = CancellationToken::new();
    let s = f
        .service(&tmp.path().join("private/send.sqlite3"), stop.clone())
        .await;
    let r = request();
    let active = s.clone();
    let first = r.clone();
    let task = tokio::spawn(async move { send(&active, first).await });
    tokio::time::timeout(Duration::from_secs(2), f.arrived.notified())
        .await
        .unwrap();
    let cancelled = request();
    let key = cancelled.idempotency_key.clone();
    let cancel = CancellationToken::new();
    let signal = cancel.clone();
    let queued = s.clone();
    let second = tokio::spawn(async move {
        queued
            .send(cancelled, Instant::now() + Duration::from_secs(2), signal)
            .await
    });
    tokio::task::yield_now().await;
    cancel.cancel();
    task.abort();
    f.release.notify_one();
    let _ = second.await;
    let result = send(&s, r).await.unwrap();
    assert_eq!(result.outcome, SendOutcome::Accepted);
    assert_eq!(f.trace.lock().unwrap().messages.len(), 1);
    assert!(matches!(
        s.status(
            key,
            Instant::now() + Duration::from_secs(1),
            CancellationToken::new()
        )
        .await,
        Err(MailError(E::SubmissionNotFound))
    ));
    stop.cancel();
    s.tx.closed().await;
}
#[tokio::test]
async fn auth_rejection_breaker_and_reply_limits_fail_without_sending() {
    for mode in [Mode::RejectAuth, Mode::OversizedGreeting] {
        let f = Fixture::new(mode).await;
        let tmp = tempfile::tempdir().unwrap();
        let stop = CancellationToken::new();
        let s = f
            .service(&tmp.path().join("private/send.sqlite3"), stop.clone())
            .await;
        let result = send(&s, request()).await.unwrap();
        assert_eq!(result.outcome, SendOutcome::NotAccepted);
        if matches!(mode, Mode::RejectAuth) {
            assert_eq!(result.reason, Some(SendReason::AuthenticationFailed));
            assert!(matches!(
                send(&s, request()).await,
                Err(MailError(E::AuthenticationFailed))
            ));
            assert_eq!(f.trace.lock().unwrap().auth, 1);
        } else {
            assert_eq!(result.reason, Some(SendReason::ProtocolError));
        }
        assert!(f.trace.lock().unwrap().messages.is_empty());
        stop.cancel();
        s.tx.closed().await;
    }
}
#[tokio::test]
async fn missing_starttls_and_failed_tls_never_transmit_authentication() {
    for mode in [Mode::NoStarttls, Mode::BadTls] {
        let f = Fixture::new(mode).await;
        let r = request();
        let message = build_message(&settings(), &r, "<synthetic@example.com>").unwrap();
        let result = smtp::submit(
            &Connector::FixtureStarttls(f.addr),
            &credentials(),
            &settings(),
            &message,
            Instant::now() + Duration::from_secs(2),
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(result.outcome, SendOutcome::NotAccepted);
        assert!(matches!(
            result.reason,
            Some(SendReason::TlsRequired | SendReason::TlsError | SendReason::ProtocolError)
        ));
        assert_eq!(f.trace.lock().unwrap().auth, 0);
        assert!(f.trace.lock().unwrap().messages.is_empty());
    }
}
#[test]
fn input_headers_and_sender_are_validated_before_submission() {
    let mut r = request();
    assert!(r.valid());
    r.to[0] = "victim@example.com\r\nRCPT TO:<other@example.com>".into();
    assert!(!r.valid());
    let mut r = request();
    r.subject = "subject\r\nBcc: other@example.com".into();
    assert!(!r.valid());
    let mut r = request();
    r.body_text = "x".repeat(BODY_LIMIT + 1);
    assert!(!r.valid());
    let mut r = request();
    r.idempotency_key = "not-a-uuid".into();
    assert!(!r.valid());
    assert!(
        SendSettings::new(
            "Sender <sender@example.com>".into(),
            "login@example.com".into()
        )
        .is_err()
    );
    assert!(serde_json::from_value::<SendMessage>(serde_json::json!({"idempotency_key":uuid::Uuid::new_v4().to_string(),"to":["recipient@example.com"],"subject":"x","body_text":"x","from":"spoof@example.com"})).is_err());
}
#[test]
fn journal_is_private_exclusive_bounded_and_account_scoped() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("private/send.sqlite3");
    let j = Journal::open(&path, &settings(), true).unwrap();
    assert!(Journal::open(&path, &settings(), true).is_err());
    let r = request();
    let fp = j.fingerprint(&r).unwrap();
    j.claim(
        &fp,
        &SendStatus {
            idempotency_key: r.idempotency_key.clone(),
            message_id: "<synthetic@example.com>".into(),
            outcome: SendOutcome::Accepted,
            reason: None,
            smtp_code: Some(250),
            updated_at: now(),
        },
    )
    .unwrap();
    drop(j);
    let different =
        SendSettings::new("other@example.com".into(), "other@example.com".into()).unwrap();
    let j = Journal::open(&path, &different, true).unwrap();
    assert!(matches!(
        j.status(&r.idempotency_key),
        Err(MailError(E::SubmissionNotFound))
    ));
    j.db.execute("WITH RECURSIVE counter(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM counter WHERE n<9999) INSERT INTO submissions SELECT CAST(n AS TEXT),x'00',x'00','<test@example.com>','accepted',NULL,250,'synthetic' FROM counter",[]).unwrap();
    assert!(matches!(
        j.claim(
            &fp,
            &SendStatus {
                idempotency_key: uuid::Uuid::new_v4().to_string(),
                message_id: "<synthetic@example.com>".into(),
                outcome: SendOutcome::OutcomeUnknown,
                reason: None,
                smtp_code: None,
                updated_at: now()
            }
        ),
        Err(MailError(E::JournalFull))
    ));
    drop(j);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(Journal::open(&path, &settings(), true).is_err());
    }
    let checkout = tmp.path().join("checkout");
    fs::create_dir(&checkout).unwrap();
    fs::create_dir(checkout.join(".git")).unwrap();
    assert!(Journal::open(&checkout.join("send.sqlite3"), &settings(), true).is_err());
}

#[tokio::test]
async fn deadlines_distinguish_pre_data_failure_from_uncertain_submission() {
    for mode in [Mode::HoldGreeting, Mode::HoldAfterData] {
        let f = Fixture::new(mode).await;
        let tmp = tempfile::tempdir().unwrap();
        let stop = CancellationToken::new();
        let s = f
            .service(&tmp.path().join("private/send.sqlite3"), stop.clone())
            .await;
        let r = request();
        let _ = s
            .send(
                r.clone(),
                Instant::now() + Duration::from_millis(150),
                CancellationToken::new(),
            )
            .await;
        let result = s
            .status(
                r.idempotency_key.clone(),
                Instant::now() + Duration::from_secs(2),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(result.reason, Some(SendReason::DeadlineExceeded));
        assert_eq!(
            result.outcome,
            if matches!(mode, Mode::HoldGreeting) {
                SendOutcome::NotAccepted
            } else {
                SendOutcome::OutcomeUnknown
            }
        );
        let _ = send(&s, r).await.unwrap();
        assert!(f.trace.lock().unwrap().messages.len() <= 1);
        stop.cancel();
        s.tx.closed().await;
    }
}

#[tokio::test]
async fn real_http_exposes_send_tools_only_when_configured_and_replays_one_submission() {
    use rmcp::{
        ServiceExt,
        model::{CallToolRequestParams, ClientConfig},
        transport::StreamableHttpClientTransport,
    };
    let f = Fixture::new(Mode::Accept).await;
    let tmp = tempfile::tempdir().unwrap();
    for enabled in [false, true] {
        let stop = CancellationToken::new();
        let service = if enabled {
            Some(
                f.service(&tmp.path().join("private/send.sqlite3"), stop.clone())
                    .await,
            )
        } else {
            None
        };
        let mailbox = crate::worker::Mailbox::start(credentials(), stop.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = crate::server::application_with_sending(
            mailbox,
            service,
            crate::server::ServerOptions {
                bind: addr.ip(),
                allowed_hosts: Vec::new(),
                allowed_origins: Vec::new(),
            },
            stop.clone(),
        );
        let shutdown = stop.clone();
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown.cancelled_owned())
                .await
                .unwrap()
        });
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let health: serde_json::Value = http
            .get(format!("http://{addr}/healthz"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(health["read_only"], !enabled);
        assert_eq!(health["sending_enabled"], enabled);
        let client = ClientConfig::default()
            .serve(StreamableHttpClientTransport::from_uri(format!(
                "http://{addr}/mcp"
            )))
            .await
            .unwrap();
        let tools = client.list_all_tools().await.unwrap();
        assert_eq!(tools.len(), if enabled { 6 } else { 4 });
        if enabled {
            let send = tools.iter().find(|t| t.name == "send_message").unwrap();
            let hints = send.annotations.as_ref().unwrap();
            assert_eq!(hints.read_only_hint, Some(false));
            assert_eq!(hints.idempotent_hint, Some(false));
            assert_eq!(hints.destructive_hint, Some(true));
        }
        let r = request();
        let args = serde_json::to_value(&r)
            .unwrap()
            .as_object()
            .unwrap()
            .clone();
        let result = client
            .call_tool(CallToolRequestParams::new("send_message").with_arguments(args.clone()))
            .await
            .unwrap();
        if enabled {
            assert_eq!(result.is_error, Some(false));
            assert_eq!(result.structured_content.unwrap()["outcome"], "accepted");
            let replay = client
                .call_tool(CallToolRequestParams::new("send_message").with_arguments(args))
                .await
                .unwrap();
            assert_eq!(replay.is_error, Some(false));
            let status = client
                .call_tool(
                    CallToolRequestParams::new("get_send_status").with_arguments(
                        serde_json::json!({"idempotency_key":r.idempotency_key})
                            .as_object()
                            .unwrap()
                            .clone(),
                    ),
                )
                .await
                .unwrap();
            assert_eq!(status.structured_content.unwrap()["outcome"], "accepted");
            assert_eq!(f.trace.lock().unwrap().messages.len(), 1);
        } else {
            assert_eq!(result.is_error, Some(true));
            assert_eq!(
                result.structured_content.unwrap()["error"]["code"],
                "sending_disabled"
            );
            assert_eq!(f.trace.lock().unwrap().auth, 0);
        }
        client.cancel().await.unwrap();
        stop.cancel();
        server.await.unwrap();
    }
}

#[test]
fn private_sender_configuration_reuses_credential_file_hardening() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("private/sending.json");
    settings().save(&path).unwrap();
    let loaded = SendSettings::load(&path).unwrap();
    assert_eq!(loaded.sender_address, "sender@example.com");
    assert!(!format!("{loaded:?}").contains("example.com"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::{PermissionsExt, symlink};
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let link = tmp.path().join("private/link.json");
        symlink(&path, &link).unwrap();
        assert!(SendSettings::load(&link).is_err());
        fs::set_permissions(path.parent().unwrap(), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(SendSettings::load(&path).is_err());
    }
}

#[tokio::test]
async fn smtp_failure_logging_excludes_provider_credentials_and_mail_payloads() {
    #[derive(Clone)]
    struct Sink(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Sink {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
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
    tracing::subscriber::set_global_default(crate::logging::subscriber(Sink(bytes.clone())))
        .unwrap();
    tracing::debug!(target:"lettre::message",payload="body-secret-canary");
    tracing::warn!(target:"rusqlite",value="password-canary");
    let f = Fixture::new(Mode::RejectAuth).await;
    let tmp = tempfile::tempdir().unwrap();
    let stop = CancellationToken::new();
    let service = f
        .service(&tmp.path().join("private/send.sqlite3"), stop.clone())
        .await;
    let result = send(&service, request()).await.unwrap();
    assert_eq!(result.reason, Some(SendReason::AuthenticationFailed));
    stop.cancel();
    service.tx.closed().await;
    let output = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
    assert!(output.contains("Submission operation completed"));
    assert!(!output.contains("canary"));
    assert!(!output.contains("@example.com"));
}

#[tokio::test]
async fn production_start_requires_initialized_journal_and_never_silently_resets_it() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("private/send.sqlite3");
    assert!(
        SubmissionService::start(
            credentials(),
            settings(),
            path.clone(),
            CancellationToken::new()
        )
        .await
        .is_err()
    );
    SubmissionService::initialize_journal(&settings(), &path).unwrap();
    let stop = CancellationToken::new();
    let service = SubmissionService::start(credentials(), settings(), path.clone(), stop.clone())
        .await
        .unwrap();
    stop.cancel();
    service.tx.closed().await;
    drop(service);
    fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&path)
        .unwrap();
    assert!(
        SubmissionService::start(
            credentials(),
            settings(),
            path.clone(),
            CancellationToken::new()
        )
        .await
        .is_err()
    );
    fs::remove_file(&path).unwrap();
    assert!(
        SubmissionService::start(credentials(), settings(), path, CancellationToken::new())
            .await
            .is_err()
    );
}
