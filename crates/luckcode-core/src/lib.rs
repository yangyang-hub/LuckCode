use anyhow::{Context, Result};
use async_trait::async_trait;
use futures_util::{StreamExt, future::BoxFuture};
use ignore::WalkBuilder;
use luckcode_model::{
    Message, MessageRole, ModelEvent, ModelProvider, ModelRequest, ToolCall as ModelToolCall,
    ToolSchema,
};
use luckcode_storage::{
    SessionInfo, append_session_compact_summary, append_session_message, append_session_tool_call,
    append_session_tool_result, project_hash, read_project_memory, read_session_events,
};
use luckcode_tools::{
    CommandApproval, CommandPolicy, CommandPolicyConfig, CommandPolicyDecision, CommandPreview,
    PermissionEngine, Tool, ToolCall as LocalToolCall, ToolContext, ToolOutput, ToolRegistry,
};
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    process::Stdio,
};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader, Lines},
    process::{ChildStdin, ChildStdout, Command as TokioCommand},
    time::{self, Duration},
};

#[derive(Debug, Clone)]
pub struct AgentOptions {
    pub max_steps: usize,
    pub stream: bool,
    pub resume_summary: Option<String>,
    pub verification: Option<VerificationOptions>,
}

impl Default for AgentOptions {
    fn default() -> Self {
        Self {
            max_steps: 8,
            stream: true,
            resume_summary: None,
            verification: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VerificationOptions {
    pub command: String,
    pub timeout_seconds: u64,
    pub max_output_bytes: usize,
}

impl VerificationOptions {
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            timeout_seconds: 120,
            max_output_bytes: 40_000,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AgentResult {
    pub final_answer: String,
    pub steps: usize,
    pub tool_calls: Vec<AgentToolCallRecord>,
    pub stopped_reason: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AgentToolCallRecord {
    pub name: String,
    pub ok: bool,
}

pub async fn run_agent(
    task: &str,
    workspace_root: &Path,
    session: &SessionInfo,
    model: &dyn ModelProvider,
    tools: &ToolRegistry,
    tool_context: ToolContext,
    options: AgentOptions,
) -> Result<AgentResult> {
    let mut messages = initial_messages(
        task,
        workspace_root,
        tool_context.max_file_size,
        options.resume_summary.as_deref(),
    )?;
    let tool_schemas = tool_schemas(tools);
    let mut final_answer = String::new();
    let mut tool_records = Vec::new();

    for step in 0..options.max_steps {
        let request = ModelRequest {
            messages: messages.clone(),
            tools: tool_schemas.clone(),
            temperature: None,
            max_tokens: None,
        };
        let mut stream = model.stream(request).await?;
        let mut assistant_text = String::new();
        let mut tool_calls = Vec::new();

        while let Some(event) = stream.next().await {
            match event? {
                ModelEvent::TextDelta(delta) => {
                    assistant_text.push_str(&delta);
                }
                ModelEvent::ToolCallDone(call) => {
                    tool_calls.push(call);
                }
                ModelEvent::Done => break,
                ModelEvent::ToolCallDelta(_) => {}
            }
        }

        if !assistant_text.is_empty() {
            append_session_message(session, "assistant", &assistant_text)?;
            messages.push(Message {
                role: MessageRole::Assistant,
                content: assistant_text.clone(),
            });
            final_answer.push_str(&assistant_text);
        }

        if tool_calls.is_empty() {
            return Ok(AgentResult {
                final_answer,
                steps: step + 1,
                tool_calls: tool_records,
                stopped_reason: None,
            });
        }

        let model_requested_shell = tool_calls.iter().any(|call| call.name == "run_shell");
        for call in tool_calls {
            let record = execute_tool_call(session, tools, tool_context.clone(), call).await?;
            let should_verify = should_verify_after_tool(&record);
            messages.push(Message {
                role: MessageRole::Tool,
                content: record.message_content.clone(),
            });
            tool_records.push(AgentToolCallRecord {
                name: record.name.clone(),
                ok: record.ok,
            });

            if should_verify
                && !model_requested_shell
                && let Some(verification) = &options.verification
                && !verification.command.trim().is_empty()
            {
                let verification_record = execute_tool_call(
                    session,
                    tools,
                    tool_context.clone(),
                    verification_tool_call(verification),
                )
                .await?;
                messages.push(Message {
                    role: MessageRole::Tool,
                    content: verification_record.message_content.clone(),
                });
                tool_records.push(AgentToolCallRecord {
                    name: verification_record.name,
                    ok: verification_record.ok,
                });
            }
        }
    }

    Ok(AgentResult {
        final_answer,
        steps: options.max_steps,
        tool_calls: tool_records,
        stopped_reason: Some("max steps exceeded".to_string()),
    })
}

#[derive(Debug, Clone)]
struct ExecutedToolCall {
    name: String,
    ok: bool,
    message_content: String,
    metadata: serde_json::Value,
}

async fn execute_tool_call(
    session: &SessionInfo,
    tools: &ToolRegistry,
    tool_context: ToolContext,
    call: ModelToolCall,
) -> Result<ExecutedToolCall> {
    append_session_tool_call(session, &call.name, &call.arguments)?;

    let name = call.name;
    let arguments = call.arguments;
    let output = tools
        .execute(
            LocalToolCall {
                name: name.clone(),
                arguments,
            },
            tool_context,
        )
        .await;

    match output {
        Ok(output) => {
            append_session_tool_result(
                session,
                &name,
                &output.content,
                &output.metadata,
                output.truncated,
            )?;
            Ok(ExecutedToolCall {
                name: name.clone(),
                ok: true,
                message_content: format!(
                    "tool_result:{name}\nmetadata:{}\ntruncated:{}\n{}",
                    output.metadata, output.truncated, output.content
                ),
                metadata: output.metadata,
            })
        }
        Err(error) => {
            let content = format!("ERROR: {error:#}");
            append_session_tool_result(
                session,
                &name,
                &content,
                &serde_json::json!({ "error": true }),
                false,
            )?;
            Ok(ExecutedToolCall {
                name: name.clone(),
                ok: false,
                message_content: format!("tool_result:{name}\n{content}"),
                metadata: serde_json::json!({ "error": true }),
            })
        }
    }
}

fn should_verify_after_tool(record: &ExecutedToolCall) -> bool {
    if !record.ok || !matches!(record.name.as_str(), "edit_file" | "write_file") {
        return false;
    }
    if record
        .metadata
        .get("skipped")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        return false;
    }
    if record.name == "edit_file"
        && record
            .metadata
            .get("replaced")
            .and_then(serde_json::Value::as_u64)
            == Some(0)
    {
        return false;
    }
    true
}

fn verification_tool_call(verification: &VerificationOptions) -> ModelToolCall {
    ModelToolCall {
        id: Some("luckcode_auto_verify".to_string()),
        name: "run_shell".to_string(),
        arguments: serde_json::json!({
            "command": verification.command,
            "timeout_seconds": verification.timeout_seconds,
            "max_output_bytes": verification.max_output_bytes,
        }),
    }
}

fn initial_messages(
    task: &str,
    workspace_root: &Path,
    max_file_size: u64,
    resume_summary: Option<&str>,
) -> Result<Vec<Message>> {
    let mut system = String::from(
        "You are LuckCode, a local Rust CLI coding agent. \
         You may use readonly tools, file editing tools, and run_shell when permissions allow. \
         Shell commands must be shown before execution and dangerous commands are refused. \
         Do not claim that files were modified or commands were executed unless a tool result proves it.",
    );

    let agents_path = workspace_root.join("AGENTS.md");
    if agents_path.exists() {
        let rules = fs::read_to_string(&agents_path)
            .with_context(|| format!("failed to read {}", agents_path.display()))?;
        system.push_str("\n\nProject rules from AGENTS.md:\n");
        system.push_str(&rules);
    }

    let project_context = build_project_context(workspace_root, max_file_size)?;
    if !project_context.trim().is_empty() {
        system.push_str("\n\nProject context:\n");
        system.push_str(&project_context);
    }

    if let Some(summary) = resume_summary
        && !summary.trim().is_empty()
    {
        system.push_str("\n\nResumed session summary:\n");
        system.push_str(summary);
        if !system.ends_with('\n') {
            system.push('\n');
        }
    }

    Ok(vec![
        Message {
            role: MessageRole::System,
            content: system,
        },
        Message {
            role: MessageRole::User,
            content: task.to_string(),
        },
    ])
}

pub fn build_project_context(workspace_root: &Path, max_file_size: u64) -> Result<String> {
    let mut out = String::new();
    let project_types = detect_project_types(workspace_root);
    if project_types.is_empty() {
        out.push_str("- Detected project types: unknown\n");
    } else {
        out.push_str("- Detected project types: ");
        out.push_str(&project_types.join(", "));
        out.push('\n');
    }

    let important_files = important_context_files(workspace_root);
    if !important_files.is_empty() {
        out.push_str("- Important files:\n");
        for path in &important_files {
            if let Ok(relative) = path.strip_prefix(workspace_root) {
                out.push_str("  - ");
                out.push_str(&relative.display().to_string());
                out.push('\n');
            }
        }
    }

    let top_level_entries = top_level_context_entries(workspace_root, 24);
    if !top_level_entries.is_empty() {
        out.push_str("- Workspace top-level:\n");
        for entry in top_level_entries {
            out.push_str("  - ");
            out.push_str(&entry);
            out.push('\n');
        }
    }

    let command_hints = command_hints(workspace_root);
    if !command_hints.is_empty() {
        out.push_str("- Command hints:\n");
        for hint in command_hints {
            out.push_str("  - ");
            out.push_str(&hint);
            out.push('\n');
        }
    }

    let source_files = source_file_overview(workspace_root, 40);
    if !source_files.is_empty() {
        out.push_str("- Source overview:\n");
        for file in source_files {
            out.push_str("  - ");
            out.push_str(&file);
            out.push('\n');
        }
    }

    if let Some(status) = git_status_short(workspace_root)
        && !status.trim().is_empty()
    {
        out.push_str("- Git status:\n");
        out.push_str(&indent_block(&status, "  "));
    }

    if let Some(diff_stat) = git_diff_stat(workspace_root)
        && !diff_stat.trim().is_empty()
    {
        out.push_str("- Git diff stat:\n");
        out.push_str(&indent_block(&diff_stat, "  "));
    }

    let memory = read_project_memory_for_root(workspace_root)?;
    if !memory.is_empty() {
        out.push_str("- Project memory:\n");
        for (key, value) in memory {
            out.push_str("  - ");
            out.push_str(&key);
            out.push_str(": ");
            out.push_str(&value);
            out.push('\n');
        }
    }

    let mut preview_count = 0;
    for path in important_files {
        if preview_count >= 4 {
            break;
        }
        if is_sensitive_path(&path) {
            continue;
        }
        let Some(preview) = read_preview(&path, max_file_size, 8_000)? else {
            continue;
        };
        if preview.trim().is_empty() {
            continue;
        }
        preview_count += 1;
        let relative = path.strip_prefix(workspace_root).unwrap_or(&path);
        out.push_str("- Preview: ");
        out.push_str(&relative.display().to_string());
        out.push('\n');
        out.push_str(&indent_block(&preview, "  "));
    }

    Ok(out)
}

pub fn compact_session(session: &SessionInfo) -> Result<CompactSessionResult> {
    let events = read_session_events(&session.project_hash, &session.id)?;
    let summary = build_compact_summary(&events);
    append_session_compact_summary(session, &summary)?;
    Ok(CompactSessionResult {
        session_id: session.id.clone(),
        event_count: events.len(),
        summary,
    })
}

pub fn compact_summary_for_session(project_hash: &str, session_id: &str) -> Result<String> {
    let events = read_session_events(project_hash, session_id)?;
    if let Some(summary) = latest_compact_summary_from_events(&events) {
        return Ok(summary);
    }
    Ok(build_compact_summary(&events))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactSessionResult {
    pub session_id: String,
    pub event_count: usize,
    pub summary: String,
}

fn build_compact_summary(events: &[serde_json::Value]) -> String {
    let mut task_goal = String::new();
    let mut completed = Vec::new();
    let mut viewed_files = Vec::new();
    let mut modified_files = Vec::new();
    let mut commands = Vec::new();
    let mut risks = Vec::new();
    let mut latest_status = String::new();

    for event in events {
        let kind = event.get("type").and_then(serde_json::Value::as_str);
        match kind {
            Some("user") if task_goal.is_empty() => {
                task_goal = compact_line(value_string(event, "content"), 240);
            }
            Some("assistant") => {
                let line = compact_line(value_string(event, "content"), 220);
                if !line.is_empty() {
                    latest_status = line.clone();
                    completed.push(line);
                }
            }
            Some("tool_call") => {
                let name = value_string(event, "name");
                let args = event.get("args").unwrap_or(&serde_json::Value::Null);
                match name.as_str() {
                    "read_file" => push_unique(&mut viewed_files, arg_string(args, "path")),
                    "edit_file" | "write_file" => {
                        push_unique(&mut modified_files, arg_string(args, "path"));
                    }
                    "run_shell" => push_unique(&mut commands, arg_string(args, "command")),
                    _ => {}
                }
            }
            Some("tool_result") => {
                if event
                    .get("truncated")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false)
                {
                    let name = value_string(event, "name");
                    risks.push(format!("{name} output was truncated"));
                }
                if event
                    .get("metadata")
                    .and_then(|metadata| metadata.get("error"))
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false)
                {
                    let name = value_string(event, "name");
                    risks.push(format!("{name} returned an error"));
                }
            }
            Some("checkpoint") => {
                let id = value_string(event, "id");
                if !id.is_empty() {
                    latest_status = format!("latest checkpoint: {id}");
                }
            }
            _ => {}
        }
    }

    let mut summary = String::new();
    push_section(
        &mut summary,
        "任务目标",
        if task_goal.is_empty() {
            "unknown"
        } else {
            task_goal.as_str()
        },
    );
    push_list_section(
        &mut summary,
        "已完成",
        &completed,
        6,
        "No assistant summary yet.",
    );
    push_list_section(
        &mut summary,
        "已查看文件",
        &viewed_files,
        20,
        "None recorded.",
    );
    push_list_section(
        &mut summary,
        "已修改文件",
        &modified_files,
        20,
        "None recorded.",
    );
    push_list_section(&mut summary, "已运行命令", &commands, 20, "None recorded.");
    push_section(
        &mut summary,
        "当前状态",
        if latest_status.is_empty() {
            "No final status recorded."
        } else {
            latest_status.as_str()
        },
    );
    push_section(
        &mut summary,
        "下一步建议",
        "Resume with the next concrete request; inspect recent tool results before editing further.",
    );
    push_list_section(&mut summary, "风险点", &risks, 10, "No risks recorded.");
    summary
}

fn latest_compact_summary_from_events(events: &[serde_json::Value]) -> Option<String> {
    let event = events.last()?;
    (event.get("type").and_then(serde_json::Value::as_str) == Some("compact_summary"))
        .then(|| value_string(event, "content"))
        .filter(|content| !content.trim().is_empty())
}

fn read_project_memory_for_root(workspace_root: &Path) -> Result<BTreeMap<String, String>> {
    let root = workspace_root
        .canonicalize()
        .unwrap_or_else(|_| workspace_root.to_path_buf());
    read_project_memory(&project_hash(root))
}

fn value_string(event: &serde_json::Value, key: &str) -> String {
    event
        .get(key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn arg_string(args: &serde_json::Value, key: &str) -> String {
    args.get(key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn compact_line(text: String, max_chars: usize) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() <= max_chars {
        return text;
    }
    let mut out = text.chars().take(max_chars).collect::<String>();
    out.push_str("...");
    out
}

fn push_unique(items: &mut Vec<String>, value: String) {
    if value.is_empty() || items.iter().any(|item| item == &value) {
        return;
    }
    items.push(value);
}

fn push_section(out: &mut String, title: &str, value: &str) {
    out.push_str(title);
    out.push_str(":\n");
    out.push_str(value);
    out.push_str("\n\n");
}

fn push_list_section(out: &mut String, title: &str, values: &[String], limit: usize, empty: &str) {
    out.push_str(title);
    out.push_str(":\n");
    if values.is_empty() {
        out.push_str("- ");
        out.push_str(empty);
        out.push('\n');
    } else {
        for value in values.iter().rev().take(limit).rev() {
            out.push_str("- ");
            out.push_str(value);
            out.push('\n');
        }
    }
    out.push('\n');
}

fn detect_project_types(root: &Path) -> Vec<String> {
    let mut types = Vec::new();
    if root.join("Cargo.toml").exists() {
        types.push("Rust".to_string());
    }
    if root.join("package.json").exists() {
        types.push("Node/TypeScript".to_string());
    }
    if root.join("pom.xml").exists() {
        types.push("Java Maven".to_string());
    }
    if root.join("build.gradle").exists() || root.join("build.gradle.kts").exists() {
        types.push("Java/Gradle".to_string());
    }
    if root.join("go.mod").exists() {
        types.push("Go".to_string());
    }
    if root.join("pyproject.toml").exists() {
        types.push("Python".to_string());
    }
    if root.join("docker-compose.yml").exists() || root.join("docker-compose.yaml").exists() {
        types.push("Docker Compose".to_string());
    }
    if contains_extension(root, "tf") {
        types.push("Terraform".to_string());
    }

    types
}

fn important_context_files(root: &Path) -> Vec<PathBuf> {
    [
        "README.md",
        "AGENTS.md",
        "CLAUDE.md",
        "Cargo.toml",
        "package.json",
        "pom.xml",
        "build.gradle",
        "build.gradle.kts",
        "go.mod",
        "pyproject.toml",
        "docker-compose.yml",
        "docker-compose.yaml",
    ]
    .into_iter()
    .map(|file| root.join(file))
    .filter(|path| path.exists() && path.is_file())
    .collect()
}

fn top_level_context_entries(root: &Path, limit: usize) -> Vec<String> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut entries = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            let name = path.file_name()?.to_string_lossy().to_string();
            if should_skip_context_name(&name) || is_sensitive_path(&path) {
                return None;
            }
            let suffix = if entry.file_type().ok()?.is_dir() {
                "/"
            } else {
                ""
            };
            Some(format!("{name}{suffix}"))
        })
        .collect::<Vec<_>>();
    entries.sort();
    entries.truncate(limit);
    entries
}

fn command_hints(root: &Path) -> Vec<String> {
    let mut commands = Vec::new();
    if let Ok(loaded) = load_config(root)
        && let Some(configured) = loaded.config.commands
    {
        push_command_hint(&mut commands, "test", configured.test);
        push_command_hint(&mut commands, "check", configured.check);
        push_command_hint(&mut commands, "lint", configured.lint);
    }

    if root.join("Cargo.toml").exists() {
        push_unique(&mut commands, "test: cargo test".to_string());
        push_unique(&mut commands, "check: cargo check".to_string());
        push_unique(&mut commands, "lint: cargo clippy".to_string());
    }
    if root.join("package.json").exists() {
        push_unique(&mut commands, "test: npm test".to_string());
    }
    if root.join("pom.xml").exists() {
        push_unique(&mut commands, "test: mvn test".to_string());
    }
    if root.join("build.gradle").exists() || root.join("build.gradle.kts").exists() {
        push_unique(&mut commands, "test: ./gradlew test".to_string());
    }
    if root.join("go.mod").exists() {
        push_unique(&mut commands, "test: go test ./...".to_string());
    }
    if root.join("pyproject.toml").exists() {
        push_unique(&mut commands, "test: pytest".to_string());
    }

    commands
}

fn push_command_hint(commands: &mut Vec<String>, label: &str, command: Option<String>) {
    if let Some(command) = command
        && !command.trim().is_empty()
    {
        push_unique(commands, format!("{label}: {command}"));
    }
}

fn source_file_overview(root: &Path, limit: usize) -> Vec<String> {
    let mut files = Vec::new();
    let walker = WalkBuilder::new(root)
        .max_depth(Some(5))
        .git_ignore(true)
        .git_exclude(true)
        .filter_entry(|entry| {
            let name = entry.file_name().to_string_lossy();
            !should_skip_context_name(&name)
        })
        .build();

    for result in walker {
        let Ok(entry) = result else {
            continue;
        };
        let path = entry.path();
        if files.len() >= limit {
            break;
        }
        if !path.is_file() || is_sensitive_path(path) || !is_source_context_file(path) {
            continue;
        }
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let size = entry.metadata().map(|metadata| metadata.len()).unwrap_or(0);
        files.push(format!("{} ({} bytes)", relative.display(), size));
    }

    files
}

fn is_source_context_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|ext| ext.to_str()),
        Some(
            "rs" | "ts"
                | "tsx"
                | "js"
                | "jsx"
                | "py"
                | "go"
                | "java"
                | "kt"
                | "kts"
                | "c"
                | "cc"
                | "cpp"
                | "h"
                | "hpp"
                | "cs"
                | "rb"
                | "php"
                | "swift"
                | "scala"
                | "sql"
        )
    )
}

