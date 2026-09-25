//! Wire-level `resultType` (SEP-2322) checks for results the proxy forwards
//! from the serve hub, across old and new downstream protocol versions.

use super::McpProxyService;
use rmcp::{
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ListToolsResult,
        PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
    },
    service::{RequestContext, RunningService},
    ErrorData as McpError, RoleClient, RoleServer, ServerHandler, ServiceExt,
};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, Lines, WriteHalf};

struct StubHub;

impl ServerHandler for StubHub {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _cx: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(vec![Tool::new(
            "search",
            "stub search",
            serde_json::Map::new(),
        )]))
    }

    async fn call_tool(
        &self,
        _request: CallToolRequestParams,
        _cx: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        Ok(CallToolResult::success(vec![ContentBlock::text("ok")]).into())
    }
}

struct Downstream {
    lines: Lines<BufReader<tokio::io::ReadHalf<DuplexStream>>>,
    writer: WriteHalf<DuplexStream>,
    _upstream: RunningService<RoleClient, ()>,
}

impl Downstream {
    /// Stub hub <-> proxy (legacy upstream handshake, as in production) <-> raw JSON-RPC.
    async fn connect() -> Self {
        let (hub_io, upstream_io) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            let hub = StubHub.serve(hub_io).await.expect("hub handshake");
            let _ = hub.waiting().await;
        });
        let upstream = ().serve(upstream_io).await.expect("upstream handshake");
        let proxy = McpProxyService::new(upstream.peer().clone());

        let (proxy_io, client_io) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            if let Ok(running) = proxy.serve(proxy_io).await {
                let _ = running.waiting().await;
            }
        });
        let (reader, writer) = tokio::io::split(client_io);
        Self {
            lines: BufReader::new(reader).lines(),
            writer,
            _upstream: upstream,
        }
    }

    async fn send(&mut self, message: Value) {
        let mut line = message.to_string();
        line.push('\n');
        self.writer.write_all(line.as_bytes()).await.unwrap();
    }

    async fn response(&mut self, id: u64) -> Value {
        let wait = async {
            while let Some(line) = self.lines.next_line().await.unwrap() {
                let message: Value = serde_json::from_str(&line).unwrap();
                if message["id"] == json!(id) {
                    return message;
                }
            }
            panic!("proxy closed before responding to id {id}");
        };
        tokio::time::timeout(std::time::Duration::from_secs(10), wait)
            .await
            .expect("proxy response timed out")
    }

    async fn request(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        let response = self.response(id).await;
        assert!(
            response.get("error").is_none(),
            "{method} failed: {response}"
        );
        response["result"].clone()
    }

    async fn initialize_legacy(&mut self, protocol_version: &str) {
        let result = self
            .request(
                1,
                "initialize",
                json!({
                    "protocolVersion": protocol_version,
                    "capabilities": {},
                    "clientInfo": {"name": "legacy-test", "version": "1"},
                }),
            )
            .await;
        assert_eq!(result["protocolVersion"], protocol_version);
        self.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .await;
    }
}

fn stateless_meta() -> Value {
    json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientInfo": {"name": "stateless-test", "version": "1"},
        "io.modelcontextprotocol/clientCapabilities": {},
    })
}

fn call_search_params() -> Value {
    json!({"name": "search", "arguments": {"query": "prometheus"}})
}

#[tokio::test]
async fn stateless_2026_07_28_client_gets_complete_result_type() {
    let mut client = Downstream::connect().await;

    let tools = client
        .request(2, "tools/list", json!({"_meta": stateless_meta()}))
        .await;
    assert_eq!(tools["resultType"], "complete", "tools/list: {tools}");
    assert_eq!(tools["tools"][0]["name"], "search");

    let mut params = call_search_params();
    params["_meta"] = stateless_meta();
    let call = client.request(3, "tools/call", params).await;
    assert_eq!(call["resultType"], "complete", "tools/call: {call}");
}

#[tokio::test]
async fn legacy_clients_keep_the_pre_2026_wire_shape() {
    for protocol_version in ["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"] {
        let mut client = Downstream::connect().await;
        client.initialize_legacy(protocol_version).await;

        let tools = client.request(2, "tools/list", json!({})).await;
        assert!(
            tools.get("resultType").is_none(),
            "{protocol_version} tools/list: {tools}"
        );
        assert_eq!(tools["tools"][0]["name"], "search");

        let call = client.request(3, "tools/call", call_search_params()).await;
        assert!(
            call.get("resultType").is_none(),
            "{protocol_version} tools/call: {call}"
        );
    }
}
