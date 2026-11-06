//! Intentionally does not install EnvFilter: RUST_LOG cannot enable protocol payloads.
use tracing_subscriber::{Layer, layer::SubscriberExt, util::SubscriberInitExt};
pub fn subscriber<W>(writer: W) -> impl tracing::Subscriber + Send + Sync
where
    W: for<'a> tracing_subscriber::fmt::MakeWriter<'a> + Send + Sync + 'static,
{
    tracing_subscriber::registry().with(
        tracing_subscriber::fmt::layer()
            .with_writer(writer)
            .with_filter(tracing_subscriber::filter::filter_fn(|m| {
                m.target() == "icloud_mail_mcp::ops"
            })),
    )
}
pub fn init() -> anyhow::Result<()> {
    subscriber(std::io::stderr)
        .try_init()
        .map_err(|_| anyhow::anyhow!("cannot install mandatory safe logging"))?;
    std::panic::set_hook(Box::new(|_| {
        eprintln!("mail service internal failure (details suppressed)");
    }));
    Ok(())
}
