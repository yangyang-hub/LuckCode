use anyhow::{Context, Result};
use clap::{CommandFactory, Parser, Subcommand};
use futures_util::StreamExt;
use luckcode_core::{
    AgentOptions, AppConfig, InitFileAction, InitOptions, PermissionMode, ResolvedProviderConfig,
    call_mcp_tool, compact_session, compact_summary_for_session, config_to_toml, init_project,
    list_mcp_tools, load_config, load_mcp_config, resolve_provider_config, run_agent,
};
use luckcode_model::{
    AnthropicProvider, Message, MessageRole, MockProvider, ModelEvent, ModelHttpOptions,
    ModelProvider, ModelRequest, ModelRequestFormat, OpenAiCompatibleProvider,
    is_anthropic_provider, is_openai_compatible_provider,
};
use luckcode_storage::{
    ProjectInfo, SessionInfo, append_session_checkpoint, append_session_message, create_checkpoint,
    create_session_jsonl, latest_checkpoint, read_project_memory, read_session_events,
    remove_project_memory, restore_checkpoint, session_exists, session_jsonl_path, sessions_root,
    set_project_memory,
};
use luckcode_tools::{
    AnnounceCommand, CommandApproval, CommandPolicyConfig, CommandPreview, ConfirmCommand,
    CreateCheckpoint, EditApproval, EditPreview, ToolCall, ToolContext, full_registry,
    readonly_registry,
};
use std::{
    env, fs,
    io::{self, Write},
    path::PathBuf,
    process::Command,
    sync::Arc,
};
use tracing::Level;

#[derive(Debug, Parser)]
#[command(name = "luckcode")]
#[command(version, about = "Local CLI coding agent written in Rust.")]
struct Cli {
    #[arg(long, global = true)]
    debug: bool,

    #[arg(short, long, global = true)]
    verbose: bool,

    #[arg(long)]
    plan: bool,

    #[arg(long = "accept-edits")]
    accept_edits: bool,

    #[arg(long)]
    sandbox: bool,

    #[arg(long, value_name = "SESSION_ID", num_args = 0..=1, default_missing_value = "")]
    resume: Option<String>,

    #[arg(long)]
    diff: bool,

    #[arg(long)]
    compact: bool,

    #[arg(long, global = true, value_name = "PROVIDER")]
    provider: Option<String>,

    #[arg(long, global = true, value_name = "MODEL")]
    model: Option<String>,

    #[command(subcommand)]
    command: Option<Commands>,

