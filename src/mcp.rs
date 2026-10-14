use std::sync::Arc;

use anyhow::Result;
use rmcp::{
    ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, Implementation, ServerCapabilities, ServerConfig},
    schemars::JsonSchema,
    tool, tool_handler, tool_router,
    transport::stdio,
};
use serde::{Deserialize, Serialize};

use crate::{browser, data};

/// Env var that exposes developer diagnostics such as `inspect_teams_page`.
const DEBUG_TOOLS_ENV: &str = "TEAMS_MCP_DEBUG_TOOLS";
const DEBUG_TOOLS: &[&str] = &["inspect_teams_page"];
const DEFAULT_LIST_PAGE_SIZE: u32 = 50;
const MAX_LIST_PAGE_SIZE: u32 = 200;

const INSTRUCTIONS: &str = "\
Microsoft Teams via the user's logged-in web UI (persistent local browser profile). \
Names are exact visible UI labels, not Graph IDs: call list_chats / list_teams / list_channels first and \
reuse labels verbatim. List tools page with page/page_size (default 50); check has_more.\n\
Reading: get_chat_messages page 1 is newest; higher pages are older.\n\
Writing (send_chat_message, reply_to_chat_message) sends real messages as the user:\n\
- Call without confirm first to get a preview (no browser opened); set confirm=true only when the user \
clearly asked to send that exact text to that chat.\n\
- Messages are single-line.\n\
- To answer a specific message use reply_to_chat_message with message_id from get_chat_messages.\n\
- If a reply fails (REPLY_NOT_SENT) nothing was sent. Do NOT fall back to send_chat_message; ask the user. \
allow_standalone_after_reply_failure=true is only for when the user explicitly asks for a new top-level message.\n\
Errors are returned as tool results with isError=true; report them instead of retrying blindly.";

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct TeamsServer {
    tool_router: ToolRouter<Self>,
    _marker: Arc<()>,
}

impl Default for TeamsServer {
    fn default() -> Self {
        Self::with_debug_tools(debug_tools_enabled())
    }
}

