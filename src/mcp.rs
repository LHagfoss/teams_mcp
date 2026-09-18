use std::sync::Arc;

use anyhow::Result;
use rmcp::{
    ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::CallToolResult,
    schemars::JsonSchema,
    tool, tool_handler, tool_router,
    transport::stdio,
};
use serde::{Deserialize, Serialize};

use crate::{browser, data};

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct TeamsServer {
    tool_router: ToolRouter<Self>,
    _marker: Arc<()>,
}

impl Default for TeamsServer {
    fn default() -> Self {
        Self {
            tool_router: Self::tool_router(),
            _marker: Arc::new(()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TeamChannelsRequest {
    /// Optional exact visible team label. Omit to list channels currently visible in Teams.
    #[serde(default)]
    pub team_name: Option<String>,
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
        description = "List visible Microsoft Teams labels from the current navigation. Results are UI labels, not Graph IDs. Read-only.",
        annotations(read_only_hint = true)
    )]
    async fn list_teams(&self) -> CallToolResult {
        match browser::list_teams().await {
            Ok(value) => json_success(&value),
            Err(error) => json_error(format!("Could not list Teams: {error:#}")),
        }
    }

    /// List channel labels under a visible team.
    #[tool(
        name = "list_channels",
        description = "List visible channel labels, optionally after selecting an exact visible team label. Read-only; UI labels are not Graph IDs.",
        annotations(read_only_hint = true)
    )]
    async fn list_channels(
        &self,
        Parameters(request): Parameters<TeamChannelsRequest>,
    ) -> CallToolResult {
        match browser::list_channels(request.team_name.as_deref()).await {
            Ok(value) => json_success(&value),
            Err(error) => json_error(format!("Could not list Teams channels: {error:#}")),
        }
    }

    /// List chat labels currently rendered in the visible Teams navigation.
    #[tool(
        name = "list_chats",
        description = "List visible Microsoft Teams chat labels. Read-only; labels are UI references, not Graph IDs.",
        annotations(read_only_hint = true)
    )]
    async fn list_chats(&self) -> CallToolResult {
        match browser::list_chats().await {
            Ok(value) => json_success(&value),
            Err(error) => json_error(format!("Could not list Teams chats: {error:#}")),
        }
    }

    /// List visible members associated with a selected chat.
    #[tool(
        name = "list_chat_members",
        description = "Click an exact visible chat label and read member or participant metadata exposed by the current Teams UI. Read-only.",
        annotations(read_only_hint = true)
    )]
    async fn list_chat_members(
        &self,
        Parameters(request): Parameters<ChatMembersRequest>,
    ) -> CallToolResult {
        match browser::list_chat_members(&request.chat_name).await {
            Ok(value) => json_success(&value),
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

    /// Return the low-level normalized page shape used by the adapter.
    #[tool(
        name = "inspect_teams_page",
        description = "Read bounded visible Teams page text for adapter diagnostics. Read-only.",
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

#[tool_handler]
impl rmcp::ServerHandler for TeamsServer {}

#[cfg(test)]
mod tests {
    use super::TeamsServer;

    /// Every tool is a read-only UI inspection: selecting a chat only moves
    /// ephemeral UI focus and never persists external state. The annotation
    /// is what lets MCP clients (e.g. rustcode) batch these calls instead of
    /// serializing them one per model round.
    #[test]
    fn all_tools_advertise_read_only_hint() {
        let tools = TeamsServer::tool_router().list_all();
        assert_eq!(tools.len(), 7, "expected 7 teams tools, got {}", tools.len());
        for tool in &tools {
            assert_eq!(
                tool.annotations.as_ref().and_then(|a| a.read_only_hint),
                Some(true),
                "tool {} must set read_only_hint",
                tool.name
            );
        }
    }
}

pub async fn serve() -> Result<()> {
    let server = TeamsServer::default();
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
