use crate::agents::extension::PlatformExtensionContext;
use crate::agents::mcp_client::{Error, McpClientTrait};
use crate::agents::tool_execution::ToolCallContext;
use crate::config::paths::Paths;
use crate::harness::{self, MemoryKind, MemoryStore, NewRecord};
use anyhow::Result;
use async_trait::async_trait;
use indoc::indoc;
use rmcp::model::{
    CallToolResult, ContentBlock, Implementation, InitializeResult, JsonObject, ListToolsResult,
    ServerCapabilities, Tool, ToolAnnotations,
};
use schemars::{schema_for, JsonSchema};
use serde::{Deserialize, Serialize};
use std::str::FromStr;
use tokio_util::sync::CancellationToken;

pub static EXTENSION_NAME: &str = "memory";
pub const MEMORY_WRITE_TOOL_NAME: &str = "memory_write";
pub const MEMORY_SEARCH_TOOL_NAME: &str = "memory_search";
pub const MEMORY_BRIEF_TOOL_NAME: &str = "memory_brief";

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct MemoryWriteParams {
    /// Record kind: fact, preference, person, or decision.
    kind: String,
    /// Optional subject for updatable facts (e.g. "timezone"). Writing
    /// again with the same kind and subject supersedes the old record.
    subject: Option<String>,
    /// The durable statement to remember.
    content: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct MemorySearchParams {
    /// Substring to search for in active memory records.
    query: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct MemoryBriefParams {}

pub struct MemoryClient {
    info: InitializeResult,
    #[allow(dead_code)]
    context: PlatformExtensionContext,
}

impl MemoryClient {
    pub fn new(context: PlatformExtensionContext) -> Result<Self> {
        let info = InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(
                Implementation::new(EXTENSION_NAME.to_string(), "1.0.0".to_string())
                    .with_title("Memory"),
            )
            .with_instructions(
                indoc! {r#"
                Long-term memory persists across sessions. A brief of the
                current memory is already in your system prompt; use
                memory_write to store durable facts, preferences, people,
                and standing decisions the user will expect you to know
                next time. Give updatable facts a subject so a later write
                supersedes the old value. Never store credentials or
                secrets — the store refuses them. Do not store transient
                conversation detail.
            "#}
                .to_string(),
            );

        Ok(Self { info, context })
    }

    async fn open_store() -> Result<MemoryStore, String> {
        let config_path = Paths::in_config_dir("config.yaml");
        let yaml = std::fs::read_to_string(&config_path).unwrap_or_default();
        let config = harness::config::parse_harness_config(&yaml)
            .map_err(|e| format!("invalid harness config: {e}"))?;
        if !config.memory.enabled {
            return Err("harness memory is disabled".to_string());
        }
        MemoryStore::open(&Paths::in_config_dir("harness.db"))
            .await
            .map_err(|e| format!("failed to open memory store: {e}"))
    }

    fn format_record(record: &harness::MemoryRecord) -> String {
        match &record.subject {
            Some(subject) => format!("[{}] {}: {}", record.kind.as_str(), subject, record.content),
            None => format!("[{}] {}", record.kind.as_str(), record.content),
        }
    }

    async fn handle_write(
        &self,
        session_id: &str,
        arguments: Option<JsonObject>,
    ) -> Result<Vec<ContentBlock>, String> {
        let args = arguments.ok_or("Missing arguments")?;
        let kind = args
            .get("kind")
            .and_then(|v| v.as_str())
            .ok_or("Missing required parameter: kind")?;
        let kind = MemoryKind::from_str(kind).map_err(|e| e.to_string())?;
        let subject = args
            .get("subject")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let content = args
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or("Missing required parameter: content")?
            .to_string();
        let store = Self::open_store().await?;
        let record = store
            .write(
                NewRecord {
                    kind,
                    subject,
                    content,
                    source: format!("session:{session_id}"),
                },
                false,
            )
            .await
            .map_err(|e| e.to_string());
        store.close().await;
        let record = record?;
        let note = match &record.supersedes {
            Some(old) => format!("Stored memory {} (supersedes {})", record.id, old),
            None => format!("Stored memory {}", record.id),
        };
        Ok(vec![ContentBlock::text(note)])
    }

    async fn handle_search(
        &self,
        arguments: Option<JsonObject>,
    ) -> Result<Vec<ContentBlock>, String> {
        let args = arguments.ok_or("Missing arguments")?;
        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or("Missing required parameter: query")?;
        let store = Self::open_store().await?;
        let records = store.search(query).await.map_err(|e| e.to_string());
        store.close().await;
        let records = records?;
        if records.is_empty() {
            return Ok(vec![ContentBlock::text("No matching memories.")]);
        }
        let text = records
            .iter()
            .map(Self::format_record)
            .collect::<Vec<_>>()
            .join("\n");
        Ok(vec![ContentBlock::text(text)])
    }

    async fn handle_brief(&self) -> Result<Vec<ContentBlock>, String> {
        let config_path = Paths::in_config_dir("config.yaml");
        let yaml = std::fs::read_to_string(&config_path).unwrap_or_default();
        let config = harness::config::parse_harness_config(&yaml)
            .map_err(|e| format!("invalid harness config: {e}"))?;
        let store = Self::open_store().await?;
        let brief = store
            .build_brief(config.memory.brief_max_tokens as usize)
            .await
            .map_err(|e| e.to_string());
        store.close().await;
        let brief = brief?;
        Ok(vec![ContentBlock::text(if brief.is_empty() {
            "(memory is empty)".to_string()
        } else {
            brief
        })])
    }

    fn get_tools() -> Vec<Tool> {
        let write_schema = serde_json::to_value(schema_for!(MemoryWriteParams))
            .expect("Failed to serialize MemoryWriteParams schema");
        let search_schema = serde_json::to_value(schema_for!(MemorySearchParams))
            .expect("Failed to serialize MemorySearchParams schema");
        let brief_schema = serde_json::to_value(schema_for!(MemoryBriefParams))
            .expect("Failed to serialize MemoryBriefParams schema");

        vec![
            Tool::new(
                MEMORY_WRITE_TOOL_NAME.to_string(),
                "Store a durable fact, preference, person note, or standing decision in long-term memory."
                    .to_string(),
                write_schema.as_object().unwrap().clone(),
            )
            .annotate(ToolAnnotations::from_raw(
                Some("Write Memory".to_string()),
                Some(true),
                Some(false),
                Some(false),
                Some(false),
            )),
            Tool::new(
                MEMORY_SEARCH_TOOL_NAME.to_string(),
                "Search active long-term memory records by substring.".to_string(),
                search_schema.as_object().unwrap().clone(),
            )
            .annotate(ToolAnnotations::from_raw(
                Some("Search Memory".to_string()),
                Some(true),
                Some(false),
                Some(true),
                Some(true),
            )),
            Tool::new(
                MEMORY_BRIEF_TOOL_NAME.to_string(),
                "Return the current long-term memory brief.".to_string(),
                brief_schema.as_object().unwrap().clone(),
            )
            .annotate(ToolAnnotations::from_raw(
                Some("Memory Brief".to_string()),
                Some(true),
                Some(false),
                Some(true),
                Some(true),
            )),
        ]
    }
}

#[async_trait]
impl McpClientTrait for MemoryClient {
    async fn list_tools(
        &self,
        _session_id: &str,
        _next_cursor: Option<String>,
        _cancellation_token: CancellationToken,
    ) -> Result<ListToolsResult, Error> {
        Ok(ListToolsResult {
            tools: Self::get_tools(),
            next_cursor: None,
            meta: None,
            ..Default::default()
        })
    }

    async fn call_tool(
        &self,
        ctx: &ToolCallContext,
        name: &str,
        arguments: Option<JsonObject>,
        _cancellation_token: CancellationToken,
    ) -> Result<CallToolResult, Error> {
        let content = match name {
            MEMORY_WRITE_TOOL_NAME => self.handle_write(&ctx.session_id, arguments).await,
            MEMORY_SEARCH_TOOL_NAME => self.handle_search(arguments).await,
            MEMORY_BRIEF_TOOL_NAME => self.handle_brief().await,
            _ => Err(format!("Unknown tool: {}", name)),
        };

        match content {
            Ok(content) => Ok(CallToolResult::success(content)),
            Err(error) => Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "Error: {}",
                error
            ))])),
        }
    }

    fn get_info(&self) -> Option<&InitializeResult> {
        Some(&self.info)
    }
}