fn should_skip_context_name(name: &str) -> bool {
    matches!(
        name,
        ".git"
            | ".hg"
            | ".svn"
            | ".luckcode"
            | "target"
            | "node_modules"
            | "dist"
            | "build"
            | ".next"
            | ".cache"
            | "coverage"
    )
}

fn contains_extension(root: &Path, extension: &str) -> bool {
    let Ok(entries) = fs::read_dir(root) else {
        return false;
    };

    entries.filter_map(Result::ok).any(|entry| {
        entry
            .path()
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext == extension)
    })
}

fn git_status_short(root: &Path) -> Option<String> {
    command_output(
        Command::new("git")
            .arg("-C")
            .arg(root)
            .arg("status")
            .arg("--short"),
    )
}

fn git_diff_stat(root: &Path) -> Option<String> {
    command_output(
        Command::new("git")
            .arg("-C")
            .arg(root)
            .arg("diff")
            .arg("--stat"),
    )
}

fn command_output(command: &mut Command) -> Option<String> {
    let output = command.output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).to_string())
}

fn read_preview(path: &Path, max_file_size: u64, max_chars: usize) -> Result<Option<String>> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("failed to read metadata for {}", path.display()))?;
    if metadata.len() > max_file_size {
        return Ok(None);
    }

    let text = fs::read_to_string(path)
        .with_context(|| format!("failed to read UTF-8 file {}", path.display()))?;
    Ok(Some(text.chars().take(max_chars).collect()))
}

