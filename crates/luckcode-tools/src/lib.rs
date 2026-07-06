use anyhow::{Context, Result};
use async_trait::async_trait;
use ignore::WalkBuilder;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
};

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;

    fn description(&self) -> &'static str;

    fn schema(&self) -> Value;

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput>;
}

#[derive(Debug, Clone)]
pub struct ToolContext {
    pub workspace_root: PathBuf,
    pub max_file_size: u64,
}

impl ToolContext {
    pub fn new(workspace_root: impl Into<PathBuf>, max_file_size: u64) -> Self {
        Self {
            workspace_root: workspace_root.into(),
            max_file_size,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ToolCall {
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub content: String,
    pub metadata: Value,
    pub truncated: bool,
}

#[derive(Debug, Clone)]
pub struct ToolInfo {
    pub name: &'static str,
    pub description: &'static str,
    pub schema: Value,
}

#[derive(Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register<T: Tool + 'static>(&mut self, tool: T) {
        self.tools.insert(tool.name().to_string(), Arc::new(tool));
    }

    pub fn list(&self) -> Vec<ToolInfo> {
        let mut tools = self
            .tools
            .values()
            .map(|tool| ToolInfo {
                name: tool.name(),
                description: tool.description(),
                schema: tool.schema(),
            })
            .collect::<Vec<_>>();
        tools.sort_by_key(|tool| tool.name);
        tools
    }

    pub async fn execute(&self, call: ToolCall, ctx: ToolContext) -> Result<ToolOutput> {
        let tool = self
            .tools
            .get(&call.name)
            .with_context(|| format!("unknown tool: {}", call.name))?;

        tool.execute(call.arguments, ctx).await
    }
}

pub fn readonly_registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(ListFilesTool);
    registry.register(ReadFileTool);
    registry.register(SearchFilesTool);
    registry.register(DetectProjectTool);
    registry.register(GitStatusTool);
    registry.register(GitDiffTool);
    registry
}

pub struct ListFilesTool;

#[async_trait]
impl Tool for ListFilesTool {
    fn name(&self) -> &'static str {
        "list_files"
    }

    fn description(&self) -> &'static str {
        "List files under a workspace-relative path."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "default": "." },
                "max_depth": { "type": "integer", "minimum": 1 },
                "limit": { "type": "integer", "minimum": 1, "default": 200 }
            }
        })
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        let input: ListFilesInput =
            serde_json::from_value(input).context("invalid list_files input")?;
        let target = resolve_existing_path(&ctx, input.path.as_deref().unwrap_or("."))?;

        if !target.is_dir() {
            anyhow::bail!("{} is not a directory", target.display());
        }

        let root = canonical_workspace_root(&ctx)?;
        let limit = input.limit.unwrap_or(200);
        let mut builder = WalkBuilder::new(&target);
        builder
            .hidden(false)
            .parents(true)
            .git_ignore(true)
            .git_exclude(true);

        if let Some(max_depth) = input.max_depth {
            builder.max_depth(Some(max_depth));
        }

        let mut entries = Vec::new();
        let mut truncated = false;

        for result in builder.build() {
            let entry = result.context("failed to walk files")?;
            let path = entry.path();

            if path == target {
                continue;
            }

            if entries.len() >= limit {
                truncated = true;
                break;
            }

            let rel = path.strip_prefix(&root).unwrap_or(path);
            let mut display = rel.display().to_string();
            if path.is_dir() {
                display.push('/');
            }
            entries.push(display);
        }

        Ok(ToolOutput {
            content: entries.join("\n"),
            metadata: json!({ "count": entries.len() }),
            truncated,
        })
    }
}

#[derive(Debug, Deserialize)]
struct ListFilesInput {
    path: Option<String>,
    max_depth: Option<usize>,
    limit: Option<usize>,
}

