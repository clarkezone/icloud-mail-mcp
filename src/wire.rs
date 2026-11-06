//! Private bounded IMAP framing. No socket or arbitrary command API is exposed to MCP.
use crate::{
    credentials::Credentials,
    error::{ErrorCode, MailError, Result},
};
use base64::{Engine, engine::general_purpose::STANDARD_NO_PAD};
use imap_proto::{Response, Status};
use std::{
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio_rustls::{
    TlsConnector,
    rustls::{ClientConfig, RootCertStore, pki_types::ServerName},
};

const FRAME: usize = 256 * 1024;
const LITERAL: usize = 64 * 1024;
const REQUEST_BYTES: usize = 12 * 1024 * 1024;
trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}
pub(crate) struct Wire {
    io: BufReader<Pin<Box<dyn Io>>>,
    pub used: Arc<AtomicUsize>,
    tag: u64,
    pub(crate) allow_icloud_examine_read_write: bool,
}
#[derive(Clone, Default)]
pub(crate) enum Connector {
    #[default]
    ICloud,
    ICloudExamineCompatibility,
    #[cfg(test)]
    Fixture(std::net::SocketAddr),
    #[cfg(test)]
    FixtureExamineCompatibility(std::net::SocketAddr),
}
impl Connector {
    pub async fn connect(&self, used: Arc<AtomicUsize>) -> Result<Wire> {
        let io: Pin<Box<dyn Io>> = match self {
            Self::ICloud | Self::ICloudExamineCompatibility => {
                let stream = tokio::time::timeout(
                    Duration::from_secs(5),
                    TcpStream::connect(("imap.mail.me.com", 993)),
                )
                .await
                .map_err(|_| MailError(ErrorCode::DeadlineExceeded))??;
                let roots =
                    RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
                let provider = tokio_rustls::rustls::crypto::ring::default_provider();
                let config = ClientConfig::builder_with_provider(Arc::new(provider))
                    .with_safe_default_protocol_versions()
                    .map_err(|_| MailError(ErrorCode::TlsError))?
                    .with_root_certificates(roots)
                    .with_no_client_auth();
                let tls = tokio::time::timeout(
                    Duration::from_secs(5),
                    TlsConnector::from(Arc::new(config))
                        .connect(ServerName::try_from("imap.mail.me.com").unwrap(), stream),
                )
                .await
                .map_err(|_| MailError(ErrorCode::DeadlineExceeded))?
                .map_err(|_| MailError(ErrorCode::TlsError))?;
                Box::pin(tls)
            }
            #[cfg(test)]
            Self::Fixture(addr) | Self::FixtureExamineCompatibility(addr) => {
                Box::pin(TcpStream::connect(addr).await?)
            }
        };
        let mut wire = Wire {
            io: BufReader::new(io),
            used,
            tag: 0,
            allow_icloud_examine_read_write: match self {
                Self::ICloudExamineCompatibility => true,
                #[cfg(test)]
                Self::FixtureExamineCompatibility(_) => true,
                _ => false,
            },
        };
        if !matches!(
            wire.response().await?,
            Response::Data {
                status: Status::Ok,
                ..
            }
        ) {
            return Err(MailError(ErrorCode::UpstreamUnavailable));
        }
        Ok(wire)
    }
}