fn indent_block(text: &str, prefix: &str) -> String {
    text.lines()
        .map(|line| format!("{prefix}{line}\n"))
        .collect::<String>()
}

fn is_sensitive_path(path: &Path) -> bool {
    let text = path.to_string_lossy();
    text.contains(".env")
        || text.ends_with(".pem")
        || text.ends_with(".key")
        || text.ends_with("id_rsa")
        || text.ends_with("id_ed25519")
}

fn tool_schemas(tools: &ToolRegistry) -> Vec<ToolSchema> {
    tools
        .list()
        .into_iter()
        .map(|tool| ToolSchema {
            name: tool.name.to_string(),
            description: tool.description.to_string(),
            schema: tool.schema,
        })
        .collect()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AppConfig {
    pub model: ModelConfig,
    pub providers: BTreeMap<String, ProviderConfig>,
    pub permission: PermissionConfig,
    pub workspace: WorkspaceConfig,
    pub ui: UiConfig,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<ProjectConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commands: Option<CommandConfig>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            model: ModelConfig::default(),
            providers: default_providers(),
            permission: PermissionConfig::default(),
            workspace: WorkspaceConfig::default(),
            ui: UiConfig::default(),
            project: None,
            commands: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ModelConfig {
    pub provider: String,
    pub model: String,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            provider: "mock".to_string(),
            model: "mock".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ProviderConfig {
    pub kind: Option<String>,
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
    pub request_format: Option<String>,
    pub timeout_seconds: Option<u64>,
    pub retry_attempts: Option<u8>,
    pub enabled: bool,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            kind: None,
            model: None,
            base_url: None,
            api_key_env: None,
            request_format: None,
            timeout_seconds: None,
            retry_attempts: None,
            enabled: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedProviderConfig {
    pub name: String,
    pub kind: String,
    pub model: String,
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
    pub request_format: Option<String>,
    pub timeout_seconds: Option<u64>,
    pub retry_attempts: Option<u8>,
}

pub fn resolve_provider_config(
    config: &AppConfig,
    provider_override: Option<&str>,
    model_override: Option<&str>,
) -> Result<ResolvedProviderConfig> {
    let mut provider_name = provider_override
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| config.model.provider.clone());
    let mut root_model = model_override
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| config.model.model.clone());

    if provider_override.is_none()
        && let Some((provider, model)) = split_provider_model(&root_model)
    {
        provider_name = provider;
        root_model = model;
    }

    let profile = config.providers.get(&provider_name);
    if profile.is_some_and(|profile| !profile.enabled) {
        anyhow::bail!("provider '{}' is disabled", provider_name);
    }

    let inferred = inferred_provider_config(&provider_name);
    let kind = profile
        .and_then(|profile| profile.kind.clone())
        .or_else(|| inferred.kind.clone())
        .unwrap_or_else(|| provider_name.clone());

    let default_model = ModelConfig::default().model;
    let model =
        if model_override.is_some() || root_model != default_model || provider_name == "mock" {
            root_model
        } else {
            profile
                .and_then(|profile| profile.model.clone())
                .or_else(|| inferred.model.clone())
                .unwrap_or(root_model)
        };

    Ok(ResolvedProviderConfig {
        name: provider_name,
        kind,
        model,
        base_url: profile
            .and_then(|profile| profile.base_url.clone())
            .or(inferred.base_url),
        api_key_env: profile
            .and_then(|profile| profile.api_key_env.clone())
            .or(inferred.api_key_env),
        request_format: profile
            .and_then(|profile| profile.request_format.clone())
            .or(inferred.request_format),
        timeout_seconds: env_u64("LUCKCODE_MODEL_TIMEOUT_SECONDS")
            .or_else(|| profile.and_then(|profile| profile.timeout_seconds))
            .or(inferred.timeout_seconds),
        retry_attempts: env_u8("LUCKCODE_MODEL_RETRY_ATTEMPTS")
            .or_else(|| profile.and_then(|profile| profile.retry_attempts))
            .or(inferred.retry_attempts),
    })
}

fn split_provider_model(value: &str) -> Option<(String, String)> {
    let (provider, model) = value.split_once('/')?;
    if provider.is_empty() || model.is_empty() {
        return None;
    }

    Some((provider.to_string(), model.to_string()))
}

fn default_providers() -> BTreeMap<String, ProviderConfig> {
    let mut providers = BTreeMap::new();
    providers.insert(
        "mock".to_string(),
        ProviderConfig {
            kind: Some("mock".to_string()),
            model: Some("mock".to_string()),
            ..ProviderConfig::default()
        },
    );
    providers
}

fn inferred_provider_config(name: &str) -> ProviderConfig {
    match name {
        "mock" => ProviderConfig {
            kind: Some("mock".to_string()),
            model: Some("mock".to_string()),
            ..ProviderConfig::default()
        },
        "openai" | "openai-compatible" | "openai-chat" | "openai-chat-completions" => {
            ProviderConfig {
                kind: Some("openai".to_string()),
                request_format: Some("chat-completions".to_string()),
                ..ProviderConfig::default()
            }
        }
        "responses" | "openai-responses" => ProviderConfig {
            kind: Some("openai".to_string()),
            request_format: Some("responses".to_string()),
            ..ProviderConfig::default()
        },
        "anthropic" | "claude" => ProviderConfig {
            kind: Some("anthropic".to_string()),
            request_format: Some("anthropic".to_string()),
            ..ProviderConfig::default()
        },
        _ => ProviderConfig::default(),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct PermissionConfig {
    pub mode: PermissionMode,
}

impl Default for PermissionConfig {
    fn default() -> Self {
        Self {
            mode: PermissionMode::Manual,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionMode {
    Plan,
    #[default]
    Manual,
    AcceptEdits,
    Auto,
    Sandbox,
    Dangerous,
}

impl PermissionMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Manual => "manual",
            Self::AcceptEdits => "accept-edits",
            Self::Auto => "auto",
            Self::Sandbox => "sandbox",
            Self::Dangerous => "dangerous",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct WorkspaceConfig {
    pub max_file_size: u64,
    pub ignore: Vec<String>,
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            max_file_size: 200_000,
            ignore: vec![
                ".git".to_string(),
                "node_modules".to_string(),
                "target".to_string(),
                "dist".to_string(),
                ".env".to_string(),
            ],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct UiConfig {
    pub stream: bool,
    pub show_tool_calls: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            stream: true,
            show_tool_calls: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProjectConfig {
    pub name: String,
    pub language: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct CommandConfig {
    pub test: Option<String>,
    pub check: Option<String>,
    pub lint: Option<String>,
    pub policy: CommandPolicyConfig,
}

#[derive(Debug, Clone)]
pub struct LoadedConfig {
    pub config: AppConfig,
    pub sources: Vec<ConfigSource>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default)]
pub struct McpConfig {
    #[serde(rename = "mcpServers")]
    pub servers: BTreeMap<String, McpServerConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default)]
pub struct McpServerConfig {
    pub command: Option<String>,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub url: Option<String>,
    pub headers: BTreeMap<String, String>,
    pub tool_policies: BTreeMap<String, CommandPolicy>,
    pub disabled: bool,
}

#[derive(Debug, Clone)]
pub struct LoadedMcpConfig {
    pub path: PathBuf,
    pub loaded: bool,
    pub config: McpConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct McpToolInfo {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub input_schema: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpToolRegistrationReport {
    pub registered_tools: Vec<String>,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ConfigSource {
    pub path: PathBuf,
    pub loaded: bool,
}

pub fn load_config(workspace_root: impl AsRef<Path>) -> Result<LoadedConfig> {
    let mut config = AppConfig::default();
    let mut sources = Vec::new();

    let global_path = luckcode_storage::config_file()?;
    load_optional_config_file(&global_path, &mut config, &mut sources)?;

    let project_path = workspace_root
        .as_ref()
        .join(".luckcode")
        .join("config.toml");
    load_optional_config_file(&project_path, &mut config, &mut sources)?;

    apply_env_overrides(&mut config);

    Ok(LoadedConfig { config, sources })
}

pub fn load_mcp_config(workspace_root: impl AsRef<Path>) -> Result<LoadedMcpConfig> {
    let path = workspace_root.as_ref().join(".luckcode").join("mcp.json");
    if !path.exists() {
        return Ok(LoadedMcpConfig {
            path,
            loaded: false,
            config: McpConfig::default(),
        });
    }

    let text = fs::read_to_string(&path)
        .with_context(|| format!("failed to read MCP config {}", path.display()))?;
    let config = serde_json::from_str(&text)
        .with_context(|| format!("failed to parse MCP config {}", path.display()))?;
    Ok(LoadedMcpConfig {
        path,
        loaded: true,
        config,
    })
}

pub async fn list_mcp_tools(
    server: &McpServerConfig,
    timeout_seconds: u64,
) -> Result<Vec<McpToolInfo>> {
    let result = mcp_request(server, timeout_seconds, "tools/list", serde_json::json!({})).await?;

    parse_mcp_tools_result(&result)
}

pub async fn call_mcp_tool(
    server: &McpServerConfig,
    tool_name: &str,
    arguments: serde_json::Value,
    timeout_seconds: u64,
) -> Result<serde_json::Value> {
    if tool_name.trim().is_empty() {
        anyhow::bail!("MCP tool name cannot be empty");
    }

    let tool_name = tool_name.to_string();
    mcp_request(
        server,
        timeout_seconds,
        "tools/call",
        serde_json::json!({
            "name": tool_name,
            "arguments": arguments,
        }),
    )
    .await
}

pub async fn list_mcp_resources(
    server: &McpServerConfig,
    timeout_seconds: u64,
) -> Result<serde_json::Value> {
    mcp_simple_request(server, timeout_seconds, "resources/list").await
}

pub async fn list_mcp_prompts(
    server: &McpServerConfig,
    timeout_seconds: u64,
) -> Result<serde_json::Value> {
    mcp_simple_request(server, timeout_seconds, "prompts/list").await
}

async fn mcp_simple_request(
    server: &McpServerConfig,
    timeout_seconds: u64,
    method: &'static str,
) -> Result<serde_json::Value> {
    mcp_request(server, timeout_seconds, method, serde_json::json!({})).await
}

async fn mcp_request(
    server: &McpServerConfig,
    timeout_seconds: u64,
    method: &'static str,
    params: serde_json::Value,
) -> Result<serde_json::Value> {
    if server.url.is_some() {
        mcp_http_request(server, timeout_seconds, method, params).await
    } else {
        with_mcp_stdio_session(server, timeout_seconds, move |stdin, reader| {
            Box::pin(async move {
                initialize_mcp(stdin, reader).await?;
                send_mcp_request(stdin, 2, method, params).await?;
                read_mcp_response(reader, 2).await
            })
        })
        .await
    }
}

async fn mcp_http_request(
    server: &McpServerConfig,
    timeout_seconds: u64,
    method: &'static str,
    params: serde_json::Value,
) -> Result<serde_json::Value> {
    if server.disabled {
        anyhow::bail!("MCP server is disabled");
    }
    let url = server
        .url
        .as_deref()
        .context("MCP HTTP server requires a url")?;
    let timeout_seconds = timeout_seconds.clamp(1, 120);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout_seconds))
        .build()
        .context("failed to build MCP HTTP client")?;

    let initialize = mcp_jsonrpc_message(1, "initialize", mcp_initialize_params());
    let (_, session_id) =
        send_mcp_http_jsonrpc(&client, server, url, initialize, None, Some(1)).await?;

    let initialized = mcp_jsonrpc_notification("notifications/initialized", serde_json::json!({}));
    send_mcp_http_jsonrpc(
        &client,
        server,
        url,
        initialized,
        session_id.as_deref(),
        None,
    )
    .await?;

    let request = mcp_jsonrpc_message(2, method, params);
    let (result, _) = send_mcp_http_jsonrpc(
        &client,
        server,
        url,
        request,
        session_id.as_deref(),
        Some(2),
    )
    .await?;
    Ok(result)
}

async fn send_mcp_http_jsonrpc(
    client: &reqwest::Client,
    server: &McpServerConfig,
    url: &str,
    message: serde_json::Value,
    session_id: Option<&str>,
    expected_id: Option<u64>,
) -> Result<(serde_json::Value, Option<String>)> {
    let mut request = client
        .post(url)
        .header(CONTENT_TYPE, "application/json")
        .header(ACCEPT, "application/json, text/event-stream")
        .json(&message);
    if let Some(session_id) = session_id {
        request = request.header("Mcp-Session-Id", session_id);
    }
    for (key, value) in &server.headers {
        let name = HeaderName::from_bytes(key.as_bytes())
            .with_context(|| format!("invalid MCP HTTP header name '{key}'"))?;
        let value = HeaderValue::from_str(value)
            .with_context(|| format!("invalid MCP HTTP header value for '{key}'"))?;
        request = request.header(name, value);
    }

    let response = request
        .send()
        .await
        .context("failed to send MCP HTTP request")?;
    let status = response.status();
    let session_id = response
        .headers()
        .get("Mcp-Session-Id")
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned);
    let body = response
        .text()
        .await
        .context("failed to read MCP HTTP response")?;
    if !status.is_success() {
        let (body, _) = truncate_text_bytes(&body, 2_000);
        anyhow::bail!("MCP HTTP request failed with status {status}: {body}");
    }

    let Some(expected_id) = expected_id else {
        return Ok((serde_json::Value::Null, session_id));
    };
    let result = parse_mcp_http_response_body(&body, expected_id)?;
    Ok((result, session_id))
}

fn parse_mcp_http_response_body(body: &str, expected_id: u64) -> Result<serde_json::Value> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        anyhow::bail!("MCP HTTP response {expected_id} was empty");
    }

    if let Ok(message) = serde_json::from_str::<serde_json::Value>(trimmed) {
        return mcp_result_from_message(&message, expected_id);
    }

    for line in trimmed.lines() {
        let Some(data) = line.trim_start().strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let message: serde_json::Value =
            serde_json::from_str(data).context("failed to parse MCP SSE JSON-RPC data")?;
        if mcp_id_matches(&message, expected_id) {
            return mcp_result_from_message(&message, expected_id);
        }
    }

    anyhow::bail!("MCP HTTP response did not contain response id {expected_id}")
}

pub async fn register_mcp_tools(
    registry: &mut ToolRegistry,
    config: &McpConfig,
    timeout_seconds: u64,
) -> McpToolRegistrationReport {
    let mut registered_tools = Vec::new();
    let mut errors = Vec::new();

    for (server_name, server) in &config.servers {
        if server.disabled {
            continue;
        }
        match list_mcp_tools(server, timeout_seconds).await {
            Ok(tools) => {
                for tool in tools {
                    let local_name = mcp_tool_local_name(server_name, &tool.name);
                    let description = mcp_tool_description(server_name, &tool);
                    registry.register(McpTool {
                        local_name: local_name.clone(),
                        description,
                        server_name: server_name.clone(),
                        tool_name: tool.name,
                        server: server.clone(),
                        input_schema: tool.input_schema,
                        timeout_seconds,
                    });
                    registered_tools.push(local_name);
                }
            }
            Err(error) => {
                errors.push(format!("{server_name}: {error:#}"));
            }
        }
    }

    McpToolRegistrationReport {
        registered_tools,
        errors,
    }
}

#[derive(Debug, Clone)]
struct McpTool {
    local_name: String,
    description: String,
    server_name: String,
    tool_name: String,
    server: McpServerConfig,
    input_schema: serde_json::Value,
    timeout_seconds: u64,
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.local_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn schema(&self) -> serde_json::Value {
        self.input_schema.clone()
    }

    async fn execute(&self, input: serde_json::Value, ctx: ToolContext) -> Result<ToolOutput> {
        match authorize_mcp_tool(
            &ctx,
            &self.server,
            &self.server_name,
            &self.tool_name,
            self.timeout_seconds,
            40_000,
        )? {
            McpToolOutcome::Skipped { preview } => Ok(ToolOutput {
                content: format!("MCP tool skipped by user: {}\n", preview.command),
                metadata: serde_json::json!({
                    "mcp": true,
                    "server": self.server_name,
                    "tool": self.tool_name,
                    "skipped": true,
                }),
                truncated: false,
            }),
            McpToolOutcome::Run { preview, prompted } => {
                if !prompted && let Some(announce) = &ctx.announce_command {
                    announce(&preview);
                }
                let result =
                    call_mcp_tool(&self.server, &self.tool_name, input, self.timeout_seconds)
                        .await?;
                let content =
                    serde_json::to_string_pretty(&result).context("failed to render MCP result")?;
                let bytes = content.len();
                let (content, truncated) = truncate_text_bytes(&content, 40_000);
                let is_error = result
                    .get("isError")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                Ok(ToolOutput {
                    content,
                    metadata: serde_json::json!({
                        "mcp": true,
                        "server": self.server_name,
                        "tool": self.tool_name,
                        "bytes": bytes,
                        "error": is_error,
                    }),
                    truncated,
                })
            }
        }
    }
}

#[derive(Debug)]
enum McpToolOutcome {
    Run {
        preview: CommandPreview,
        prompted: bool,
    },
    Skipped {
        preview: CommandPreview,
    },
}

fn authorize_mcp_tool(
    ctx: &ToolContext,
    server: &McpServerConfig,
    server_name: &str,
    tool_name: &str,
    timeout_seconds: u64,
    max_output_bytes: usize,
) -> Result<McpToolOutcome> {
    let default_policy = match ctx.command_approval {
        CommandApproval::Refuse => CommandPolicy::Deny,
        CommandApproval::Prompt => CommandPolicy::Ask,
        CommandApproval::Auto => CommandPolicy::Allow,
    };
    let policy_config = if ctx.command_approval == CommandApproval::Refuse {
        CommandPolicyConfig {
            default_policy: None,
            allowlist: Vec::new(),
            denylist: ctx.command_policy.denylist.clone(),
        }
    } else {
        ctx.command_policy.clone()
    };
    let command = format!("mcp {server_name} {tool_name}");
    let decision = match mcp_tool_policy(server, tool_name) {
        Some(policy) if ctx.command_approval != CommandApproval::Refuse => CommandPolicyDecision {
            policy,
            reason: format!("MCP tool policy configured as {policy:?}"),
        },
        _ => PermissionEngine::new(default_policy).evaluate_command(&command, &policy_config),
    };
    let working_dir = ctx
        .workspace_root
        .canonicalize()
        .unwrap_or_else(|_| ctx.workspace_root.clone());
    let preview = CommandPreview {
        command,
        working_dir,
        timeout_seconds,
        max_output_bytes,
        reason: decision.reason.clone(),
    };

    match decision.policy {
        CommandPolicy::Deny => anyhow::bail!("MCP tool denied: {}", decision.reason),
        CommandPolicy::Allow => Ok(McpToolOutcome::Run {
            preview,
            prompted: false,
        }),
        CommandPolicy::Ask => {
            let Some(confirm) = &ctx.confirm_command else {
                anyhow::bail!(
                    "interactive MCP confirmation is unavailable; rerun in auto mode or configure manual confirmation"
                );
            };
            if !confirm(&preview) {
                return Ok(McpToolOutcome::Skipped { preview });
            }
            Ok(McpToolOutcome::Run {
                preview,
                prompted: true,
            })
        }
    }
}

fn mcp_tool_local_name(server_name: &str, tool_name: &str) -> String {
    format!(
        "mcp_{}_{}",
        sanitize_mcp_identifier(server_name),
        sanitize_mcp_identifier(tool_name)
    )
}

fn mcp_tool_policy(server: &McpServerConfig, tool_name: &str) -> Option<CommandPolicy> {
    server
        .tool_policies
        .get(tool_name)
        .copied()
        .or_else(|| server.tool_policies.get("*").copied())
}

fn mcp_tool_description(server_name: &str, tool: &McpToolInfo) -> String {
    match tool.description.as_deref() {
        Some(description) if !description.trim().is_empty() => {
            format!(
                "MCP tool '{}' from server '{}': {description}",
                tool.name, server_name
            )
        }
        _ => format!("MCP tool '{}' from server '{}'.", tool.name, server_name),
    }
}

fn sanitize_mcp_identifier(value: &str) -> String {
    let mut out = String::new();
    let mut last_was_underscore = false;
    for ch in value.chars() {
        let normalized = if ch.is_ascii_alphanumeric() {
            Some(ch.to_ascii_lowercase())
        } else {
            Some('_')
        };
        if let Some(ch) = normalized {
            if ch == '_' {
                if !last_was_underscore && !out.is_empty() {
                    out.push('_');
                }
                last_was_underscore = true;
            } else {
                out.push(ch);
                last_was_underscore = false;
            }
        }
    }
    let out = out.trim_matches('_').to_string();
    if out.is_empty() {
        "tool".to_string()
    } else {
        out
    }
}

fn truncate_text_bytes(text: &str, max_bytes: usize) -> (String, bool) {
    if text.len() <= max_bytes {
        return (text.to_string(), false);
    }

    let mut end = max_bytes.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut truncated = text[..end].to_string();
    if !truncated.ends_with('\n') {
        truncated.push('\n');
    }
    truncated.push_str(&format!("[MCP result truncated after {max_bytes} bytes]\n"));
    (truncated, true)
}

async fn with_mcp_stdio_session<F>(
    server: &McpServerConfig,
    timeout_seconds: u64,
    operation: F,
) -> Result<serde_json::Value>
where
    F: for<'a> FnOnce(
        &'a mut ChildStdin,
        &'a mut Lines<BufReader<ChildStdout>>,
    ) -> BoxFuture<'a, Result<serde_json::Value>>,
{
    if server.disabled {
        anyhow::bail!("MCP server is disabled");
    }
    let command_name = server
        .command
        .as_deref()
        .context("MCP stdio server requires a command")?;

    let mut command = TokioCommand::new(command_name);
    command
        .args(&server.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    for (key, value) in &server.env {
        command.env(key, value);
    }

    let mut child = command
        .spawn()
        .with_context(|| format!("failed to start MCP server command '{command_name}'"))?;
    let mut stdin = child
        .stdin
        .take()
        .context("failed to open MCP server stdin")?;
    let stdout = child
        .stdout
        .take()
        .context("failed to open MCP server stdout")?;
    let mut reader = BufReader::new(stdout).lines();
    let timeout_seconds = timeout_seconds.clamp(1, 120);

    let result = time::timeout(
        Duration::from_secs(timeout_seconds),
        operation(&mut stdin, &mut reader),
    )
    .await;
    let _ = child.kill().await;

    match result {
        Ok(result) => result,
        Err(_) => anyhow::bail!("MCP request timed out after {timeout_seconds}s"),
    }
}

async fn initialize_mcp<W, R>(stdin: &mut W, reader: &mut Lines<R>) -> Result<()>
where
    W: AsyncWrite + Unpin,
    R: AsyncBufRead + Unpin,
{
    send_mcp_request(stdin, 1, "initialize", mcp_initialize_params()).await?;
    read_mcp_response(reader, 1).await?;
    send_mcp_notification(stdin, "notifications/initialized", serde_json::json!({})).await
}

fn mcp_initialize_params() -> serde_json::Value {
    serde_json::json!({
        "protocolVersion": "2024-11-05",
        "capabilities": {},
        "clientInfo": {
            "name": "luckcode",
            "version": env!("CARGO_PKG_VERSION")
        }
    })
}

async fn send_mcp_request<W>(
    stdin: &mut W,
    id: u64,
    method: &str,
    params: serde_json::Value,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let message = mcp_jsonrpc_message(id, method, params);
    write_mcp_message(stdin, &message).await
}

async fn send_mcp_notification<W>(
    stdin: &mut W,
    method: &str,
    params: serde_json::Value,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let message = mcp_jsonrpc_notification(method, params);
    write_mcp_message(stdin, &message).await
}

fn mcp_jsonrpc_message(id: u64, method: &str, params: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    })
}

fn mcp_jsonrpc_notification(method: &str, params: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
    })
}

async fn write_mcp_message<W>(stdin: &mut W, message: &serde_json::Value) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let line = serde_json::to_string(message).context("failed to serialize MCP message")?;
    stdin
        .write_all(line.as_bytes())
        .await
        .context("failed to write MCP message")?;
    stdin
        .write_all(b"\n")
        .await
        .context("failed to write MCP newline")?;
    stdin.flush().await.context("failed to flush MCP stdin")
}

async fn read_mcp_response<R>(reader: &mut Lines<R>, expected_id: u64) -> Result<serde_json::Value>
where
    R: AsyncBufRead + Unpin,
{
    while let Some(line) = reader
        .next_line()
        .await
        .context("failed to read MCP response")?
    {
        if line.trim().is_empty() {
            continue;
        }
        let message: serde_json::Value =
            serde_json::from_str(&line).context("failed to parse MCP JSON-RPC message")?;
        if !mcp_id_matches(&message, expected_id) {
            continue;
        }
        return mcp_result_from_message(&message, expected_id);
    }

    anyhow::bail!("MCP server closed stdout before response {expected_id}")
}

fn mcp_result_from_message(
    message: &serde_json::Value,
    expected_id: u64,
) -> Result<serde_json::Value> {
    if message.is_array() {
        let Some(matching) = message.as_array().and_then(|messages| {
            messages
                .iter()
                .find(|message| mcp_id_matches(message, expected_id))
        }) else {
            anyhow::bail!("MCP response batch did not contain id {expected_id}");
        };
        return mcp_result_from_message(matching, expected_id);
    }

    if !mcp_id_matches(message, expected_id) {
        anyhow::bail!("MCP response id did not match expected id {expected_id}");
    }
    if let Some(error) = message.get("error") {
        anyhow::bail!("MCP request {expected_id} failed: {}", compact_json(error));
    }
    Ok(message
        .get("result")
        .cloned()
        .unwrap_or(serde_json::Value::Null))
}

fn mcp_id_matches(message: &serde_json::Value, expected_id: u64) -> bool {
    let Some(id) = message.get("id") else {
        return false;
    };
    id.as_u64() == Some(expected_id)
        || id
            .as_str()
            .is_some_and(|value| value == expected_id.to_string())
}

fn parse_mcp_tools_result(result: &serde_json::Value) -> Result<Vec<McpToolInfo>> {
    let tools = result
        .get("tools")
        .and_then(serde_json::Value::as_array)
        .context("MCP tools/list result did not contain a tools array")?;
    tools
        .iter()
        .map(|tool| {
            let name = tool
                .get("name")
                .and_then(serde_json::Value::as_str)
                .context("MCP tool is missing a string name")?
                .to_string();
            let description = tool
                .get("description")
                .and_then(serde_json::Value::as_str)
                .map(ToOwned::to_owned);
            let input_schema = tool
                .get("inputSchema")
                .or_else(|| tool.get("input_schema"))
                .cloned()
                .unwrap_or_else(|| serde_json::json!({ "type": "object" }));
            Ok(McpToolInfo {
                name,
                description,
                input_schema,
            })
        })
        .collect()
}

fn compact_json(value: &serde_json::Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| value.to_string())
}