fn debug_tools_enabled() -> bool {
    std::env::var(DEBUG_TOOLS_ENV)
        .map(|value| matches!(value.trim(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

impl TeamsServer {
    fn with_debug_tools(debug_tools: bool) -> Self {
        let mut tool_router = Self::tool_router();
        if !debug_tools {
            for name in DEBUG_TOOLS {
                tool_router.remove_route(name);
            }
        }
        Self {
            tool_router,
            _marker: Arc::new(()),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ListPageRequest {
    /// 1-based page. Defaults to 1.
    #[serde(default)]
    pub page: Option<u32>,
    /// Items per page. Defaults to 50, max 200.
    #[serde(default)]
    pub page_size: Option<u32>,
}

/// Slice an already-collected list result into the requested page.
fn paginate(
    mut result: data::ItemsResult,
    page: Option<u32>,
    page_size: Option<u32>,
) -> data::ItemsResult {
    let page = page.unwrap_or(1).max(1);
    let page_size = page_size
        .unwrap_or(DEFAULT_LIST_PAGE_SIZE)
        .clamp(1, MAX_LIST_PAGE_SIZE);
    let start = ((page - 1) as usize).saturating_mul(page_size as usize);
    let total = result.items.len();
    result.items = result
        .items
        .into_iter()
        .skip(start)
        .take(page_size as usize)
        .collect();
    result.page = page;
    result.page_size = page_size;
    result.has_more = result.has_more || start.saturating_add(page_size as usize) < total;
    result
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TeamChannelsRequest {
    /// Optional exact visible team label. Omit to list channels currently visible in Teams.
    #[serde(default)]
    pub team_name: Option<String>,
    /// 1-based page. Defaults to 1.
    #[serde(default)]
    pub page: Option<u32>,
    /// Items per page. Defaults to 50, max 200.
    #[serde(default)]
    pub page_size: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ChatMessagesRequest {
    /// Exact visible chat label.
    pub chat_name: String,
    /// 1-based page, where page 1 contains the latest messages and higher pages move older.
    #[serde(default)]
    pub page: Option<u32>,
    /// Messages per page. Defaults to 10 and is capped at 100.
    #[serde(default)]
    pub page_size: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ChatMembersRequest {
    /// Exact visible chat label.
    pub chat_name: String,
    /// 1-based page. Defaults to 1.
    #[serde(default)]
    pub page: Option<u32>,
    /// Items per page. Defaults to 50, max 200.
    #[serde(default)]
    pub page_size: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SendChatMessageRequest {
    /// Exact visible chat label.
    pub chat_name: String,
    /// A single-line message to send.
    pub message: String,
    /// Required to perform the external send. Omit or set false for a preview.
    #[serde(default)]
    pub confirm: bool,
    /// Set true only when the user explicitly wants a new top-level message after a reply attempt failed.
    #[serde(default)]
    pub allow_standalone_after_reply_failure: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ReplyToChatMessageRequest {
    /// Exact visible chat label.
    pub chat_name: String,
    /// The message_id returned by get_chat_messages.
    pub message_id: String,
    /// A single-line message to send as a reply.
    pub message: String,
    /// Required to perform the external send. Omit or set false for a preview.
    #[serde(default)]
    pub confirm: bool,
}

fn json_success<T: Serialize>(value: &T) -> CallToolResult {
    match serde_json::to_string_pretty(value) {
        Ok(text) => CallToolResult::success(vec![rmcp::model::ContentBlock::text(text)]),
        Err(error) => json_error(format!("could not serialize tool result: {error}")),
    }
}

fn json_error(error: impl std::fmt::Display) -> CallToolResult {
    CallToolResult::error(vec![rmcp::model::ContentBlock::text(error.to_string())])
}

#[tool_router]
impl TeamsServer {
    /// Return a bounded snapshot of the authenticated Teams page.
    #[tool(
        name = "get_teams_overview",
        description = "Read the current visible Microsoft Teams page. Read-only; visible text is bounded and no credentials are returned.",
        annotations(read_only_hint = true)
    )]
    async fn get_teams_overview(&self) -> CallToolResult {
        match browser::visible_snapshot().await {
            Ok(value) => json_success(&value),
            Err(error) => json_error(format!("Could not read Teams overview: {error:#}")),
        }
    }

    /// List team labels currently exposed in the visible Teams navigation.
    #[tool(
        name = "list_teams",
        description = "List visible Microsoft Teams team labels (UI labels, not Graph IDs). Paginated.",
        annotations(read_only_hint = true)
    )]
    async fn list_teams(&self, Parameters(request): Parameters<ListPageRequest>) -> CallToolResult {
        match browser::list_teams().await {
            Ok(value) => json_success(&paginate(value, request.page, request.page_size)),
            Err(error) => json_error(format!("Could not list Teams: {error:#}")),
        }
    }

    /// List channel labels under a visible team.
    #[tool(
        name = "list_channels",
        description = "List visible channel labels, optionally for an exact visible team label. Paginated.",
        annotations(read_only_hint = true)
    )]
    async fn list_channels(
        &self,
        Parameters(request): Parameters<TeamChannelsRequest>,
    ) -> CallToolResult {
        match browser::list_channels(request.team_name.as_deref()).await {
            Ok(value) => json_success(&paginate(value, request.page, request.page_size)),
            Err(error) => json_error(format!("Could not list Teams channels: {error:#}")),
        }
    }

    /// List chat labels currently rendered in the visible Teams navigation.
    #[tool(
        name = "list_chats",
        description = "List visible Microsoft Teams chat labels (UI labels, not Graph IDs). Paginated.",
        annotations(read_only_hint = true)
    )]
    async fn list_chats(&self, Parameters(request): Parameters<ListPageRequest>) -> CallToolResult {
        match browser::list_chats().await {
            Ok(value) => json_success(&paginate(value, request.page, request.page_size)),
            Err(error) => json_error(format!("Could not list Teams chats: {error:#}")),
        }
    }

    /// List visible members associated with a selected chat.
    #[tool(
        name = "list_chat_members",
        description = "List members of an exact visible chat label, as exposed by the Teams UI. Paginated.",
        annotations(read_only_hint = true)
    )]
    async fn list_chat_members(
        &self,
        Parameters(request): Parameters<ChatMembersRequest>,
    ) -> CallToolResult {
        match browser::list_chat_members(&request.chat_name).await {
            Ok(value) => json_success(&paginate(value, request.page, request.page_size)),
            Err(error) => json_error(format!("Could not read Teams chat members: {error:#}")),
        }
    }

    /// Read paginated messages after selecting a visible chat.
    #[tool(
        name = "get_chat_messages",
        description = "Read visible Teams chat messages with page-based pagination. Page 1 is newest; larger pages scroll toward older messages. This uses the visible UI only.",
        annotations(read_only_hint = true)
    )]
    async fn get_chat_messages(
        &self,
        Parameters(request): Parameters<ChatMessagesRequest>,
    ) -> CallToolResult {
        let page = request.page.unwrap_or(1).max(1);
        let page_size = request.page_size.unwrap_or(10).clamp(1, 100);
        match browser::visible_messages(&request.chat_name, page, page_size).await {
            Ok(value) => json_success(&value),
            Err(error) => json_error(format!("Could not read Teams messages: {error:#}")),
        }
    }

    /// Preview or send a message to a visible chat.
    #[tool(
        name = "send_chat_message",
        description = "Send (or preview, if confirm is false) a single-line top-level message to an exact visible Teams chat. Not a reply; never a fallback for a failed reply_to_chat_message. Writes as the user.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false
        )
    )]
    async fn send_chat_message(
        &self,
        Parameters(request): Parameters<SendChatMessageRequest>,
    ) -> CallToolResult {
        match browser::send_chat_message(
            &request.chat_name,
            &request.message,
            request.confirm,
            request.allow_standalone_after_reply_failure,
        )
        .await
        {
            Ok(value) => json_success(&value),
            Err(error) => json_error(format!("Could not send Teams chat message: {error:#}")),
        }
    }

    /// Preview or reply to a visible message in a chat.
    #[tool(
        name = "reply_to_chat_message",
        description = "Reply (or preview, if confirm is false) to a visible Teams message, using message_id from get_chat_messages. On failure nothing was sent. Writes as the user.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false
        )
    )]
    async fn reply_to_chat_message(
        &self,
        Parameters(request): Parameters<ReplyToChatMessageRequest>,
    ) -> CallToolResult {
        match browser::reply_to_chat_message(
            &request.chat_name,
            &request.message_id,
            &request.message,
            request.confirm,
        )
        .await
        {
            Ok(value) => json_success(&value),
            Err(error) => json_error(format!(
                "REPLY_NOT_SENT: Could not reply to Teams chat message: {error:#}. Do not send the same text as a standalone message unless the user explicitly requests that change."
            )),
        }
    }

    /// Return the low-level normalized page shape used by the adapter.
    #[tool(
        name = "inspect_teams_page",
        description = "Developer diagnostic: bounded visible Teams page text. Only exposed when TEAMS_MCP_DEBUG_TOOLS=1.",
        annotations(read_only_hint = true)
    )]
    async fn inspect_teams_page(&self) -> CallToolResult {
        match browser::visible_snapshot().await {
            Ok(value) => json_success(&data::PageSnapshot {
                visible_text: value.visible_text,
                ..value
            }),
            Err(error) => json_error(format!("Could not inspect Teams page: {error:#}")),
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl rmcp::ServerHandler for TeamsServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(
                Implementation::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"))
                    .with_title("Microsoft Teams"),
            )
            .with_instructions(INSTRUCTIONS)
    }
}