pub struct ReadFileTool;

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &'static str {
        "read_file"
    }

    fn description(&self) -> &'static str {
        "Read a UTF-8 text file from the workspace."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["path"],
            "properties": {
                "path": { "type": "string" },
                "offset": { "type": "integer", "minimum": 0, "default": 0 },
                "limit": { "type": "integer", "minimum": 1, "default": 400 }
            }
        })
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        let input: ReadFileInput =
            serde_json::from_value(input).context("invalid read_file input")?;
        let path = resolve_existing_path(&ctx, &input.path)?;

        if !path.is_file() {
            anyhow::bail!("{} is not a file", path.display());
        }

        let metadata = fs::metadata(&path)
            .with_context(|| format!("failed to read metadata for {}", path.display()))?;
        if metadata.len() > ctx.max_file_size {
            anyhow::bail!(
                "{} is too large: {} bytes exceeds limit {}",
                path.display(),
                metadata.len(),
                ctx.max_file_size
            );
        }

        let text = fs::read_to_string(&path)
            .with_context(|| format!("failed to read UTF-8 file {}", path.display()))?;
        let offset = input.offset.unwrap_or(0);
        let limit = input.limit.unwrap_or(400);
        let lines = text.lines().collect::<Vec<_>>();
        let total_lines = lines.len();
        let selected = lines
            .iter()
            .enumerate()
            .skip(offset)
            .take(limit)
            .map(|(idx, line)| format!("{:>6} {}", idx + 1, line))
            .collect::<Vec<_>>();

        Ok(ToolOutput {
            content: selected.join("\n"),
            metadata: json!({
                "bytes": metadata.len(),
                "total_lines": total_lines,
                "offset": offset,
                "limit": limit
            }),
            truncated: offset + selected.len() < total_lines,
        })
    }
}

#[derive(Debug, Deserialize)]
struct ReadFileInput {
    path: String,
    offset: Option<usize>,
    limit: Option<usize>,
}

pub struct SearchFilesTool;

#[async_trait]
impl Tool for SearchFilesTool {
    fn name(&self) -> &'static str {
        "search_files"
    }

    fn description(&self) -> &'static str {
        "Search workspace text files with a plain substring query."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["query"],
            "properties": {
                "query": { "type": "string" },
                "path": { "type": "string", "default": "." },
                "limit": { "type": "integer", "minimum": 1, "default": 100 }
            }
        })
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        let input: SearchFilesInput =
            serde_json::from_value(input).context("invalid search_files input")?;
        let target = resolve_existing_path(&ctx, input.path.as_deref().unwrap_or("."))?;
        let root = canonical_workspace_root(&ctx)?;
        let limit = input.limit.unwrap_or(100);

        let mut matches = Vec::new();
        let mut truncated = false;
        let mut builder = WalkBuilder::new(&target);
        builder
            .hidden(false)
            .parents(true)
            .git_ignore(true)
            .git_exclude(true);

        for result in builder.build() {
            let entry = result.context("failed to walk files")?;
            let path = entry.path();

            if !path.is_file() {
                continue;
            }

            let metadata = match fs::metadata(path) {
                Ok(metadata) => metadata,
                Err(_) => continue,
            };

            if metadata.len() > ctx.max_file_size {
                continue;
            }

            let Ok(text) = fs::read_to_string(path) else {
                continue;
            };

            let rel = path.strip_prefix(&root).unwrap_or(path);
            for (idx, line) in text.lines().enumerate() {
                if !line.contains(&input.query) {
                    continue;
                }

                if matches.len() >= limit {
                    truncated = true;
                    break;
                }

                matches.push(format!("{}:{}:{}", rel.display(), idx + 1, line));
            }

            if truncated {
                break;
            }
        }

        Ok(ToolOutput {
            content: matches.join("\n"),
            metadata: json!({ "count": matches.len() }),
            truncated,
        })
    }
}

#[derive(Debug, Deserialize)]
struct SearchFilesInput {
    query: String,
    path: Option<String>,
    limit: Option<usize>,
}

pub struct DetectProjectTool;