pub fn config_to_toml(config: &AppConfig) -> Result<String> {
    toml::to_string_pretty(config).context("failed to serialize config")
}

fn load_optional_config_file(
    path: &Path,
    config: &mut AppConfig,
    sources: &mut Vec<ConfigSource>,
) -> Result<()> {
    if !path.exists() {
        sources.push(ConfigSource {
            path: path.to_path_buf(),
            loaded: false,
        });
        return Ok(());
    }

    let text = fs::read_to_string(path)
        .with_context(|| format!("failed to read config file {}", path.display()))?;
    let partial: PartialAppConfig = toml::from_str(&text)
        .with_context(|| format!("failed to parse config file {}", path.display()))?;
    partial.apply_to(config);

    sources.push(ConfigSource {
        path: path.to_path_buf(),
        loaded: true,
    });
    Ok(())
}

fn apply_env_overrides(config: &mut AppConfig) {
    if let Ok(value) =
        env::var("LUCKCODE_PROVIDER").or_else(|_| env::var("LUCKCODE_MODEL_PROVIDER"))
    {
        config.model.provider = value;
    }

    if let Ok(value) = env::var("LUCKCODE_MODEL") {
        config.model.model = value;
    }

    if let Ok(value) = env::var("LUCKCODE_PERMISSION_MODE")
        && let Ok(mode) = parse_permission_mode(&value)
    {
        config.permission.mode = mode;
    }

    if let Some(timeout_seconds) = env_u64("LUCKCODE_MODEL_TIMEOUT_SECONDS") {
        for provider in config.providers.values_mut() {
            provider.timeout_seconds = Some(timeout_seconds);
        }
    }

    if let Some(retry_attempts) = env_u8("LUCKCODE_MODEL_RETRY_ATTEMPTS") {
        for provider in config.providers.values_mut() {
            provider.retry_attempts = Some(retry_attempts);
        }
    }
}

