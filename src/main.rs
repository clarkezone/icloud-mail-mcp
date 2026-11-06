use clap::{Parser, Subcommand};
use icloud_mail_mcp::{
    credentials::Credentials,
    logging,
    sending::{SendSettings, SubmissionService},
    server::{ServerOptions, application_with_sending},
    worker::Mailbox,
};
use std::{net::IpAddr, path::PathBuf};
use tokio_util::sync::CancellationToken;

#[derive(Parser)]
#[command(version, about = "iCloud Mail MCP bridge with optional sending")]
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
    /// Store the fixed From/SMTP identity privately using hidden input.
    ConfigureSending {
        #[arg(long)]
        config: PathBuf,
    },
    /// Start the MCP service; health checks do not authenticate to iCloud.
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
        /// Opt in to SMTP tools using a private sender configuration file.
        #[arg(long)]
        sending_config: Option<PathBuf>,
    },
    /// Print the exact MCP tool schemas without touching an account.
    Schemas {
        #[arg(long)]
        with_sending: bool,
    },
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
        Command::Schemas { with_sending } => println!(
            "{}",
            serde_json::to_string_pretty(&if with_sending {
                icloud_mail_mcp::server::tools_with_sending()
            } else {
                icloud_mail_mcp::server::tools()
            })?
        ),
        Command::Configure { credentials } => {
            let username = rpassword::prompt_password("iCloud login (hidden): ")?;
            let password = rpassword::prompt_password("App-specific password (hidden): ")?;
            Credentials::new(username, password)?.save(&credentials)?;
            println!("Credentials saved in private local storage.");
        }
        Command::ConfigureSending { config } => {
            let sender = rpassword::prompt_password("From address (hidden): ")?;
            let username =
                rpassword::prompt_password("SMTP login (hidden; Enter to use From address): ")?;
            let username = if username.is_empty() {
                sender.clone()
            } else {
                username
            };
            let settings = SendSettings::new(sender, username)?;
            SubmissionService::initialize_journal(
                &settings,
                &config.with_file_name("send-journal.sqlite3"),
            )?;
            settings.save(&config)?;
            println!("Private sending configuration saved. No email was sent.");
        }
        Command::Serve {
            credentials,
            bind,
            port,
            allowed_host,
            allowed_origin,
            allow_icloud_examine_read_write,
            sending_config,
        } => {
            let credentials = Credentials::load(&credentials)?;
            let shutdown = CancellationToken::new();
            let sending = if let Some(path) = sending_config {
                let settings = SendSettings::load(&path)?;
                Some(
                    SubmissionService::start(
                        credentials.clone(),
                        settings,
                        path.with_file_name("send-journal.sqlite3"),
                        shutdown.clone(),
                    )
                    .await?,
                )
            } else {
                None
            };
            let mailbox = if allow_icloud_examine_read_write {
                tracing::warn!(target: "icloud_mail_mcp::ops", "Explicit iCloud EXAMINE compatibility enabled; command allowlist and PEEK restrictions remain active");
                Mailbox::start_with_icloud_examine_compatibility(credentials, shutdown.clone())
            } else {
                Mailbox::start(credentials, shutdown.clone())
            };
            let app = application_with_sending(
                mailbox,
                sending,
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
            tracing::info!(target:"icloud_mail_mcp::ops","mail service ready");
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown.cancelled_owned())
                .await?;
        }
    }
    Ok(())
}
