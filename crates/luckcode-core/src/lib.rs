use anyhow::{Context, Result};
use futures_util::StreamExt;
use luckcode_model::{
    Message, MessageRole, ModelEvent, ModelProvider, ModelRequest, ToolCall as ModelToolCall,
    ToolSchema,
};
use luckcode_storage::{
    SessionInfo, append_session_message, append_session_tool_call, append_session_tool_result,
};
use luckcode_tools::{ToolCall as LocalToolCall, ToolContext, ToolRegistry};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Debug, Clone)]
pub struct AgentOptions {
    pub max_steps: usize,
    pub stream: bool,
}

impl Default for AgentOptions {
    fn default() -> Self {
        Self {
            max_steps: 8,
            stream: true,
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

pub async fn run_readonly_agent(
    task: &str,
    workspace_root: &Path,
    session: &SessionInfo,
    model: &dyn ModelProvider,
    tools: &ToolRegistry,
    tool_context: ToolContext,
    options: AgentOptions,
) -> Result<AgentResult> {
    let mut messages = initial_messages(task, workspace_root, tool_context.max_file_size)?;
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

        for call in tool_calls {
            let record = execute_tool_call(session, tools, tool_context.clone(), call).await?;
            messages.push(Message {
                role: MessageRole::Tool,
                content: record.message_content.clone(),
            });
            tool_records.push(AgentToolCallRecord {
                name: record.name,
                ok: record.ok,
            });
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
            })
        }
    }
}

fn initial_messages(task: &str, workspace_root: &Path, max_file_size: u64) -> Result<Vec<Message>> {
    let mut system = String::from(
        "You are LuckCode, a local Rust CLI coding agent. \
         In this phase you may only use readonly tools. \
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

    if let Some(status) = git_status_short(workspace_root) {
        if !status.trim().is_empty() {
            out.push_str("- Git status:\n");
            out.push_str(&indent_block(&status, "  "));
        }
    }

    if let Some(diff_stat) = git_diff_stat(workspace_root) {
        if !diff_stat.trim().is_empty() {
            out.push_str("- Git diff stat:\n");
            out.push_str(&indent_block(&diff_stat, "  "));
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
    let model = if model_override.is_some() {
        root_model
    } else if root_model != default_model || provider_name == "mock" {
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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionMode {
    Plan,
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

impl Default for PermissionMode {
    fn default() -> Self {
        Self::Manual
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommandConfig {
    pub test: Option<String>,
    pub check: Option<String>,
    pub lint: Option<String>,
}

#[derive(Debug, Clone)]
pub struct LoadedConfig {
    pub config: AppConfig,
    pub sources: Vec<ConfigSource>,
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

    if let Ok(value) = env::var("LUCKCODE_PERMISSION_MODE") {
        if let Ok(mode) = parse_permission_mode(&value) {
            config.permission.mode = mode;
        }
    }
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

        if let Some(permission) = self.permission {
            if let Some(mode) = permission.mode {
                config.permission.mode = mode;
            }
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
enabled = true

[providers.responses]
kind = "openai"
model = "gpt-4.1"
request_format = "responses"
api_key_env = "OPENAI_API_KEY"
base_url = "https://api.openai.com/v1"
enabled = true

[providers.anthropic]
kind = "anthropic"
model = "claude-sonnet-4-5"
api_key_env = "ANTHROPIC_API_KEY"
base_url = "https://api.anthropic.com"
enabled = true

[commands]
test = "cargo test"
check = "cargo check"
lint = "cargo clippy"

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
}
