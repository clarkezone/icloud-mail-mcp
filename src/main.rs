use clap::{Parser, Subcommand};
use icloud_mail_mcp::{
    credentials::Credentials,
    logging,
    server::{ServerOptions, application},
    worker::Mailbox,
};
use std::{net::IpAddr, path::PathBuf};
use tokio_util::sync::CancellationToken;

#[derive(Parser)]
#[command(version, about = "Read-only iCloud Mail MCP bridge")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Explicit read-only TLS/authentication/folder check; never invoked by health polling.
    Check {
        #[arg(long, env = "ICLOUD_MAIL_CREDENTIALS")]
        credentials: PathBuf,
    },
    /// Store credentials locally using hidden input. The parent must be private.
    Configure {
        #[arg(long)]
        credentials: PathBuf,
    },
    /// Start the read-only MCP service; health checks do not authenticate to iCloud.
    Serve {
        #[arg(long, env = "ICLOUD_MAIL_CREDENTIALS")]
        credentials: PathBuf,
        #[arg(long, default_value = "127.0.0.1")]
        bind: IpAddr,
        #[arg(long, default_value_t = 8080)]
        port: u16,
        #[arg(long, value_delimiter = ',')]
        allowed_host: Vec<String>,
        #[arg(long, value_delimiter = ',')]
        allowed_origin: Vec<String>,
        /// Tolerate iCloud's tagged READ-WRITE EXAMINE reply; writes stay blocked.
        #[arg(long)]
        allow_icloud_examine_read_write: bool,
    },
    /// Print the exact MCP tool schemas without touching an account.
    Schemas,
}
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    logging::init()?;
    match cli.command {
        Command::Check { credentials } => {
            let shutdown = CancellationToken::new();
            let mailbox = Mailbox::start(Credentials::load(&credentials)?, shutdown.clone());
            let outcome = mailbox
                .call(
                    icloud_mail_mcp::model::Operation::Folders(
                        icloud_mail_mcp::model::ListFolders {
                            prefix: String::new(),
                            include_counts: false,
                            page_size: 1,
                            cursor: None,
                        },
                    ),
                    tokio::time::Instant::now() + std::time::Duration::from_secs(30),
                    CancellationToken::new(),
                )
                .await;
            shutdown.cancel();
            outcome?;
            println!("TLS, authentication, and read-only folder discovery succeeded.");
        }
        Command::Schemas => println!(
            "{}",
            serde_json::to_string_pretty(&icloud_mail_mcp::server::tools())?
        ),
        Command::Configure { credentials } => {
            let username = rpassword::prompt_password("iCloud login (hidden): ")?;
            let password = rpassword::prompt_password("App-specific password (hidden): ")?;
            Credentials::new(username, password)?.save(&credentials)?;
            println!("Credentials saved in private local storage.");
        }
        Command::Serve {
            credentials,
            bind,
            port,
            allowed_host,
            allowed_origin,
            allow_icloud_examine_read_write,
        } => {
            let credentials = Credentials::load(&credentials)?;
            let shutdown = CancellationToken::new();
            let mailbox = if allow_icloud_examine_read_write {
                tracing::warn!(target: "icloud_mail_mcp::ops", "Explicit iCloud EXAMINE compatibility enabled; command allowlist and PEEK restrictions remain active");
                Mailbox::start_with_icloud_examine_compatibility(credentials, shutdown.clone())
            } else {
                Mailbox::start(credentials, shutdown.clone())
            };
            let app = application(
                mailbox,
                ServerOptions {
                    bind,
                    allowed_hosts: allowed_host,
                    allowed_origins: allowed_origin,
                },
                shutdown.clone(),
            );
            let listener = tokio::net::TcpListener::bind((bind, port)).await?;
            let stop = shutdown.clone();
            tokio::spawn(async move {
                #[cfg(unix)]
                {
                    let mut term =
                        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                            .expect("install termination handler");
                    tokio::select! {_=tokio::signal::ctrl_c()=>{},_=term.recv()=>{}}
                }
                #[cfg(not(unix))]
                {
                    let _ = tokio::signal::ctrl_c().await;
                }
                stop.cancel();
            });
            tracing::info!(target:"icloud_mail_mcp::ops","read-only mail service ready");
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown.cancelled_owned())
                .await?;
        }
    }
    Ok(())
}