    #[arg(value_name = "PROMPT")]
    prompt: Vec<String>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    Init {
        #[arg(long)]
        force: bool,
    },
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    Tools {
        #[command(subcommand)]
        command: ToolsCommand,
    },
    Run {
        #[arg(value_name = "PROMPT")]
        prompt: Vec<String>,
    },
    Ask {
        #[arg(value_name = "PROMPT")]
        prompt: Vec<String>,
    },
    Providers {
        #[command(subcommand)]
        command: ProviderCommand,
    },
    Symbols {
        #[arg(value_name = "PATH", default_value = ".")]
        path: String,
        #[arg(long, default_value_t = 200)]
        limit: usize,
    },
    Session {
        #[command(subcommand)]
        command: Option<SessionCommand>,
    },
    Memory {
        #[command(subcommand)]
        command: MemoryCommand,
    },
    Mcp {
        #[command(subcommand)]
        command: McpCommand,
    },
    Restore {
        #[arg(value_name = "CHECKPOINT_ID")]
        checkpoint_id: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    Show,
}

#[derive(Debug, Subcommand)]
enum ToolsCommand {
    List,
    Call { name: String, input: String },
}

#[derive(Debug, Subcommand)]
enum ProviderCommand {
    List,
}

#[derive(Debug, Subcommand)]
enum SessionCommand {
    List,
    Show {
        #[arg(value_name = "SESSION_ID")]
        session_id: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum MemoryCommand {
    Show,
    Set { key: String, value: String },
    Remove { key: String },
}

#[derive(Debug, Subcommand)]
enum McpCommand {
    List,
    Show {
        name: String,
    },
    Tools {
        server: String,
        #[arg(long, default_value_t = 30)]
        timeout_seconds: u64,
    },
    Call {
        server: String,
        tool: String,
        input: String,
        #[arg(long, default_value_t = 30)]
        timeout_seconds: u64,
    },
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.debug, cli.verbose);

    if cli.accept_edits && cli.sandbox {
        anyhow::bail!("--accept-edits and --sandbox cannot be used together");
    }

    if cli.diff {
        return print_git_diff();
    }

    if cli.compact {
        return handle_compact(cli.resume.as_deref());
    }

    if let Some(session_id) = &cli.resume {
        if cli.command.is_some() {
            anyhow::bail!("--resume cannot be combined with subcommands");
        }
        return run_resumed_prompt(
            cli.prompt,
            session_id,
            cli.plan,
            cli.accept_edits,
            cli.sandbox,
            cli.provider,
            cli.model,
        )
        .await;
    }

    match cli.command {
        Some(Commands::Init { force }) => handle_init(force),
        Some(Commands::Config { command }) => handle_config(command),
        Some(Commands::Tools { command }) => handle_tools(command).await,
        Some(Commands::Run { prompt }) => {
            run_prompt(
                prompt,
                cli.plan,
                cli.accept_edits,
                cli.sandbox,
                cli.provider,
                cli.model,
            )
            .await
        }
        Some(Commands::Ask { prompt }) => handle_ask(cli.provider, cli.model, prompt).await,
        Some(Commands::Providers { command }) => handle_providers(command),
        Some(Commands::Symbols { path, limit }) => handle_symbols(path, limit).await,
        Some(Commands::Session { command }) => handle_session(command),
        Some(Commands::Memory { command }) => handle_memory(command),
        Some(Commands::Mcp { command }) => handle_mcp(command).await,
        Some(Commands::Restore { checkpoint_id }) => handle_restore(checkpoint_id),
        None if !cli.prompt.is_empty() => {
            run_prompt(
                cli.prompt,
                cli.plan,
                cli.accept_edits,
                cli.sandbox,
                cli.provider,
                cli.model,
            )
            .await
        }
        None => {
            Cli::command().print_help()?;
            println!();
            Ok(())
        }
    }
}

fn init_tracing(debug: bool, verbose: bool) {
    let level = if debug {
        Level::DEBUG
    } else if verbose {
        Level::INFO
    } else {
        Level::WARN
    };

    tracing_subscriber::fmt()
        .with_max_level(level)
        .without_time()
        .init();
}

fn handle_init(force: bool) -> Result<()> {
    let cwd = env::current_dir().context("failed to read current directory")?;
    let report = init_project(&cwd, InitOptions { force })?;

    for file in report.files {
        let action = match file.action {
            InitFileAction::Created => "created",
            InitFileAction::Overwritten => "overwritten",
            InitFileAction::Skipped => "skipped",
        };
        println!("{action}: {}", file.path.display());
    }

    Ok(())
}

fn handle_config(command: ConfigCommand) -> Result<()> {
    match command {
        ConfigCommand::Show => {
            let cwd = env::current_dir().context("failed to read current directory")?;
            let loaded = load_config(&cwd)?;

            println!("{}", config_to_toml(&loaded.config)?);
            println!("# sources");
            for source in loaded.sources {
                let status = if source.loaded { "loaded" } else { "missing" };
                println!("# {status}: {}", source.path.display());
            }

            Ok(())
        }
    }
}

async fn handle_tools(command: ToolsCommand) -> Result<()> {
    let cwd = env::current_dir().context("failed to read current directory")?;
    let loaded = load_config(&cwd)?;
    let registry = full_registry();

    match command {
        ToolsCommand::List => {
            for tool in registry.list() {
                println!("{}\t{}", tool.name, tool.description);
            }
            Ok(())
        }
        ToolsCommand::Call { name, input } => {
            let arguments =
                serde_json::from_str(&input).context("tool input must be valid JSON")?;
            // Editing tools prompt via stdin; readonly tools ignore the approval policy.
            let ctx = ToolContext::new(cwd, loaded.config.workspace.max_file_size)
                .with_edit_approval(EditApproval::Prompt)
                .with_command_approval(CommandApproval::Prompt)
                .with_command_policy(command_policy_from_config(&loaded.config))
                .with_confirm_edit(Arc::new(confirm_edit_stdin))
                .with_confirm_command(Arc::new(confirm_command_stdin));
            let output = registry.execute(ToolCall { name, arguments }, ctx).await?;

            println!("{}", output.content);
            if output.truncated {
                println!("\n[truncated]");
            }
            Ok(())
        }
    }
}

async fn run_prompt(
    prompt: Vec<String>,
    plan: bool,
    accept_edits: bool,
    sandbox: bool,
    provider_override: Option<String>,
    model_override: Option<String>,
) -> Result<()> {
    let prompt = prompt.join(" ");
    if prompt.trim().is_empty() {
        anyhow::bail!("prompt cannot be empty");
    }

    let cwd = env::current_dir().context("failed to read current directory")?;
    let loaded = load_config(&cwd)?;
    let resolved = resolve_provider_config(
        &loaded.config,
        provider_override.as_deref(),
        model_override.as_deref(),
    )?;
    let provider = build_agent_provider(&resolved)?;
    let project = ProjectInfo::discover(&cwd)?;
    let session = SessionInfo::new(&project);
    let session_path = create_session_jsonl(&session, &prompt)?;

    let permission = resolve_permission(plan, accept_edits, sandbox, loaded.config.permission.mode);
    let registry = if permission.edit_approval == EditApproval::Refuse
        && permission.command_approval == CommandApproval::Refuse
    {
        readonly_registry()
    } else {
        full_registry()
    };

    println!("mode: {}", permission.mode_label);
    println!("session: {}", session.id);
    println!("session_path: {}", session_path.display());
    println!("project_hash: {}", project.hash);
    println!("model: {}/{}", resolved.name, resolved.model);
    println!();

    let tool_context = build_tool_context(
        cwd.clone(),
        loaded.config.workspace.max_file_size,
        permission.edit_approval,
        permission.command_approval,
        command_policy_from_config(&loaded.config),
        &session,
    );
    let result = run_agent(
        &prompt,
        &cwd,
        &session,
        provider.as_ref(),
        &registry,
        tool_context,
        AgentOptions {
            max_steps: 8,
            stream: loaded.config.ui.stream,
            resume_summary: None,
        },
    )
    .await?;

    print!("{}", result.final_answer);
    if !result.final_answer.ends_with('\n') {
        println!();
    }

    if loaded.config.ui.show_tool_calls {
        println!("\n工具调用:");
        for call in result.tool_calls {
            let status = if call.ok { "ok" } else { "error" };
            println!("- {} ({status})", call.name);
        }
    }

    if let Some(reason) = result.stopped_reason {
        println!("\n停止原因: {reason}");
    }

    Ok(())
}

async fn run_resumed_prompt(
    prompt: Vec<String>,
    requested_session_id: &str,
    plan: bool,
    accept_edits: bool,
    sandbox: bool,
    provider_override: Option<String>,
    model_override: Option<String>,
) -> Result<()> {
    let prompt = prompt.join(" ");
    let cwd = env::current_dir().context("failed to read current directory")?;
    let loaded = load_config(&cwd)?;
    let resolved = resolve_provider_config(
        &loaded.config,
        provider_override.as_deref(),
        model_override.as_deref(),
    )?;
    let project = ProjectInfo::discover(&cwd)?;
    let session_id = resolve_session_id(&project.hash, requested_session_id)?;
    if !session_exists(&project.hash, &session_id)? {
        anyhow::bail!("session {session_id} does not exist for this project");
    }

    let summary = compact_summary_for_session(&project.hash, &session_id)?;
    let session_path = session_jsonl_path(&project.hash, &session_id)?;
    if prompt.trim().is_empty() {
        println!("session: {session_id}");
        println!("session_path: {}", session_path.display());
        println!();
        print!("{summary}");
        return Ok(());
    }

    let provider = build_agent_provider(&resolved)?;
    let session = existing_session(&project, session_id);
    append_session_message(&session, "user", &prompt)?;

    let permission = resolve_permission(plan, accept_edits, sandbox, loaded.config.permission.mode);
    let registry = if permission.edit_approval == EditApproval::Refuse
        && permission.command_approval == CommandApproval::Refuse
    {
        readonly_registry()
    } else {
        full_registry()
    };

    println!("mode: {}", permission.mode_label);
    println!("session: {}", session.id);
    println!("session_path: {}", session_path.display());
    println!("project_hash: {}", project.hash);
    println!("model: {}/{}", resolved.name, resolved.model);
    println!("resumed: true");
    println!();

    let tool_context = build_tool_context(
        cwd.clone(),
        loaded.config.workspace.max_file_size,
        permission.edit_approval,
        permission.command_approval,
        command_policy_from_config(&loaded.config),
        &session,
    );
    let result = run_agent(
        &prompt,
        &cwd,
        &session,
        provider.as_ref(),
        &registry,
        tool_context,
        AgentOptions {
            max_steps: 8,
            stream: loaded.config.ui.stream,
            resume_summary: Some(summary),
        },
    )
    .await?;

    print!("{}", result.final_answer);
    if !result.final_answer.ends_with('\n') {
        println!();
    }

    if loaded.config.ui.show_tool_calls {
        println!("\n工具调用:");
        for call in result.tool_calls {
            let status = if call.ok { "ok" } else { "error" };
            println!("- {} ({status})", call.name);
        }
    }

    if let Some(reason) = result.stopped_reason {
        println!("\n停止原因: {reason}");
    }

    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct ResolvedPermission {
    mode_label: &'static str,
    edit_approval: EditApproval,
    command_approval: CommandApproval,
}

/// Resolve the displayed mode label and tool approval policies from CLI flags + config.
fn resolve_permission(
    plan: bool,
    accept_edits: bool,
    sandbox: bool,
    configured: PermissionMode,
) -> ResolvedPermission {
    if plan {
        return ResolvedPermission {
            mode_label: "plan",
            edit_approval: EditApproval::Refuse,
            command_approval: CommandApproval::Refuse,
        };
    }
    if accept_edits {
        return ResolvedPermission {
            mode_label: "accept-edits",
            edit_approval: EditApproval::Auto,
            command_approval: CommandApproval::Prompt,
        };
    }
    if sandbox {
        return ResolvedPermission {
            mode_label: "sandbox",
            edit_approval: EditApproval::Refuse,
            command_approval: CommandApproval::Prompt,
        };
    }
    match configured {
        PermissionMode::Plan => ResolvedPermission {
            mode_label: "plan",
            edit_approval: EditApproval::Refuse,
            command_approval: CommandApproval::Refuse,
        },
        PermissionMode::Manual => ResolvedPermission {
            mode_label: "manual",
            edit_approval: EditApproval::Prompt,
            command_approval: CommandApproval::Prompt,
        },
        PermissionMode::AcceptEdits => ResolvedPermission {
            mode_label: "accept-edits",
            edit_approval: EditApproval::Auto,
            command_approval: CommandApproval::Prompt,
        },
        PermissionMode::Auto => ResolvedPermission {
            mode_label: "auto",
            edit_approval: EditApproval::Auto,
            command_approval: CommandApproval::Auto,
        },
        PermissionMode::Sandbox => ResolvedPermission {
            mode_label: "sandbox",
            edit_approval: EditApproval::Refuse,
            command_approval: CommandApproval::Prompt,
        },
        PermissionMode::Dangerous => ResolvedPermission {
            mode_label: "dangerous",
            edit_approval: EditApproval::Auto,
            command_approval: CommandApproval::Auto,
        },
    }
}

fn build_tool_context(
    cwd: PathBuf,
    max_file_size: u64,
    edit_approval: EditApproval,
    command_approval: CommandApproval,
    command_policy: CommandPolicyConfig,
    session: &SessionInfo,
) -> ToolContext {
    let mut ctx = ToolContext::new(cwd, max_file_size)
        .with_edit_approval(edit_approval)
        .with_command_approval(command_approval)
        .with_command_policy(command_policy);
    if edit_approval != EditApproval::Refuse {
        let session_for_checkpoint = session.clone();
        let create_checkpoint: CreateCheckpoint = Arc::new(move |files: &[PathBuf]| {
            let id = create_checkpoint(&session_for_checkpoint, files)?;
            append_session_checkpoint(&session_for_checkpoint, &id)?;
            Ok(id)
        });
        ctx = ctx
            .with_create_checkpoint(create_checkpoint)
            .with_confirm_edit(Arc::new(confirm_edit_stdin));
    }
    match command_approval {
        CommandApproval::Refuse => {}
        CommandApproval::Prompt => {
            let confirm: ConfirmCommand = Arc::new(confirm_command_stdin);
            ctx = ctx.with_confirm_command(confirm);
        }
        CommandApproval::Auto => {
            let announce: AnnounceCommand = Arc::new(announce_command_stdout);
            ctx = ctx.with_announce_command(announce);
        }
    }
    ctx
}

fn command_policy_from_config(config: &AppConfig) -> CommandPolicyConfig {
    config
        .commands
        .as_ref()
        .map(|commands| commands.policy.clone())
        .unwrap_or_default()
}

/// Print an edit preview and ask the user whether to apply it.
fn confirm_edit_stdin(preview: &EditPreview) -> bool {
    println!("\n--- proposed edit: {} ---", preview.path.display());
    print!("{}", preview.diff);
    println!(
        "--- {} addition(s), {} deletion(s) ---",
        preview.additions, preview.deletions
    );
    print!("apply this change? (y/N): ");
    io::stdout().flush().ok();
    let mut line = String::new();
    let _ = io::stdin().read_line(&mut line);
    matches!(line.trim().to_lowercase().as_str(), "y" | "yes")
}

/// Print a shell command preview and ask the user whether to run it.
fn confirm_command_stdin(preview: &CommandPreview) -> bool {
    println!("\n--- proposed command ---");
    println!("cwd: {}", preview.working_dir.display());
    println!("command: {}", preview.command);
    println!(
        "timeout: {}s; max output: {} bytes",
        preview.timeout_seconds, preview.max_output_bytes
    );
    println!("policy: {}", preview.reason);
    print!("run this command? (y/N): ");
    io::stdout().flush().ok();
    let mut line = String::new();
    let _ = io::stdin().read_line(&mut line);
    matches!(line.trim().to_lowercase().as_str(), "y" | "yes")
}

/// Show a shell command before automatic execution.
fn announce_command_stdout(preview: &CommandPreview) {
    println!("\n--- running command ---");
    println!("cwd: {}", preview.working_dir.display());
    println!("command: {}", preview.command);
}

async fn handle_ask(
    provider_override: Option<String>,
    model_override: Option<String>,
    prompt: Vec<String>,
) -> Result<()> {
    let prompt = prompt.join(" ");
    if prompt.trim().is_empty() {
        anyhow::bail!("ask requires a prompt");
    }

    let cwd = env::current_dir().context("failed to read current directory")?;
    let loaded = load_config(&cwd)?;
    let resolved = resolve_provider_config(
        &loaded.config,
        provider_override.as_deref(),
        model_override.as_deref(),
    )?;
    let provider = build_ask_provider(&resolved, &prompt)?;
    let project = ProjectInfo::discover(&cwd)?;
    let session = SessionInfo::new(&project);
    create_session_jsonl(&session, &prompt)?;

    let request = ModelRequest {
        messages: vec![Message {
            role: MessageRole::User,
            content: prompt,
        }],
        tools: Vec::new(),
        temperature: None,
        max_tokens: None,
    };

    let mut stream = provider.stream(request).await?;
    let mut assistant = String::new();
    while let Some(event) = stream.next().await {
        match event? {
            ModelEvent::TextDelta(delta) => {
                print!("{delta}");
                io::stdout().flush().context("failed to flush stdout")?;
                assistant.push_str(&delta);
            }
            ModelEvent::Done => break,
            ModelEvent::ToolCallDelta(_) | ModelEvent::ToolCallDone(_) => {}
        }
    }

    append_session_message(&session, "assistant", &assistant)?;

    Ok(())
}

fn build_agent_provider(resolved: &ResolvedProviderConfig) -> Result<Box<dyn ModelProvider>> {
    if resolved.kind == "mock" {
        return Ok(Box::new(MockProvider::agent()));
    }

    if is_anthropic_provider(&resolved.kind) {
        return Ok(Box::new(AnthropicProvider::from_env_with_http_options(
            &resolved.model,
            resolved.base_url.as_deref(),
            resolved.api_key_env.as_deref(),
            http_options_for_provider(resolved),
        )?));
    }

    if is_openai_compatible_provider(&resolved.kind) {
        return Ok(Box::new(
            OpenAiCompatibleProvider::from_env_with_http_options(
                &resolved.model,
                openai_request_format_for_provider(resolved)?,
                resolved.base_url.as_deref(),
                resolved.api_key_env.as_deref(),
                http_options_for_provider(resolved),
            )?,
        ));
    }

    anyhow::bail!(
        "model provider '{}' is not implemented yet; use 'mock', 'openai', 'responses', or 'anthropic'",
        resolved.name
    )
}

fn build_ask_provider(
    resolved: &ResolvedProviderConfig,
    prompt: &str,
) -> Result<Box<dyn ModelProvider>> {
    if resolved.kind == "mock" {
        return Ok(Box::new(MockProvider::new(format!(
            "Mock response: {prompt}\n"
        ))));
    }

    if is_anthropic_provider(&resolved.kind) {
        return Ok(Box::new(AnthropicProvider::from_env_with_http_options(
            &resolved.model,
            resolved.base_url.as_deref(),
            resolved.api_key_env.as_deref(),
            http_options_for_provider(resolved),
        )?));
    }

    if is_openai_compatible_provider(&resolved.kind) {
        return Ok(Box::new(
            OpenAiCompatibleProvider::from_env_with_http_options(
                &resolved.model,
                openai_request_format_for_provider(resolved)?,
                resolved.base_url.as_deref(),
                resolved.api_key_env.as_deref(),
                http_options_for_provider(resolved),
            )?,
        ));
    }

    anyhow::bail!(
        "provider '{}' is not implemented yet; use 'mock', 'openai', 'responses', or 'anthropic'",
        resolved.name
    )
}

fn http_options_for_provider(resolved: &ResolvedProviderConfig) -> ModelHttpOptions {
    let defaults = ModelHttpOptions::default();
    ModelHttpOptions {
        timeout_seconds: resolved.timeout_seconds.unwrap_or(defaults.timeout_seconds),
        retry_attempts: resolved.retry_attempts.unwrap_or(defaults.retry_attempts),
    }
    .normalized()
}

fn openai_request_format_for_provider(
    resolved: &ResolvedProviderConfig,
) -> Result<ModelRequestFormat> {
    let raw = resolved
        .request_format
        .clone()
        .or_else(|| env::var("LUCKCODE_MODEL_REQUEST_FORMAT").ok())
        .unwrap_or_else(|| "chat-completions".to_string());
    let format = ModelRequestFormat::parse(&raw)
        .context("request_format must be chat-completions or responses")?;
    match format {
        ModelRequestFormat::OpenAiChatCompletions => Ok(ModelRequestFormat::OpenAiChatCompletions),
        ModelRequestFormat::OpenAiResponses => Ok(ModelRequestFormat::OpenAiResponses),
        ModelRequestFormat::AnthropicMessages => {
            anyhow::bail!("request_format=anthropic requires provider kind 'anthropic'")
        }
    }
}

fn handle_providers(command: ProviderCommand) -> Result<()> {
    match command {
        ProviderCommand::List => {
            let cwd = env::current_dir().context("failed to read current directory")?;
            let loaded = load_config(&cwd)?;
            print_providers(&loaded.config);
            Ok(())
        }
    }
}

async fn handle_symbols(path: String, limit: usize) -> Result<()> {
    let cwd = env::current_dir().context("failed to read current directory")?;
    let loaded = load_config(&cwd)?;
    let output = readonly_registry()
        .execute(
            ToolCall {
                name: "list_symbols".to_string(),
                arguments: serde_json::json!({
                    "path": path,
                    "limit": limit,
                }),
            },
            ToolContext::new(cwd, loaded.config.workspace.max_file_size),
        )
        .await?;

    println!("{}", output.content);
    if output.truncated {
        println!("\n[truncated]");
    }
    Ok(())
}

fn print_providers(config: &AppConfig) {
    let active = resolve_provider_config(config, None, None)
        .map(|provider| provider.name)
        .unwrap_or_else(|_| config.model.provider.clone());
    println!("NAME\tKIND\tMODEL\tFORMAT\tTIMEOUT\tRETRIES\tENABLED\tACTIVE");
    for (name, profile) in &config.providers {
        let inferred = resolve_provider_config(config, Some(name), None).ok();
        let kind = profile
            .kind
            .as_deref()
            .or_else(|| inferred.as_ref().map(|provider| provider.kind.as_str()))
            .unwrap_or("");
        let model = profile
            .model
            .as_deref()
            .or_else(|| inferred.as_ref().map(|provider| provider.model.as_str()))
            .unwrap_or("");
        let format = profile.request_format.as_deref().unwrap_or("");
        let timeout = profile
            .timeout_seconds
            .or_else(|| {
                inferred
                    .as_ref()
                    .and_then(|provider| provider.timeout_seconds)
            })
            .map(|value| value.to_string())
            .unwrap_or_default();
        let retries = profile
            .retry_attempts
            .or_else(|| {
                inferred
                    .as_ref()
                    .and_then(|provider| provider.retry_attempts)
            })
            .map(|value| value.to_string())
            .unwrap_or_default();
        let active_marker = if name == &active { "*" } else { "" };
        println!(
            "{name}\t{kind}\t{model}\t{format}\t{timeout}\t{retries}\t{}\t{active_marker}",
            profile.enabled
        );
    }
}

fn handle_session(command: Option<SessionCommand>) -> Result<()> {
    match command.unwrap_or(SessionCommand::List) {
        SessionCommand::List => {
            let root = sessions_root()?;
            if !root.exists() {
                println!("No sessions found.");
                return Ok(());
            }

            let mut sessions = Vec::new();
            for project_dir in
                fs::read_dir(&root).with_context(|| format!("failed to read {}", root.display()))?
            {
                let project_dir = project_dir?;
                if !project_dir.file_type()?.is_dir() {
                    continue;
                }
                let project_hash = project_dir.file_name().to_string_lossy().to_string();

                for session_file in fs::read_dir(project_dir.path())? {
                    let session_file = session_file?;
                    let path = session_file.path();
                    if path.extension().and_then(|ext| ext.to_str()) == Some("jsonl") {
                        let modified = session_file.metadata()?.modified()?;
                        let session_id = path
                            .file_stem()
                            .and_then(|stem| stem.to_str())
                            .unwrap_or("")
                            .to_string();
                        sessions.push((modified, project_hash.clone(), session_id, path));
                    }
                }
            }

            sessions.sort_by(|a, b| b.0.cmp(&a.0));
            let stdout = io::stdout();
            let mut out = io::BufWriter::new(stdout.lock());
            if !write_stdout_line(&mut out, "SESSION\tPROJECT\tUPDATED\tPATH")? {
                return Ok(());
            }
            for (modified, project_hash, session_id, path) in sessions {
                let updated = format_system_time(modified);
                let line = format!(
                    "{session_id}\t{project_hash}\t{updated}\t{}",
                    path.display()
                );
                if !write_stdout_line(&mut out, &line)? {
                    return Ok(());
                }
            }

            Ok(())
        }
        SessionCommand::Show { session_id } => {
            let cwd = env::current_dir().context("failed to read current directory")?;
            let project = ProjectInfo::discover(&cwd)?;
            let session_id = match session_id {
                Some(session_id) => session_id,
                None => resolve_session_id(&project.hash, "")?,
            };
            let events = read_session_events(&project.hash, &session_id)?;
            println!("session: {session_id}");
            println!("events: {}", events.len());
            println!();
            for (idx, event) in events.iter().enumerate() {
                print_session_event(idx + 1, event);
            }
            Ok(())
        }
    }
}

fn handle_memory(command: MemoryCommand) -> Result<()> {
    let cwd = env::current_dir().context("failed to read current directory")?;
    let project = ProjectInfo::discover(&cwd)?;

    match command {
        MemoryCommand::Show => {
            let memory = read_project_memory(&project.hash)?;
            if memory.is_empty() {
                println!("No project memory for this workspace.");
                return Ok(());
            }
            for (key, value) in memory {
                println!("{key} = {value}");
            }
            Ok(())
        }
        MemoryCommand::Set { key, value } => {
            if key.trim().is_empty() {
                anyhow::bail!("memory key cannot be empty");
            }
            set_project_memory(&project.hash, &key, &value)?;
            println!("set: {key}");
            Ok(())
        }
        MemoryCommand::Remove { key } => {
            if remove_project_memory(&project.hash, &key)? {
                println!("removed: {key}");
            } else {
                println!("not found: {key}");
            }
            Ok(())
        }
    }
}

async fn handle_mcp(command: McpCommand) -> Result<()> {
    let cwd = env::current_dir().context("failed to read current directory")?;
    let loaded = load_mcp_config(&cwd)?;

    match command {
        McpCommand::List => {
            println!("config: {}", loaded.path.display());
            println!(
                "status: {}",
                if loaded.loaded { "loaded" } else { "missing" }
            );
            if loaded.config.servers.is_empty() {
                println!("No MCP servers configured.");
                return Ok(());
            }
            println!("NAME\tTRANSPORT\tSTATUS\tENV_KEYS");
            for (name, server) in loaded.config.servers {
                let transport = if server.url.is_some() {
                    "http"
                } else {
                    "stdio"
                };
                let status = if server.disabled {
                    "disabled"
                } else {
                    "enabled"
                };
                let env_keys = server.env.keys().cloned().collect::<Vec<_>>().join(",");
                println!("{name}\t{transport}\t{status}\t{env_keys}");
            }
            Ok(())
        }
        McpCommand::Show { name } => {
            let Some(server) = loaded.config.servers.get(&name) else {
                anyhow::bail!("MCP server '{name}' is not configured");
            };
            println!("name: {name}");
            println!("disabled: {}", server.disabled);
            if let Some(command) = &server.command {
                println!("command: {command}");
            }
            if !server.args.is_empty() {
                println!("args: {}", server.args.join(" "));
            }
            if let Some(url) = &server.url {
                println!("url: {url}");
            }
            if !server.env.is_empty() {
                println!("env:");
                for key in server.env.keys() {
                    println!("- {key}=<redacted>");
                }
            }
            Ok(())
        }
        McpCommand::Tools {
            server,
            timeout_seconds,
        } => {
            let server_config = loaded
                .config
                .servers
                .get(&server)
                .with_context(|| format!("MCP server '{server}' is not configured"))?;
            let tools = list_mcp_tools(server_config, timeout_seconds).await?;
            if tools.is_empty() {
                println!("No tools reported by MCP server {server}.");
                return Ok(());
            }
            println!("NAME\tDESCRIPTION");
            for tool in tools {
                println!("{}\t{}", tool.name, tool.description.unwrap_or_default());
            }
            Ok(())
        }
        McpCommand::Call {
            server,
            tool,
            input,
            timeout_seconds,
        } => {
            let server_config = loaded
                .config
                .servers
                .get(&server)
                .with_context(|| format!("MCP server '{server}' is not configured"))?;
            let arguments =
                serde_json::from_str(&input).context("MCP tool input must be valid JSON")?;
            let result = call_mcp_tool(server_config, &tool, arguments, timeout_seconds).await?;
            println!(
                "{}",
                serde_json::to_string_pretty(&result)
                    .context("failed to serialize MCP tool result")?
            );
            Ok(())
        }
    }
}

fn handle_compact(requested_session_id: Option<&str>) -> Result<()> {
    let cwd = env::current_dir().context("failed to read current directory")?;
    let project = ProjectInfo::discover(&cwd)?;
    let session_id = resolve_session_id(&project.hash, requested_session_id.unwrap_or(""))?;
    if !session_exists(&project.hash, &session_id)? {
        anyhow::bail!("session {session_id} does not exist for this project");
    }

    let session = existing_session(&project, session_id);
    let compacted = compact_session(&session)?;
    println!("session: {}", compacted.session_id);
    println!("events: {}", compacted.event_count);
    println!();
    print!("{}", compacted.summary);
    Ok(())
}

fn handle_restore(checkpoint_id: Option<String>) -> Result<()> {
    let cwd = env::current_dir().context("failed to read current directory")?;
    let project = ProjectInfo::discover(&cwd)?;

    let Some(session_id) = latest_session_id(&project.hash)? else {
        println!("No sessions found for this project; nothing to restore.");
        return Ok(());
    };

    let checkpoint_id = match checkpoint_id {
        Some(id) => id,
        None => match latest_checkpoint(&project.hash, &session_id)? {
            Some(latest) => latest.id,
            None => {
                println!("No checkpoints found for session {session_id}; nothing to restore.");
                return Ok(());
            }
        },
    };

    let restored = restore_checkpoint(&project.root, &project.hash, &session_id, &checkpoint_id)
        .with_context(|| format!("failed to restore checkpoint {checkpoint_id}"))?;

    println!("restored checkpoint: {checkpoint_id}");
    println!("session: {session_id}");
    for path in restored {
        println!("- {path}");
    }

    Ok(())
}

fn resolve_session_id(project_hash: &str, requested: &str) -> Result<String> {
    if requested.is_empty() {
        return latest_session_id(project_hash)?
            .context("No sessions found for this project; nothing to resume.");
    }
    Ok(requested.to_string())
}

fn existing_session(project: &ProjectInfo, session_id: String) -> SessionInfo {
    SessionInfo::existing(project, session_id)
}

fn print_session_event(index: usize, event: &serde_json::Value) {
    let kind = json_str(event, "type");
    let created_at = json_str(event, "created_at");
    let detail = match kind.as_str() {
        "user" | "assistant" | "compact_summary" => compact_display(json_str(event, "content")),
        "tool_call" => {
            let name = json_str(event, "name");
            let args = event
                .get("args")
                .map(|args| serde_json::to_string(args).unwrap_or_else(|_| "{}".to_string()))
                .unwrap_or_else(|| "{}".to_string());
            compact_display(format!("{name} {args}"))
        }
        "tool_result" => {
            let name = json_str(event, "name");
            let truncated = event
                .get("truncated")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let suffix = if truncated { " [truncated]" } else { "" };
            compact_display(format!("{} {}{}", name, json_str(event, "content"), suffix))
        }
        "checkpoint" => json_str(event, "id"),
        _ => compact_display(event.to_string()),
    };
    println!("{index:>4} {created_at} {kind:<15} {detail}");
}

fn json_str(event: &serde_json::Value, key: &str) -> String {
    event
        .get(key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn compact_display(text: String) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() <= 180 {
        return text;
    }
    let mut out = text.chars().take(180).collect::<String>();
    out.push_str("...");
    out
}

fn write_stdout_line(out: &mut impl Write, line: &str) -> Result<bool> {
    match writeln!(out, "{line}") {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(false),
        Err(error) => Err(error).context("failed to write stdout"),
    }
}

fn format_system_time(time: std::time::SystemTime) -> String {
    let datetime: chrono::DateTime<chrono::Utc> = time.into();
    datetime.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Newest session id for a project, based on JSONL file mtime.
fn latest_session_id(project_hash: &str) -> Result<Option<String>> {
    let dir = sessions_root()?.join(project_hash);
    let Ok(entries) = fs::read_dir(&dir) else {
        return Ok(None);
    };

    let mut newest: Option<(std::time::SystemTime, String)> = None;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
            continue;
        }
        let modified = entry.metadata()?.modified()?;
        let id = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .map(ToOwned::to_owned);
        if let Some(id) = id
            && newest.as_ref().is_none_or(|(prev, _)| modified > *prev)
        {
            newest = Some((modified, id));
        }
    }

    Ok(newest.map(|(_, id)| id))
}

fn print_git_diff() -> Result<()> {
    let output = Command::new("git")
        .arg("diff")
        .output()
        .context("failed to run git diff")?;

    if !output.status.success() {
        anyhow::bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }

    print!("{}", String::from_utf8_lossy(&output.stdout));
    Ok(())
}