pub(crate) enum Arg {
    Raw(String),
    Str(zeroize::Zeroizing<String>),
}
pub(crate) fn raw(s: impl Into<String>) -> Arg {
    Arg::Raw(s.into())
}
pub(crate) fn string(s: impl Into<String>) -> Arg {
    Arg::Str(zeroize::Zeroizing::new(s.into()))
}
impl Wire {
    fn charge(&self, n: usize) -> Result<()> {
        if self.used.fetch_add(n, Ordering::Relaxed).saturating_add(n) > REQUEST_BYTES {
            Err(MailError(ErrorCode::SizeLimit))
        } else {
            Ok(())
        }
    }
    async fn line(&mut self) -> Result<Vec<u8>> {
        let mut line = Vec::new();
        loop {
            let buf = self.io.fill_buf().await?;
            if buf.is_empty() {
                return Err(MailError(ErrorCode::UpstreamUnavailable));
            }
            let n = buf
                .iter()
                .position(|b| *b == b'\n')
                .map_or(buf.len(), |n| n + 1);
            if line.len().saturating_add(n) > FRAME {
                return Err(MailError(ErrorCode::SizeLimit));
            }
            let done = buf[n - 1] == b'\n';
            let chunk = buf[..n].to_vec();
            self.charge(n)?;
            line.extend_from_slice(&chunk);
            self.io.consume(n);
            if done {
                if !line.ends_with(b"\r\n") {
                    return Err(MailError(ErrorCode::UpstreamUnavailable));
                }
                return Ok(line);
            }
        }
    }
    pub async fn response(&mut self) -> Result<Response<'static>> {
        let mut frame = Vec::new();
        let mut depth: usize = 0;
        let mut groups = 0;
        loop {
            let line = self.line().await?;
            let mut quoted = false;
            let mut escaped = false;
            for b in &line {
                if escaped {
                    escaped = false;
                    continue;
                }
                if quoted && *b == b'\\' {
                    escaped = true;
                    continue;
                }
                if *b == b'"' {
                    quoted = !quoted;
                    continue;
                }
                if !quoted {
                    if *b == b'(' {
                        depth += 1;
                        groups += 1;
                    }
                    if *b == b')' {
                        depth = depth.saturating_sub(1);
                    }
                    if depth > 64 || groups > 2048 {
                        return Err(MailError(ErrorCode::SizeLimit));
                    }
                }
            }
            let literal = if line.ends_with(b"}\r\n") && !quoted {
                let start = line
                    .iter()
                    .rposition(|b| *b == b'{')
                    .ok_or(MailError(ErrorCode::UpstreamUnavailable))?;
                let digits = &line[start + 1..line.len() - 3];
                let digits = digits.strip_suffix(b"+").unwrap_or(digits);
                let n = std::str::from_utf8(digits)
                    .ok()
                    .and_then(|s| s.parse::<usize>().ok())
                    .ok_or(MailError(ErrorCode::SizeLimit))?;
                if n > LITERAL {
                    return Err(MailError(ErrorCode::SizeLimit));
                }
                Some(n)
            } else {
                None
            };
            if frame.len() + line.len() + literal.unwrap_or(0) > FRAME {
                return Err(MailError(ErrorCode::SizeLimit));
            }
            frame.extend_from_slice(&line);
            if let Some(n) = literal {
                self.charge(n)?;
                let old = frame.len();
                frame.resize(old + n, 0);
                self.io.read_exact(&mut frame[old..]).await?;
            } else {
                break;
            }
        }
        if frame.starts_with(b"* SEARCH ")
            && frame.split(|b| b.is_ascii_whitespace()).count() > 10004
        {
            return Err(MailError(ErrorCode::SizeLimit));
        }
        let (rest, result) =
            Response::parse(&frame).map_err(|_| {
                tracing::warn!(target: "icloud_mail_mcp::ops", phase = "imap_response_parse", "IMAP response rejected");
                MailError(ErrorCode::UpstreamUnavailable)
            })?;
        if !rest.is_empty() {
            return Err(MailError(ErrorCode::UpstreamUnavailable));
        }
        Ok(result.into_owned())
    }
    pub async fn command(
        &mut self,
        args: Vec<Arg>,
        failure: ErrorCode,
    ) -> Result<Vec<Response<'static>>> {
        let allowed = args.first().is_some_and(|arg| match arg {
            Arg::Raw(command) => {
                let mut words = command.split_whitespace();
                match words.next() {
                    Some("LOGIN" | "CAPABILITY" | "LIST" | "STATUS" | "EXAMINE") => {
                        words.next().is_none()
                    }
                    Some("UID") => matches!(words.next(), Some("SEARCH" | "FETCH")),
                    _ => false,
                }
            }
            _ => false,
        });
        if !allowed {
            return Err(MailError(ErrorCode::InternalError));
        }
        let command_kind = match args.first() {
            Some(Arg::Raw(s)) if s.starts_with("UID FETCH") => "fetch",
            Some(Arg::Raw(s)) if s.starts_with("UID SEARCH") => "search",
            Some(Arg::Raw(s)) if s == "EXAMINE" => "examine",
            Some(Arg::Raw(s)) if s == "LOGIN" => "login",
            Some(Arg::Raw(s)) if s == "CAPABILITY" => "capability",
            Some(Arg::Raw(s)) if s == "LIST" => "list",
            Some(Arg::Raw(s)) if s == "STATUS" => "status",
            _ => "unknown",
        };
        self.tag += 1;
        let tag = format!("M{}", self.tag);
        self.io.write_all(tag.as_bytes()).await?;
        let mut replies = Vec::new();
        for arg in args {
            self.io.write_all(b" ").await?;
            match arg {
                Arg::Raw(s) => self.io.write_all(s.as_bytes()).await?,
                Arg::Str(s) => {
                    if s.chars().any(char::is_control) {
                        return Err(MailError(ErrorCode::ConfigurationError));
                    }
                    if s.is_ascii() {
                        let mut q = zeroize::Zeroizing::new(String::with_capacity(s.len() * 2 + 2));
                        q.push('"');
                        for c in s.chars() {
                            if c == '"' || c == '\\' {
                                q.push('\\');
                            }
                            q.push(c);
                        }
                        q.push('"');
                        self.io.write_all(q.as_bytes()).await?;
                    } else {
                        self.io
                            .write_all(format!("{{{}}}\r\n", s.len()).as_bytes())
                            .await?;
                        self.io.flush().await?;
                        loop {
                            let response = self.response().await?;
                            match response {
                                Response::Continue(_) => break,
                                Response::Done { .. } => return Err(MailError(failure)),
                                other => {
                                    if replies.len() >= 10000 {
                                        return Err(MailError(ErrorCode::SizeLimit));
                                    }
                                    replies.push(other);
                                }
                            }
                        }
                        self.io.write_all(s.as_bytes()).await?;
                    }
                }
            }
        }
        self.io.write_all(b"\r\n").await?;
        self.io.flush().await?;
        loop {
            let response = self.response().await.inspect_err(|error| {
                tracing::warn!(target: "icloud_mail_mcp::ops", command = command_kind, error_code = ?error.0, "IMAP response failed");
            })?;
            match response {
                Response::Done {
                    ref tag,
                    ref status,
                    ..
                } if tag.0 == format!("M{}", self.tag) => {
                    if *status != Status::Ok {
                        tracing::warn!(target: "icloud_mail_mcp::ops", command = command_kind, status = ?status, "IMAP command rejected");
                        return Err(MailError(failure));
                    }
                    replies.push(response);
                    return Ok(replies);
                }
                Response::Data {
                    status: Status::Bye,
                    ..
                }
                | Response::Done { .. }
                | Response::Continue(_) => return Err(MailError(ErrorCode::UpstreamUnavailable)),
                _ => {
                    if replies.len() >= 10000 {
                        return Err(MailError(ErrorCode::SizeLimit));
                    }
                    replies.push(response);
                }
            }
        }
    }
    pub async fn login(&mut self, credentials: &Credentials) -> Result<()> {
        self.command(
            vec![
                raw("LOGIN"),
                string(credentials.username.to_string()),
                string(credentials.password.to_string()),
            ],
            ErrorCode::AuthenticationFailed,
        )
        .await?;
        self.command(vec![raw("CAPABILITY")], ErrorCode::UpstreamUnavailable)
            .await?;
        Ok(())
    }
}

