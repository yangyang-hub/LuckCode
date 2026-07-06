use anyhow::{Context, Result};
use clap::{CommandFactory, Parser, Subcommand};
use futures_util::StreamExt;
use luckcode_core::{
    AgentOptions, AppConfig, InitFileAction, InitOptions, PermissionMode, ResolvedProviderConfig,
    config_to_toml, init_project, load_config, resolve_provider_config, run_agent,
};
use luckcode_model::{
    AnthropicProvider, Message, MessageRole, MockProvider, ModelEvent, ModelProvider, ModelRequest,
    ModelRequestFormat, OpenAiCompatibleProvider, is_anthropic_provider,
    is_openai_compatible_provider,
};
use luckcode_storage::{
    ProjectInfo, SessionInfo, append_session_checkpoint, append_session_message, create_checkpoint,
    create_session_jsonl, latest_checkpoint, restore_checkpoint, sessions_root,
};
use luckcode_tools::{
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
    Session {
        #[command(subcommand)]
        command: Option<SessionCommand>,
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

    if cli.diff {
        return print_git_diff();
    }

    if cli.compact {
        println!("Context compaction is planned for v0.4; no compact summary exists yet.");
        return Ok(());
    }

    if let Some(session_id) = &cli.resume {
        let label = if session_id.is_empty() {
            "latest session"
        } else {
            session_id.as_str()
        };
        println!("Session resume is planned for v0.4; requested {label}.");
        return Ok(());
    }

    match cli.command {
        Some(Commands::Init { force }) => handle_init(force),
        Some(Commands::Config { command }) => handle_config(command),
        Some(Commands::Tools { command }) => handle_tools(command).await,
        Some(Commands::Run { prompt }) => {
            run_prompt(prompt, cli.plan, cli.accept_edits, cli.provider, cli.model).await
        }
        Some(Commands::Ask { prompt }) => handle_ask(cli.provider, cli.model, prompt).await,
        Some(Commands::Providers { command }) => handle_providers(command),
        Some(Commands::Session { command }) => handle_session(command),
        Some(Commands::Restore { checkpoint_id }) => handle_restore(checkpoint_id),
        None if !cli.prompt.is_empty() => {
            run_prompt(
                cli.prompt,
                cli.plan,
                cli.accept_edits,
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
                .with_confirm_edit(Arc::new(confirm_edit_stdin));
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

    let (mode, approval) = resolve_approval(plan, accept_edits, loaded.config.permission.mode);
    let registry = if approval == EditApproval::Refuse {
        readonly_registry()
    } else {
        full_registry()
    };

    println!("mode: {mode}");
    println!("session: {}", session.id);
    println!("session_path: {}", session_path.display());
    println!("project_hash: {}", project.hash);
    println!("model: {}/{}", resolved.name, resolved.model);
    println!();

    let tool_context = build_tool_context(
        cwd.clone(),
        loaded.config.workspace.max_file_size,
        approval,
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

/// Resolve the displayed mode label and the [`EditApproval`] policy from CLI flags + config.
fn resolve_approval(
    plan: bool,
    accept_edits: bool,
    configured: PermissionMode,
) -> (&'static str, EditApproval) {
    if plan {
        return ("plan", EditApproval::Refuse);
    }
    if accept_edits {
        return ("accept-edits", EditApproval::Auto);
    }
    match configured {
        PermissionMode::Plan => ("plan", EditApproval::Refuse),
        PermissionMode::Manual => ("manual", EditApproval::Prompt),
        PermissionMode::AcceptEdits => ("accept-edits", EditApproval::Auto),
        PermissionMode::Auto => ("auto", EditApproval::Auto),
        PermissionMode::Dangerous => ("dangerous", EditApproval::Auto),
        PermissionMode::Sandbox => ("sandbox", EditApproval::Refuse),
    }
}

fn build_tool_context(
    cwd: PathBuf,
    max_file_size: u64,
    approval: EditApproval,
    session: &SessionInfo,
) -> ToolContext {
    let mut ctx = ToolContext::new(cwd, max_file_size).with_edit_approval(approval);
    if approval != EditApproval::Refuse {
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
    ctx
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
        return Ok(Box::new(AnthropicProvider::from_env_with_options(
            &resolved.model,
            resolved.base_url.as_deref(),
            resolved.api_key_env.as_deref(),
        )?));
    }

    if is_openai_compatible_provider(&resolved.kind) {
        return Ok(Box::new(OpenAiCompatibleProvider::from_env_with_options(
            &resolved.model,
            openai_request_format_for_provider(resolved)?,
            resolved.base_url.as_deref(),
            resolved.api_key_env.as_deref(),
        )?));
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
        return Ok(Box::new(AnthropicProvider::from_env_with_options(
            &resolved.model,
            resolved.base_url.as_deref(),
            resolved.api_key_env.as_deref(),
        )?));
    }

    if is_openai_compatible_provider(&resolved.kind) {
        return Ok(Box::new(OpenAiCompatibleProvider::from_env_with_options(
            &resolved.model,
            openai_request_format_for_provider(resolved)?,
            resolved.base_url.as_deref(),
            resolved.api_key_env.as_deref(),
        )?));
    }

    anyhow::bail!(
        "provider '{}' is not implemented yet; use 'mock', 'openai', 'responses', or 'anthropic'",
        resolved.name
    )
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

fn print_providers(config: &AppConfig) {
    let active = resolve_provider_config(config, None, None)
        .map(|provider| provider.name)
        .unwrap_or_else(|_| config.model.provider.clone());
    println!("NAME\tKIND\tMODEL\tFORMAT\tENABLED\tACTIVE");
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
        let active_marker = if name == &active { "*" } else { "" };
        println!(
            "{name}\t{kind}\t{model}\t{format}\t{}\t{active_marker}",
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

                for session_file in fs::read_dir(project_dir.path())? {
                    let session_file = session_file?;
                    let path = session_file.path();
                    if path.extension().and_then(|ext| ext.to_str()) == Some("jsonl") {
                        let modified = session_file.metadata()?.modified()?;
                        sessions.push((modified, path));
                    }
                }
            }

            sessions.sort_by_key(|(modified, _)| *modified);
            for (_, path) in sessions {
                println!("{}", path.display());
            }

            Ok(())
        }
    }
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