pub async fn serve() -> Result<()> {
    let server = TeamsServer::default();
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{TeamsServer, paginate};
    use crate::data::{ItemsResult, UiItem};
    use rmcp::ServerHandler;

    #[test]
    fn debug_tools_hidden_by_default() {
        let hidden = TeamsServer::with_debug_tools(false);
        assert!(!hidden.tool_router.has_route("inspect_teams_page"));
        assert!(hidden.tool_router.has_route("send_chat_message"));
        let shown = TeamsServer::with_debug_tools(true);
        assert!(shown.tool_router.has_route("inspect_teams_page"));
    }

    #[test]
    fn server_info_names_server_and_has_instructions() {
        let info = TeamsServer::with_debug_tools(false).get_info();
        assert_eq!(info.server_info.name, "teams-mcp");
        assert!(info.instructions.unwrap_or_default().contains("confirm"));
    }

    #[test]
    fn tool_descriptions_stay_short() {
        for tool in TeamsServer::tool_router().list_all() {
            let len = tool.description.as_deref().unwrap_or_default().len();
            assert!(len <= 200, "{} description is {len} chars", tool.name);
        }
    }

    #[test]
    fn paginate_slices_items() {
        let items = (0..5)
            .map(|i| {
                serde_json::from_value::<UiItem>(serde_json::json!({ "label": format!("c{i}") }))
            })
            .collect::<Result<Vec<_>, _>>()
            .expect("UiItem from label");
        let result = ItemsResult {
            page_url: String::new(),
            page: 1,
            page_size: 5,
            has_more: false,
            items,
            warnings: Vec::new(),
        };
        let page = paginate(result.clone(), Some(2), Some(2));
        assert_eq!(page.items.len(), 2);
        assert!(page.has_more);
        let last = paginate(result, Some(3), Some(2));
        assert_eq!(last.items.len(), 1);
        assert!(!last.has_more);
    }

    /// Read tools advertise their read-only hint so MCP clients can batch them.
    #[test]
    fn all_tools_advertise_read_only_hint() {
        let tools = TeamsServer::tool_router().list_all();
        assert_eq!(
            tools.len(),
            9,
            "expected 9 teams tools, got {}",
            tools.len()
        );
        for tool in &tools {
            let read_only = tool.annotations.as_ref().and_then(|a| a.read_only_hint);
            if matches!(
                tool.name.as_ref(),
                "send_chat_message" | "reply_to_chat_message"
            ) {
                assert_eq!(read_only, Some(false));
            } else {
                assert_eq!(
                    read_only,
                    Some(true),
                    "tool {} must set read_only_hint",
                    tool.name
                );
            }
        }
    }
}
