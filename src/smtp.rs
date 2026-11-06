//! Bounded SMTP framing. Production always requires STARTTLS on the fixed iCloud endpoint.
use crate::{
    credentials::Credentials,
    sending::{SendOutcome, SendReason, SendSettings},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use lettre::Message;
use std::{pin::Pin, sync::Arc};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    net::TcpStream,
    time::Instant,
};
use tokio_rustls::{
    TlsConnector,
    rustls::{ClientConfig, RootCertStore, pki_types::ServerName},
};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;
trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}
type Stream = Pin<Box<dyn Io>>;
#[derive(Clone)]
pub(crate) enum Connector {
    ICloud,
    #[cfg(test)]
    Fixture(std::net::SocketAddr),
    #[cfg(test)]
    FixtureStarttls(std::net::SocketAddr),
}
pub(crate) struct Outcome {
    pub outcome: SendOutcome,
    pub reason: Option<SendReason>,
    pub code: Option<u16>,
}
#[derive(Clone, Copy)]
struct Failure {
    reason: SendReason,
    code: Option<u16>,
    negative: bool,
}
type Result<T> = std::result::Result<T, Failure>;
fn failure(reason: SendReason) -> Failure {
    Failure {
        reason,
        code: None,
        negative: false,
    }
}
struct Reply {
    code: u16,
    lines: Vec<Vec<u8>>,
}
struct Wire {
    io: BufReader<Stream>,
    used: usize,
}
impl Wire {
    async fn line(&mut self) -> Result<Vec<u8>> {
        let mut line = Vec::new();
        loop {
            let buf = self
                .io
                .fill_buf()
                .await
                .map_err(|_| failure(SendReason::ConnectionLost))?;
            if buf.is_empty() {
                return Err(failure(SendReason::ConnectionLost));
            }
            let n = buf
                .iter()
                .position(|b| *b == b'\n')
                .map_or(buf.len(), |i| i + 1);
            if line.len() + n > 4096 || self.used + n > 128 * 1024 {
                return Err(failure(SendReason::ProtocolError));
            }
            let done = buf[n - 1] == b'\n';
            line.extend_from_slice(&buf[..n]);
            self.io.consume(n);
            self.used += n;
            if done {
                if !line.ends_with(b"\r\n") {
                    return Err(failure(SendReason::ProtocolError));
                }
                return Ok(line);
            }
        }
    }
    async fn reply(&mut self) -> Result<Reply> {
        let mut lines = Vec::new();
        let mut code = None;
        let mut bytes = 0;
        loop {
            let line = self.line().await?;
            bytes += line.len();
            if line.len() < 5
                || bytes > 65536
                || lines.len() >= 128
                || !line[..3].iter().all(u8::is_ascii_digit)
            {
                return Err(failure(SendReason::ProtocolError));
            }
            let n = u16::from(line[0] - b'0') * 100
                + u16::from(line[1] - b'0') * 10
                + u16::from(line[2] - b'0');
            if code.is_some_and(|c| c != n) {
                return Err(failure(SendReason::ProtocolError));
            }
            code = Some(n);
            let last = if line.len() == 5 {
                true
            } else {
                match line[3] {
                    b' ' => true,
                    b'-' => false,
                    _ => return Err(failure(SendReason::ProtocolError)),
                }
            };
            lines.push(if line.len() == 5 {
                Vec::new()
            } else {
                line[4..line.len() - 2].to_vec()
            });
            if last {
                return Ok(Reply { code: n, lines });
            }
        }
    }
    async fn write(&mut self, bytes: &[u8]) -> Result<()> {
        self.io
            .write_all(bytes)
            .await
            .map_err(|_| failure(SendReason::ConnectionLost))?;
        self.io
            .flush()
            .await
            .map_err(|_| failure(SendReason::ConnectionLost))
    }
    async fn command(&mut self, bytes: &[u8]) -> Result<Reply> {
        self.write(bytes).await?;
        self.reply().await
    }
}
fn expect(reply: Reply, code: u16) -> Result<Reply> {
    if reply.code == code {
        Ok(reply)
    } else if (400..600).contains(&reply.code) {
        Err(Failure {
            reason: SendReason::SmtpRejected,
            code: Some(reply.code),
            negative: true,
        })
    } else {
        Err(failure(SendReason::ProtocolError))
    }
}
fn capability(reply: &Reply, name: &[u8]) -> bool {
    reply.lines.iter().any(|line| {
        line.split(|b| b.is_ascii_whitespace())
            .next()
            .is_some_and(|part| part.eq_ignore_ascii_case(name))
    })
}
fn auth_plain(reply: &Reply) -> bool {
    reply.lines.iter().any(|line| {
        let mut words = line.split(|b| b.is_ascii_whitespace());
        words
            .next()
            .is_some_and(|part| part.eq_ignore_ascii_case(b"AUTH"))
            && words.any(|word| word.eq_ignore_ascii_case(b"PLAIN"))
    })
}
async fn connect(connector: &Connector) -> Result<Wire> {
    let stream = match connector {
        Connector::ICloud => TcpStream::connect(("smtp.mail.me.com", 587)).await,
        #[cfg(test)]
        Connector::Fixture(addr) | Connector::FixtureStarttls(addr) => {
            TcpStream::connect(addr).await
        }
    }
    .map_err(|_| failure(SendReason::ConnectionFailed))?;
    let mut wire = Wire {
        io: BufReader::new(Box::pin(stream) as Stream),
        used: 0,
    };
    expect(wire.reply().await?, 220)?;
    let hello = expect(
        wire.command(b"EHLO icloud-mail-mcp.invalid\r\n").await?,
        250,
    )?;
    let require_tls = match connector {
        #[cfg(test)]
        Connector::Fixture(_) => false,
        _ => true,
    };
    if require_tls {
        if !capability(&hello, b"STARTTLS") {
            return Err(failure(SendReason::TlsRequired));
        }
        expect(wire.command(b"STARTTLS\r\n").await?, 220)?;
        if !wire.io.buffer().is_empty() {
            return Err(failure(SendReason::ProtocolError));
        }
        let roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let config = ClientConfig::builder_with_provider(Arc::new(
            tokio_rustls::rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|_| failure(SendReason::TlsError))?
        .with_root_certificates(roots)
        .with_no_client_auth();
        let tls = TlsConnector::from(Arc::new(config))
            .connect(
                ServerName::try_from("smtp.mail.me.com").unwrap(),
                wire.io.into_inner(),
            )
            .await
            .map_err(|_| failure(SendReason::TlsError))?;
        wire.io = BufReader::new(Box::pin(tls) as Stream);
    }
    Ok(wire)
}
pub(crate) async fn submit(
    connector: &Connector,
    credentials: &Credentials,
    settings: &SendSettings,
    message: &Message,
    deadline: Instant,
    shutdown: &CancellationToken,
) -> Outcome {
    let mut body_started = false;
    let exchange = async {
        let mut wire = connect(connector).await?;
        // Always refresh extensions after the TLS upgrade; no pre-TLS AUTH trust.
        let hello = expect(
            wire.command(b"EHLO icloud-mail-mcp.invalid\r\n").await?,
            250,
        )?;
        if !auth_plain(&hello) {
            return Err(failure(SendReason::AuthenticationFailed));
        }
        let mut plain = Zeroizing::new(Vec::new());
        plain.push(0);
        plain.extend_from_slice(settings.smtp_username.as_bytes());
        plain.push(0);
        plain.extend_from_slice(credentials.password.as_bytes());
        let encoded = Zeroizing::new(STANDARD.encode(&*plain));
        let command = Zeroizing::new(format!("AUTH PLAIN {}\r\n", *encoded));
        let mut auth = wire.command(command.as_bytes()).await?;
        if auth.code == 334 {
            let continuation = Zeroizing::new(format!("{}\r\n", *encoded));
            auth = wire.command(continuation.as_bytes()).await?;
        }
        expect(auth, 235).map_err(|mut e| {
            e.reason = SendReason::AuthenticationFailed;
            e
        })?;
        expect(
            wire.command(format!("MAIL FROM:<{}>\r\n", settings.sender_address).as_bytes())
                .await?,
            250,
        )?;
        let mut recipients = Vec::new();
        for recipient in message.envelope().to() {
            let recipient = recipient.to_string();
            if recipients.contains(&recipient) {
                continue;
            }
            let reply = wire
                .command(format!("RCPT TO:<{}>\r\n", recipient).as_bytes())
                .await?;
            if !matches!(reply.code, 250..=252) {
                expect(reply, 250)?;
            }
            recipients.push(recipient);
        }
        expect(wire.command(b"DATA\r\n").await?, 354)?;
        let bytes = message.formatted();
        let mut data = Vec::with_capacity(bytes.len() + 1024);
        let mut line_start = true;
        for b in bytes {
            if line_start && b == b'.' {
                data.push(b'.');
            }
            data.push(b);
            line_start = b == b'\n';
        }
        if !data.ends_with(b"\r\n") {
            data.extend_from_slice(b"\r\n");
        }
        data.extend_from_slice(b".\r\n");
        body_started = true;
        wire.write(&data).await?;
        expect(wire.reply().await?, 250)?;
        Ok(())
    };
    let result = tokio::select! {
        _=shutdown.cancelled()=>Err(failure(SendReason::Cancelled)),
        result=tokio::time::timeout_at(deadline,exchange)=>result.unwrap_or_else(|_|Err(failure(SendReason::DeadlineExceeded))),
    };
    match result {
        Ok(()) => Outcome {
            outcome: SendOutcome::Accepted,
            reason: None,
            code: Some(250),
        },
        Err(error) => Outcome {
            outcome: if !body_started || error.negative {
                SendOutcome::NotAccepted
            } else {
                SendOutcome::OutcomeUnknown
            },
            reason: Some(error.reason),
            code: error.code,
        },
    }
}