fn env_u64(name: &str) -> Option<u64> {
    env::var(name).ok()?.parse().ok()
}

fn env_u8(name: &str) -> Option<u8> {
    env::var(name).ok()?.parse().ok()
}

fn parse_permission_mode(value: &str) -> Result<PermissionMode> {
    match value {
        "plan" => Ok(PermissionMode::Plan),
        "manual" => Ok(PermissionMode::Manual),
        "accept-edits" => Ok(PermissionMode::AcceptEdits),
        "auto" => Ok(PermissionMode::Auto),
        "sandbox" => Ok(PermissionMode::Sandbox),
        "dangerous" => Ok(PermissionMode::Dangerous),
        _ => anyhow::bail!("unknown permission mode: {value}"),
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PartialAppConfig {
    model: Option<PartialModelConfig>,
    providers: Option<BTreeMap<String, PartialProviderConfig>>,
    permission: Option<PartialPermissionConfig>,
    workspace: Option<PartialWorkspaceConfig>,
    ui: Option<PartialUiConfig>,
    project: Option<ProjectConfig>,
    commands: Option<CommandConfig>,
}

impl PartialAppConfig {
    fn apply_to(self, config: &mut AppConfig) {
        if let Some(model) = self.model {
            if let Some(provider) = model.provider {
                config.model.provider = provider;
            }
            if let Some(model_name) = model.model {
                config.model.model = model_name;
            }
        }

        if let Some(providers) = self.providers {
            for (name, partial) in providers {
                let entry = config.providers.entry(name).or_default();
                partial.apply_to(entry);
            }
        }

        if let Some(permission) = self.permission
            && let Some(mode) = permission.mode
        {
            config.permission.mode = mode;
        }

        if let Some(workspace) = self.workspace {
            if let Some(max_file_size) = workspace.max_file_size {
                config.workspace.max_file_size = max_file_size;
            }
            if let Some(ignore) = workspace.ignore {
                config.workspace.ignore = ignore;
            }
        }

        if let Some(ui) = self.ui {
            if let Some(stream) = ui.stream {
                config.ui.stream = stream;
            }
            if let Some(show_tool_calls) = ui.show_tool_calls {
                config.ui.show_tool_calls = show_tool_calls;
            }
        }

        if self.project.is_some() {
            config.project = self.project;
        }

        if self.commands.is_some() {
            config.commands = self.commands;
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PartialModelConfig {
    provider: Option<String>,
    model: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PartialProviderConfig {
    kind: Option<String>,
    model: Option<String>,
    base_url: Option<String>,
    api_key_env: Option<String>,
    request_format: Option<String>,
    timeout_seconds: Option<u64>,
    retry_attempts: Option<u8>,
    enabled: Option<bool>,
}

impl PartialProviderConfig {
    fn apply_to(self, config: &mut ProviderConfig) {
        if let Some(kind) = self.kind {
            config.kind = Some(kind);
        }
        if let Some(model) = self.model {
            config.model = Some(model);
        }
        if let Some(base_url) = self.base_url {
            config.base_url = Some(base_url);
        }
        if let Some(api_key_env) = self.api_key_env {
            config.api_key_env = Some(api_key_env);
        }
        if let Some(request_format) = self.request_format {
            config.request_format = Some(request_format);
        }
        if let Some(timeout_seconds) = self.timeout_seconds {
            config.timeout_seconds = Some(timeout_seconds);
        }
        if let Some(retry_attempts) = self.retry_attempts {
            config.retry_attempts = Some(retry_attempts);
        }
        if let Some(enabled) = self.enabled {
            config.enabled = enabled;
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PartialPermissionConfig {
    mode: Option<PermissionMode>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PartialWorkspaceConfig {
    max_file_size: Option<u64>,
    ignore: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PartialUiConfig {
    stream: Option<bool>,
    show_tool_calls: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InitOptions {
    pub force: bool,
}

#[derive(Debug, Clone)]
pub struct InitReport {
    pub files: Vec<InitFile>,
}

#[derive(Debug, Clone)]
pub struct InitFile {
    pub path: PathBuf,
    pub action: InitFileAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitFileAction {
    Created,
    Overwritten,
    Skipped,
}

pub fn init_project(workspace_root: impl AsRef<Path>, options: InitOptions) -> Result<InitReport> {
    let root = workspace_root.as_ref();
    let files = vec![
        write_text(root.join("AGENTS.md"), AGENTS_TEMPLATE, options.force)?,
        write_text(
            root.join(".luckcode").join("config.toml"),
            PROJECT_CONFIG_TEMPLATE,
            options.force,
        )?,
        write_text(
            root.join(".luckcode").join("mcp.json"),
            MCP_CONFIG_TEMPLATE,
            options.force,
        )?,
        write_text(
            root.join(".luckcode").join("ignore"),
            IGNORE_TEMPLATE,
            options.force,
        )?,
    ];

    Ok(InitReport { files })
}

fn write_text(path: PathBuf, content: &str, force: bool) -> Result<InitFile> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create directory {}", parent.display()))?;
    }

    let existed = path.exists();
    if existed && !force {
        return Ok(InitFile {
            path,
            action: InitFileAction::Skipped,
        });
    }

    fs::write(&path, content).with_context(|| format!("failed to write {}", path.display()))?;

    Ok(InitFile {
        path,
        action: if existed {
            InitFileAction::Overwritten
        } else {
            InitFileAction::Created
        },
    })
}

const AGENTS_TEMPLATE: &str = r#"# LuckCode Project Rules

- Run the configured test command after modifying code.
- Do not create git commits unless the user asks for them.
- Do not read `.env`, private keys, or credentials.
- Do not run `sudo`.
- Do not run destructive infrastructure commands such as `terraform apply` or `terraform destroy`.
- Show shell commands to the user before executing them.
"#;

const PROJECT_CONFIG_TEMPLATE: &str = r#"[project]
name = "luckcode-project"
language = "rust"

[model]
provider = "mock"
model = "mock"

[providers.mock]
kind = "mock"
model = "mock"

[providers.openai]
kind = "openai"
model = "gpt-4.1"
request_format = "chat-completions"
api_key_env = "OPENAI_API_KEY"
base_url = "https://api.openai.com/v1"
timeout_seconds = 120
retry_attempts = 2
enabled = true

[providers.responses]
kind = "openai"
model = "gpt-4.1"
request_format = "responses"
api_key_env = "OPENAI_API_KEY"
base_url = "https://api.openai.com/v1"
timeout_seconds = 120
retry_attempts = 2
enabled = true

[providers.anthropic]
kind = "anthropic"
model = "claude-sonnet-4-5"
api_key_env = "ANTHROPIC_API_KEY"
base_url = "https://api.anthropic.com"
timeout_seconds = 120
retry_attempts = 2
enabled = true

[commands]
test = "cargo test"
check = "cargo check"
lint = "cargo clippy"

[commands.policy]
# One of: allow, ask, deny. Hard-denied commands are still refused first.
default_policy = "ask"
allowlist = ["git status", "git diff"]
denylist = []

[permission]
mode = "manual"
"#;

const MCP_CONFIG_TEMPLATE: &str = r#"{
  "mcpServers": {}
}
"#;

const IGNORE_TEMPLATE: &str = r#".git
node_modules
target
dist
build
.env
*.pem
*.key
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_uses_manual_permission() {
        assert_eq!(AppConfig::default().permission.mode, PermissionMode::Manual);
    }

    #[test]
    fn permission_mode_round_trips_as_kebab_case() {
        let config = AppConfig {
            permission: PermissionConfig {
                mode: PermissionMode::AcceptEdits,
            },
            ..AppConfig::default()
        };

        let text = config_to_toml(&config).expect("serialize config");
        assert!(text.contains("mode = \"accept-edits\""));
    }

    #[test]
    fn resolves_default_mock_provider() {
        let config = AppConfig::default();
        let resolved = resolve_provider_config(&config, None, None).expect("resolve provider");

        assert_eq!(resolved.name, "mock");
        assert_eq!(resolved.kind, "mock");
        assert_eq!(resolved.model, "mock");
    }

    #[test]
    fn resolves_named_provider_profile() {
        let mut config = AppConfig::default();
        config.providers.insert(
            "local".to_string(),
            ProviderConfig {
                kind: Some("openai".to_string()),
                model: Some("qwen2.5-coder".to_string()),
                base_url: Some("http://localhost:11434/v1".to_string()),
                api_key_env: Some("LOCAL_LLM_API_KEY".to_string()),
                request_format: Some("chat-completions".to_string()),
                enabled: true,
                ..ProviderConfig::default()
            },
        );

        let resolved =
            resolve_provider_config(&config, Some("local"), None).expect("resolve provider");

        assert_eq!(resolved.name, "local");
        assert_eq!(resolved.kind, "openai");
        assert_eq!(resolved.model, "qwen2.5-coder");
        assert_eq!(
            resolved.base_url.as_deref(),
            Some("http://localhost:11434/v1")
        );
        assert_eq!(resolved.api_key_env.as_deref(), Some("LOCAL_LLM_API_KEY"));
    }

    #[test]
    fn provider_http_options_merge_from_partial_config() {
        let partial: PartialAppConfig = toml::from_str(
            r#"
            [providers.openai]
            kind = "openai"
            timeout_seconds = 30
            retry_attempts = 4
            "#,
        )
        .expect("parse partial config");
        let mut config = AppConfig::default();

        partial.apply_to(&mut config);
        let resolved =
            resolve_provider_config(&config, Some("openai"), None).expect("resolve provider");

        assert_eq!(resolved.timeout_seconds, Some(30));
        assert_eq!(resolved.retry_attempts, Some(4));
    }

    #[test]
    fn command_policy_merges_from_config() {
        let partial: PartialAppConfig = toml::from_str(
            r#"
            [commands]
            test = "cargo test"

            [commands.policy]
            default_policy = "deny"
            allowlist = ["cargo test"]
            denylist = ["npm publish"]
            "#,
        )
        .expect("parse partial config");
        let mut config = AppConfig::default();

        partial.apply_to(&mut config);
        let commands = config.commands.expect("commands configured");

        assert_eq!(commands.test.as_deref(), Some("cargo test"));
        assert_eq!(
            commands.policy.default_policy,
            Some(luckcode_tools::CommandPolicy::Deny)
        );
        assert_eq!(commands.policy.allowlist, vec!["cargo test"]);
        assert_eq!(commands.policy.denylist, vec!["npm publish"]);
    }

    #[test]
    fn resolves_provider_model_shorthand() {
        let config = AppConfig {
            model: ModelConfig {
                provider: "mock".to_string(),
                model: "anthropic/claude-sonnet-4-5".to_string(),
            },
            ..AppConfig::default()
        };

        let resolved = resolve_provider_config(&config, None, None).expect("resolve provider");

        assert_eq!(resolved.name, "anthropic");
        assert_eq!(resolved.kind, "anthropic");
        assert_eq!(resolved.model, "claude-sonnet-4-5");
    }

    #[test]
    fn compact_summary_extracts_session_activity() {
        let events = vec![
            serde_json::json!({ "type": "user", "content": "fix tests" }),
            serde_json::json!({ "type": "assistant", "content": "I will inspect files." }),
            serde_json::json!({
                "type": "tool_call",
                "name": "read_file",
                "args": { "path": "Cargo.toml" }
            }),
            serde_json::json!({
                "type": "tool_call",
                "name": "edit_file",
                "args": { "path": "src/lib.rs" }
            }),
            serde_json::json!({
                "type": "tool_call",
                "name": "run_shell",
                "args": { "command": "cargo test" }
            }),
            serde_json::json!({
                "type": "tool_result",
                "name": "run_shell",
                "metadata": { "error": true },
                "truncated": true
            }),
        ];

        let summary = build_compact_summary(&events);

        assert!(summary.contains("任务目标:\nfix tests"));
        assert!(summary.contains("- Cargo.toml"));
        assert!(summary.contains("- src/lib.rs"));
        assert!(summary.contains("- cargo test"));
        assert!(summary.contains("run_shell output was truncated"));
        assert!(summary.contains("run_shell returned an error"));
    }

    #[test]
    fn verification_runs_only_after_real_file_changes() {
        let changed_edit = ExecutedToolCall {
            name: "edit_file".to_string(),
            ok: true,
            message_content: String::new(),
            metadata: serde_json::json!({ "replaced": 1 }),
        };
        let skipped_write = ExecutedToolCall {
            name: "write_file".to_string(),
            ok: true,
            message_content: String::new(),
            metadata: serde_json::json!({ "skipped": true }),
        };
        let noop_edit = ExecutedToolCall {
            name: "edit_file".to_string(),
            ok: true,
            message_content: String::new(),
            metadata: serde_json::json!({ "replaced": 0 }),
        };
        let read_file = ExecutedToolCall {
            name: "read_file".to_string(),
            ok: true,
            message_content: String::new(),
            metadata: serde_json::json!({}),
        };

        assert!(should_verify_after_tool(&changed_edit));
        assert!(!should_verify_after_tool(&skipped_write));
        assert!(!should_verify_after_tool(&noop_edit));
        assert!(!should_verify_after_tool(&read_file));
    }

    #[test]
    fn verification_tool_call_uses_run_shell_shape() {
        let verification = VerificationOptions {
            command: "cargo test".to_string(),
            timeout_seconds: 30,
            max_output_bytes: 1024,
        };

        let call = verification_tool_call(&verification);

        assert_eq!(call.name, "run_shell");
        assert_eq!(call.arguments["command"], "cargo test");
        assert_eq!(call.arguments["timeout_seconds"], 30);
        assert_eq!(call.arguments["max_output_bytes"], 1024);
    }

    #[test]
    fn project_context_includes_workspace_commands_and_source_overview() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let root = tmp.path();
        fs::create_dir_all(root.join("src")).expect("create src");
        fs::write(root.join("Cargo.toml"), "[package]\nname = \"demo\"\n").expect("write manifest");
        fs::write(root.join("src/lib.rs"), "pub fn run() {}\n").expect("write source");
        fs::write(root.join(".env"), "SECRET=hidden\n").expect("write sensitive file");

        let context = build_project_context(root, 200_000).expect("build context");

        assert!(context.contains("- Detected project types: Rust"));
        assert!(context.contains("- Workspace top-level:"));
        assert!(context.contains("- src/"));
        assert!(context.contains("- Command hints:"));
        assert!(context.contains("test: cargo test"));
        assert!(context.contains("- Source overview:"));
        assert!(context.contains("src/lib.rs"));
        assert!(!context.contains(".env"));
        assert!(!context.contains("SECRET=hidden"));
    }

    #[test]
    fn mcp_config_parses_servers_without_exposing_env_semantics() {
        let config: McpConfig = serde_json::from_str(
            r#"{
              "mcpServers": {
                "local": {
                  "command": "node",
                  "args": ["server.js"],
                  "env": { "API_KEY": "secret" },
                  "headers": { "Authorization": "Bearer secret" },
                  "tool_policies": { "lookup": "allow", "delete": "deny" }
                }
              }
            }"#,
        )
        .expect("parse mcp config");

        let server = config.servers.get("local").expect("server exists");
        assert_eq!(server.command.as_deref(), Some("node"));
        assert_eq!(server.args, vec!["server.js"]);
        assert_eq!(
            server.env.get("API_KEY").map(String::as_str),
            Some("secret")
        );
        assert_eq!(
            server.headers.get("Authorization").map(String::as_str),
            Some("Bearer secret")
        );
        assert_eq!(
            server.tool_policies.get("lookup"),
            Some(&CommandPolicy::Allow)
        );
        assert_eq!(
            server.tool_policies.get("delete"),
            Some(&CommandPolicy::Deny)
        );
    }

    #[test]
    fn parses_mcp_tools_list_result() {
        let result = serde_json::json!({
            "tools": [
                {
                    "name": "lookup",
                    "description": "Lookup a value",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "key": { "type": "string" }
                        }
                    }
                }
            ]
        });

        let tools = parse_mcp_tools_result(&result).expect("parse tools");

        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "lookup");
        assert_eq!(tools[0].description.as_deref(), Some("Lookup a value"));
        assert_eq!(tools[0].input_schema["type"], "object");
    }

    #[test]
    fn mcp_response_id_matches_numeric_and_string_ids() {
        assert!(mcp_id_matches(&serde_json::json!({ "id": 2 }), 2));
        assert!(mcp_id_matches(&serde_json::json!({ "id": "2" }), 2));
        assert!(!mcp_id_matches(&serde_json::json!({ "id": 3 }), 2));
    }

    #[test]
    fn parses_mcp_http_json_and_sse_responses() {
        let json =
            parse_mcp_http_response_body(r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[]}}"#, 2)
                .expect("parse json response");
        assert_eq!(json["tools"], serde_json::json!([]));

        let sse = parse_mcp_http_response_body(
            "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":\"2\",\"result\":{\"ok\":true}}\n\n",
            2,
        )
        .expect("parse sse response");
        assert_eq!(sse["ok"], true);
    }

    #[test]
    fn mcp_tool_local_names_are_sanitized() {
        assert_eq!(
            mcp_tool_local_name("Local Server", "lookup.value"),
            "mcp_local_server_lookup_value"
        );
    }

    #[test]
    fn mcp_tool_authorization_uses_command_confirmation() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let ctx = ToolContext::new(tmp.path(), 200_000)
            .with_command_approval(CommandApproval::Prompt)
            .with_confirm_command(std::sync::Arc::new(|preview| {
                assert_eq!(preview.command, "mcp local lookup");
                false
            }));
        let server = McpServerConfig::default();

        let outcome = authorize_mcp_tool(&ctx, &server, "local", "lookup", 30, 10_000)
            .expect("authorization should prompt");

        assert!(matches!(outcome, McpToolOutcome::Skipped { .. }));
    }

    #[test]
    fn mcp_tool_authorization_uses_per_tool_policy() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let mut server = McpServerConfig::default();
        server
            .tool_policies
            .insert("lookup".to_string(), CommandPolicy::Allow);
        let ctx =
            ToolContext::new(tmp.path(), 200_000).with_command_approval(CommandApproval::Prompt);

        let outcome = authorize_mcp_tool(&ctx, &server, "local", "lookup", 30, 10_000)
            .expect("tool policy should allow");

        assert!(matches!(
            outcome,
            McpToolOutcome::Run {
                prompted: false,
                ..
            }
        ));
    }

    #[test]
    fn mcp_tool_authorization_uses_configured_denylist() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let ctx = ToolContext::new(tmp.path(), 200_000)
            .with_command_approval(CommandApproval::Auto)
            .with_command_policy(CommandPolicyConfig {
                default_policy: None,
                allowlist: Vec::new(),
                denylist: vec!["mcp local lookup".to_string()],
            });
        let server = McpServerConfig::default();

        let error = authorize_mcp_tool(&ctx, &server, "local", "lookup", 30, 10_000).unwrap_err();

        assert!(format!("{error:#}").contains("configured pattern"));
    }
}
