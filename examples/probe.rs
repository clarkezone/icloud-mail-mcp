use anyhow::{Context, ensure};
use rmcp::{
    ServiceExt,
    model::{CallToolRequestParams, ClientConfig},
    transport::StreamableHttpClientTransport,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let url = std::env::args()
        .nth(1)
        .context("usage: cargo run --example probe -- http://host:port/mcp")?;
    tokio::time::timeout(std::time::Duration::from_secs(35), async move {
        let client = ClientConfig::default().serve(StreamableHttpClientTransport::from_uri(url)).await.context("MCP initialization failed")?;
        let tools = client.list_all_tools().await?;
        ensure!(tools.len() == 4 && ["list_folders", "list_messages", "search_messages", "get_message"].iter().all(|name| tools.iter().any(|t| t.name == *name)), "unexpected mail tool inventory");
        let result = client.call_tool(CallToolRequestParams::new("list_folders").with_arguments(serde_json::Map::new())).await?;
        let success = result.is_error != Some(true);
        client.cancel().await?;
        ensure!(success, "read-only folder check failed; inspect sanitized structured errors with your MCP client");
        println!("PASS: four read-only tools discovered and folder enumeration succeeded (mailbox data omitted).");
        anyhow::Ok(())
    }).await.context("probe deadline exceeded")??;
    Ok(())
}
