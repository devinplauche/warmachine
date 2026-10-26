use crate::agents::extension::PlatformExtensionContext;
use crate::agents::mcp_client::{Error, McpClientTrait};
use crate::agents::tool_execution::ToolCallContext;
use crate::config::paths::Paths;
use crate::harness::{NewTask, TaskStatus, TaskStore};
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

pub static EXTENSION_NAME: &str = "tasks";
pub const TASK_CREATE_TOOL_NAME: &str = "task_create";
pub const TASK_LIST_TOOL_NAME: &str = "task_list";
pub const TASK_COMPLETE_TOOL_NAME: &str = "task_complete";
pub const TASK_FAIL_TOOL_NAME: &str = "task_fail";
pub const TASK_NOTE_TOOL_NAME: &str = "task_note";

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct TaskCreateParams {
    /// Short task title.
    title: String,
    /// Optional next step.
    next_step: Option<String>,
    /// Optional deadline: RFC 3339 or YYYY-MM-DD.
    deadline: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct TaskListParams {
    /// Optional status filter: open, in_progress, failed, done, delivered, cancelled.
    status: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct TaskCompleteParams {
    /// Task id.
    id: String,
    /// The outcome / result text.
    result: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct TaskFailParams {
    /// Task id.
    id: String,
    /// Why it failed.
    reason: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct TaskNoteParams {
    /// Task id.
    id: String,
    /// Note to append to the task's history.
    note: String,
}

pub struct TasksClient {
    info: InitializeResult,
    #[allow(dead_code)]
    context: PlatformExtensionContext,
}

fn parse_deadline(text: &str) -> Result<chrono::DateTime<chrono::Utc>, String> {
    if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(text) {
        return Ok(parsed.with_timezone(&chrono::Utc));
    }
    let date = chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d")
        .map_err(|_| "deadline must be RFC 3339 or YYYY-MM-DD".to_string())?;
    let end_of_day = date
        .and_hms_opt(23, 59, 59)
        .ok_or("invalid deadline date")?;
    Ok(chrono::DateTime::from_naive_utc_and_offset(
        end_of_day,
        chrono::Utc,
    ))
}

impl TasksClient {
    pub fn new(context: PlatformExtensionContext) -> Result<Self> {
        let info = InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(
                Implementation::new(EXTENSION_NAME.to_string(), "1.0.0".to_string())
                    .with_title("Tasks"),
            )
            .with_instructions(
                indoc! {r#"
                The task ledger is durable across sessions and restarts.
                Create a task for any work that should outlive this
                session or be handed off: background jobs, follow-ups,
                scheduled work. Complete tasks with their result when
                the outcome is known; the result is delivered to the
                user exactly once. If work fails, mark it failed with
                the reason — a failed task stays open and visible, it
                never silently closes.
            "#}
                .to_string(),
            );

        Ok(Self { info, context })
    }

    async fn open_store() -> Result<TaskStore, String> {
        TaskStore::open(&Paths::in_config_dir("harness.db"))
            .await
            .map_err(|e| format!("failed to open task ledger: {e}"))
    }

    fn format_task(task: &crate::harness::Task) -> String {
        let mut line = format!("[{}] {} ({})", task.status.as_str(), task.title, task.id);
        if let Some(deadline) = &task.deadline {
            line.push_str(&format!(" due {}", deadline.format("%Y-%m-%d")));
        }
        if let Some(next) = &task.next_step {
            line.push_str(&format!(" next: {next}"));
        }
        line
    }

    async fn handle_create(
        &self,
        session_id: &str,
        arguments: Option<JsonObject>,
    ) -> Result<Vec<ContentBlock>, String> {
        let args = arguments.ok_or("Missing arguments")?;
        let title = args
            .get("title")
            .and_then(|v| v.as_str())
            .ok_or("Missing required parameter: title")?
            .to_string();
        let next_step = args
            .get("next_step")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let deadline = match args.get("deadline").and_then(|v| v.as_str()) {
            Some(text) => Some(parse_deadline(text)?),
            None => None,
        };
        let owner = format!("session:{session_id}");
        let store = Self::open_store().await?;
        let task = store
            .create(
                NewTask {
                    title,
                    owner,
                    next_step,
                    deadline,
                },
                &format!("session:{session_id}"),
            )
            .await
            .map_err(|e| e.to_string());
        store.close().await;
        let task = task?;
        Ok(vec![ContentBlock::text(format!(
            "Created task {}",
            task.id
        ))])
    }

    async fn handle_list(
        &self,
        arguments: Option<JsonObject>,
    ) -> Result<Vec<ContentBlock>, String> {
        let status = match arguments
            .as_ref()
            .and_then(|args| args.get("status"))
            .and_then(|v| v.as_str())
        {
            Some(text) => Some(TaskStatus::from_str(text).map_err(|e| e.to_string())?),
            None => None,
        };
        let store = Self::open_store().await?;
        let tasks = store
            .list(status, None, false)
            .await
            .map_err(|e| e.to_string());
        store.close().await;
        let tasks = tasks?;
        if tasks.is_empty() {
            return Ok(vec![ContentBlock::text("No tasks.")]);
        }
        let text = tasks
            .iter()
            .map(Self::format_task)
            .collect::<Vec<_>>()
            .join("\n");
        Ok(vec![ContentBlock::text(text)])
    }

    async fn handle_complete(
        &self,
        session_id: &str,
        arguments: Option<JsonObject>,
    ) -> Result<Vec<ContentBlock>, String> {
        let args = arguments.ok_or("Missing arguments")?;
        let id = args
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or("Missing required parameter: id")?;
        let result = args
            .get("result")
            .and_then(|v| v.as_str())
            .ok_or("Missing required parameter: result")?;
        let store = Self::open_store().await?;
        let outcome = store
            .complete(id, result, &format!("session:{session_id}"))
            .await
            .map_err(|e| e.to_string());
        store.close().await;
        outcome?;
        Ok(vec![ContentBlock::text(format!(
            "Task {id} completed; result queued for delivery"
        ))])
    }

    async fn handle_fail(
        &self,
        session_id: &str,
        arguments: Option<JsonObject>,
    ) -> Result<Vec<ContentBlock>, String> {
        let args = arguments.ok_or("Missing arguments")?;
        let id = args
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or("Missing required parameter: id")?;
        let reason = args
            .get("reason")
            .and_then(|v| v.as_str())
            .ok_or("Missing required parameter: reason")?;
        let store = Self::open_store().await?;
        let outcome = store
            .fail(id, reason, &format!("session:{session_id}"))
            .await
            .map_err(|e| e.to_string());
        store.close().await;
        outcome?;
        Ok(vec![ContentBlock::text(format!(
            "Task {id} marked failed (stays open)"
        ))])
    }

    async fn handle_note(
        &self,
        session_id: &str,
        arguments: Option<JsonObject>,
    ) -> Result<Vec<ContentBlock>, String> {
        let args = arguments.ok_or("Missing arguments")?;
        let id = args
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or("Missing required parameter: id")?;
        let note = args
            .get("note")
            .and_then(|v| v.as_str())
            .ok_or("Missing required parameter: note")?;
        let store = Self::open_store().await?;
        let outcome = store
            .add_note(id, note, &format!("session:{session_id}"))
            .await
            .map_err(|e| e.to_string());
        store.close().await;
        outcome?;
        Ok(vec![ContentBlock::text(format!("Noted on task {id}"))])
    }

    fn get_tools() -> Vec<Tool> {
        let create_schema = serde_json::to_value(schema_for!(TaskCreateParams))
            .expect("Failed to serialize TaskCreateParams schema");
        let list_schema = serde_json::to_value(schema_for!(TaskListParams))
            .expect("Failed to serialize TaskListParams schema");
        let complete_schema = serde_json::to_value(schema_for!(TaskCompleteParams))
            .expect("Failed to serialize TaskCompleteParams schema");
        let fail_schema = serde_json::to_value(schema_for!(TaskFailParams))
            .expect("Failed to serialize TaskFailParams schema");
        let note_schema = serde_json::to_value(schema_for!(TaskNoteParams))
            .expect("Failed to serialize TaskNoteParams schema");

        vec![
            Tool::new(
                TASK_CREATE_TOOL_NAME.to_string(),
                "Create a durable task in the ledger (survives sessions and restarts).".to_string(),
                create_schema.as_object().unwrap().clone(),
            )
            .annotate(ToolAnnotations::from_raw(
                Some("Create Task".to_string()),
                Some(false),
                Some(false),
                Some(false),
                Some(false),
            )),
            Tool::new(
                TASK_LIST_TOOL_NAME.to_string(),
                "List tasks in the ledger, optionally by status.".to_string(),
                list_schema.as_object().unwrap().clone(),
            )
            .annotate(ToolAnnotations::from_raw(
                Some("List Tasks".to_string()),
                Some(true),
                Some(false),
                Some(true),
                Some(true),
            )),
            Tool::new(
                TASK_COMPLETE_TOOL_NAME.to_string(),
                "Record a task's outcome; the result is delivered to the user exactly once."
                    .to_string(),
                complete_schema.as_object().unwrap().clone(),
            )
            .annotate(ToolAnnotations::from_raw(
                Some("Complete Task".to_string()),
                Some(false),
                Some(false),
                Some(false),
                Some(false),
            )),
            Tool::new(
                TASK_FAIL_TOOL_NAME.to_string(),
                "Mark a task failed; it stays open and visible, never auto-closed.".to_string(),
                fail_schema.as_object().unwrap().clone(),
            )
            .annotate(ToolAnnotations::from_raw(
                Some("Fail Task".to_string()),
                Some(false),
                Some(false),
                Some(false),
                Some(false),
            )),
            Tool::new(
                TASK_NOTE_TOOL_NAME.to_string(),
                "Append a note to a task's history.".to_string(),
                note_schema.as_object().unwrap().clone(),
            )
            .annotate(ToolAnnotations::from_raw(
                Some("Note Task".to_string()),
                Some(false),
                Some(false),
                Some(false),
                Some(false),
            )),
        ]
    }
}

#[async_trait]
impl McpClientTrait for TasksClient {
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
            TASK_CREATE_TOOL_NAME => self.handle_create(&ctx.session_id, arguments).await,
            TASK_LIST_TOOL_NAME => self.handle_list(arguments).await,
            TASK_COMPLETE_TOOL_NAME => self.handle_complete(&ctx.session_id, arguments).await,
            TASK_FAIL_TOOL_NAME => self.handle_fail(&ctx.session_id, arguments).await,
            TASK_NOTE_TOOL_NAME => self.handle_note(&ctx.session_id, arguments).await,
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