#[async_trait]
impl Tool for DetectProjectTool {
    fn name(&self) -> &'static str {
        "detect_project"
    }

    fn description(&self) -> &'static str {
        "Detect project languages, manifests, and common project files."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "include_previews": { "type": "boolean", "default": false }
            }
        })
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        let input: DetectProjectInput =
            serde_json::from_value(input).context("invalid detect_project input")?;
        let root = canonical_workspace_root(&ctx)?;
        let project_types = detect_project_types(&root);
        let important_files = important_project_files(&root);

        let mut content = String::new();
        if project_types.is_empty() {
            content.push_str("Project types: unknown\n");
        } else {
            content.push_str("Project types: ");
            content.push_str(&project_types.join(", "));
            content.push('\n');
        }

        content.push_str("Important files:\n");
        for path in &important_files {
            let rel = path.strip_prefix(&root).unwrap_or(path);
            content.push_str("- ");
            content.push_str(&rel.display().to_string());
            content.push('\n');
        }

        if input.include_previews.unwrap_or(false) {
            for path in important_files.iter().take(4) {
                if is_sensitive_path(path) {
                    continue;
                }
                let Some(preview) = read_preview(path, ctx.max_file_size, 4_000)? else {
                    continue;
                };
                if preview.trim().is_empty() {
                    continue;
                }
                let rel = path.strip_prefix(&root).unwrap_or(path);
                content.push_str("\nPreview: ");
                content.push_str(&rel.display().to_string());
                content.push('\n');
                content.push_str(&preview);
                if !content.ends_with('\n') {
                    content.push('\n');
                }
            }
        }

        Ok(ToolOutput {
            content,
            metadata: json!({
                "project_types": project_types,
                "important_file_count": important_files.len(),
            }),
            truncated: false,
        })
    }
}

#[derive(Debug, Deserialize)]
struct DetectProjectInput {
    include_previews: Option<bool>,
}

pub struct GitStatusTool;

#[async_trait]
impl Tool for GitStatusTool {
    fn name(&self) -> &'static str {
        "git_status"
    }

    fn description(&self) -> &'static str {
        "Show git status for the workspace."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "short": { "type": "boolean", "default": true }
            }
        })
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        let input: GitStatusInput =
            serde_json::from_value(input).context("invalid git_status input")?;
        let mut command = Command::new("git");
        command.arg("status");
        if input.short.unwrap_or(true) {
            command.arg("--short");
        }

        run_git_command(command, &ctx, "git status")
    }
}

#[derive(Debug, Deserialize)]
struct GitStatusInput {
    short: Option<bool>,
}

pub struct GitDiffTool;

#[async_trait]
impl Tool for GitDiffTool {
    fn name(&self) -> &'static str {
        "git_diff"
    }

    fn description(&self) -> &'static str {
        "Show git diff for the workspace."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "staged": { "type": "boolean", "default": false }
            }
        })
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        let input: GitDiffInput =
            serde_json::from_value(input).context("invalid git_diff input")?;
        let mut command = Command::new("git");
        command.arg("diff");
        if input.staged.unwrap_or(false) {
            command.arg("--staged");
        }

        run_git_command(command, &ctx, "git diff")
    }
}

#[derive(Debug, Deserialize)]
struct GitDiffInput {
    staged: Option<bool>,
}

fn run_git_command(mut command: Command, ctx: &ToolContext, label: &str) -> Result<ToolOutput> {
    let root = canonical_workspace_root(ctx)?;
    let output = command
        .current_dir(root)
        .output()
        .with_context(|| format!("failed to run {label}"))?;

    if !output.status.success() {
        anyhow::bail!(
            "{} failed: {}",
            label,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    Ok(ToolOutput {
        content: stdout,
        metadata: json!({
            "exit_code": output.status.code(),
            "stderr": String::from_utf8_lossy(&output.stderr).trim()
        }),
        truncated: false,
    })
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

fn important_project_files(root: &Path) -> Vec<PathBuf> {
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

fn is_sensitive_path(path: &Path) -> bool {
    let text = path.to_string_lossy();
    text.contains(".env")
        || text.ends_with(".pem")
        || text.ends_with(".key")
        || text.ends_with("id_rsa")
        || text.ends_with("id_ed25519")
}

fn canonical_workspace_root(ctx: &ToolContext) -> Result<PathBuf> {
    ctx.workspace_root
        .canonicalize()
        .with_context(|| format!("failed to canonicalize {}", ctx.workspace_root.display()))
}

fn resolve_existing_path(ctx: &ToolContext, requested: &str) -> Result<PathBuf> {
    let root = canonical_workspace_root(ctx)?;
    let requested = Path::new(requested);
    let candidate = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        root.join(requested)
    };

    let canonical = candidate
        .canonicalize()
        .with_context(|| format!("failed to resolve {}", candidate.display()))?;

    if !canonical.starts_with(&root) {
        anyhow::bail!(
            "path {} is outside workspace {}",
            canonical.display(),
            root.display()
        );
    }

    Ok(canonical)
}