pub(crate) fn encode_mailbox(name: &str) -> String {
    let mut out = String::new();
    let mut utf16 = Vec::new();
    fn flush(bytes: &mut Vec<u8>, out: &mut String) {
        if !bytes.is_empty() {
            out.push('&');
            out.push_str(&STANDARD_NO_PAD.encode(&*bytes).replace('/', ","));
            out.push('-');
            bytes.clear();
        }
    }
    for ch in name.chars() {
        if (' '..='~').contains(&ch) {
            flush(&mut utf16, &mut out);
            if ch == '&' {
                out.push_str("&-");
            } else {
                out.push(ch);
            }
        } else {
            for u in ch.encode_utf16(&mut [0; 2]) {
                utf16.extend_from_slice(&u.to_be_bytes());
            }
        }
    }
    flush(&mut utf16, &mut out);
    out
}
pub(crate) fn decode_mailbox(name: &str) -> Result<String> {
    let mut out = String::new();
    let mut rest = name;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i + 1..];
        let end = rest
            .find('-')
            .ok_or(MailError(ErrorCode::UpstreamUnavailable))?;
        if end == 0 {
            out.push('&');
        } else {
            let bytes = STANDARD_NO_PAD
                .decode(rest[..end].replace(',', "/"))
                .map_err(|_| MailError(ErrorCode::UpstreamUnavailable))?;
            if bytes.len() % 2 != 0 {
                return Err(MailError(ErrorCode::UpstreamUnavailable));
            }
            let units: Vec<_> = bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| u16::from_be_bytes([b[0], b[1]]))
                .collect();
            out.push_str(
                &String::from_utf16(&units)
                    .map_err(|_| MailError(ErrorCode::UpstreamUnavailable))?,
            );
        }
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}
