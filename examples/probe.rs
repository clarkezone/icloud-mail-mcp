use anyhow::{Context, ensure};
use rmcp::{
    ServiceExt,
    model::{CallToolRequestParams, ClientConfig},
    transport::StreamableHttpClientTransport,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let url = args
        .next()
        .context("usage: cargo run --example probe -- http://host:port/mcp [--with-sending]")?;
    let option = args.next();
    ensure!(
        option.as_deref().is_none_or(|s| s == "--with-sending") && args.next().is_none(),
        "unsupported probe option"
    );
    let with_sending = option.is_some();
    tokio::time::timeout(std::time::Duration::from_secs(35), async move {
        let client = ClientConfig::default()
            .serve(StreamableHttpClientTransport::from_uri(url)).await
            .context("MCP initialization failed")?;
        let tools = client.list_all_tools().await?;
        let mut names = vec!["list_folders", "list_messages", "search_messages", "get_message"];
        if with_sending { names.extend(["send_message", "get_send_status"]); }
        ensure!(tools.len()==names.len() && names.iter().all(|name|tools.iter().any(|t|t.name==*name)), "unexpected mail tool inventory");
        let result = client.call_tool(CallToolRequestParams::new("list_folders").with_arguments(serde_json::Map::new())).await?;
        let success = result.is_error != Some(true);
        client.cancel().await?;
        ensure!(success, "read-only folder check failed; inspect sanitized structured errors with your MCP client");
        println!("PASS: requested tools discovered and folder enumeration succeeded (mailbox data omitted; no SMTP send).");
        anyhow::Ok(())
    }).await.context("probe deadline exceeded")??;
    Ok(())
}
