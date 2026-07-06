use anyhow::{Context, Result};
use async_trait::async_trait;
use ignore::WalkBuilder;
use serde::Deserialize;
use serde_json::{Value, json};
use similar::{ChangeTag, TextDiff};
use std::{
    collections::HashMap,
    fmt, fs,
    path::{Component, Path, PathBuf},
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

/// How an editing tool decides whether to apply a change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditApproval {
    /// Editing is disabled (e.g. `--plan`). The tool bails.
    Refuse,
    /// Show the diff and require user confirmation before writing.
    Prompt,
    /// Apply without prompting (e.g. `--accept-edits`).
    Auto,
}

/// A diff preview handed to the confirmation callback.
#[derive(Debug, Clone)]
pub struct EditPreview {
    pub path: PathBuf,
    pub diff: String,
    pub additions: usize,
    pub deletions: usize,
}

pub type ConfirmEdit = Arc<dyn Fn(&EditPreview) -> bool + Send + Sync>;
pub type CreateCheckpoint = Arc<dyn Fn(&[PathBuf]) -> Result<String> + Send + Sync>;

#[derive(Clone)]
pub struct ToolContext {
    pub workspace_root: PathBuf,
    pub max_file_size: u64,
    pub edit_approval: EditApproval,
    pub confirm_edit: Option<ConfirmEdit>,
    pub create_checkpoint: Option<CreateCheckpoint>,
}

impl fmt::Debug for ToolContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolContext")
            .field("workspace_root", &self.workspace_root)
            .field("max_file_size", &self.max_file_size)
            .field("edit_approval", &self.edit_approval)
            .field("confirm_edit", &self.confirm_edit.is_some())
            .field("create_checkpoint", &self.create_checkpoint.is_some())
            .finish()
    }
}

impl ToolContext {
    pub fn new(workspace_root: impl Into<PathBuf>, max_file_size: u64) -> Self {
        Self {
            workspace_root: workspace_root.into(),
            max_file_size,
            edit_approval: EditApproval::Refuse,
            confirm_edit: None,
            create_checkpoint: None,
        }
    }

    pub fn with_edit_approval(mut self, approval: EditApproval) -> Self {
        self.edit_approval = approval;
        self
    }

    pub fn with_confirm_edit(mut self, confirm: ConfirmEdit) -> Self {
        self.confirm_edit = Some(confirm);
        self
    }

    pub fn with_create_checkpoint(mut self, create: CreateCheckpoint) -> Self {
        self.create_checkpoint = Some(create);
        self
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

fn register_readonly(registry: &mut ToolRegistry) {
    registry.register(ListFilesTool);
    registry.register(ReadFileTool);
    registry.register(SearchFilesTool);
    registry.register(DetectProjectTool);
    registry.register(GitStatusTool);
    registry.register(GitDiffTool);
}

fn register_mutating(registry: &mut ToolRegistry) {
    registry.register(EditFileTool);
    registry.register(WriteFileTool);
}

/// Tools that only read the workspace; safe to expose in `--plan` mode.
pub fn readonly_registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    register_readonly(&mut registry);
    registry
}

/// Tools that mutate files; never exposed in `--plan` mode.
pub fn mutating_registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    register_mutating(&mut registry);
    registry
}

/// All built-in tools (readonly + mutating).
pub fn full_registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    register_readonly(&mut registry);
    register_mutating(&mut registry);
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

pub struct EditFileTool;

#[async_trait]
impl Tool for EditFileTool {
    fn name(&self) -> &'static str {
        "edit_file"
    }

    fn description(&self) -> &'static str {
        "Replace an exact substring in an existing workspace file with a new substring."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["path", "old_string", "new_string"],
            "properties": {
                "path": { "type": "string", "description": "Workspace-relative file path." },
                "old_string": { "type": "string", "description": "Exact text to replace; must be unique unless replace_all is true." },
                "new_string": { "type": "string", "description": "Replacement text." },
                "replace_all": { "type": "boolean", "default": false }
            }
        })
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        let input: EditFileInput =
            serde_json::from_value(input).context("invalid edit_file input")?;
        if input.old_string.is_empty() {
            anyhow::bail!("old_string must not be empty");
        }

        let path = resolve_existing_path(&ctx, &input.path)?;
        if !path.is_file() {
            anyhow::bail!("{} is not a file", path.display());
        }
        if is_sensitive_path(&path) {
            anyhow::bail!("refusing to edit sensitive file {}", path.display());
        }

        let root = canonical_workspace_root(&ctx)?;
        let rel = relative_path(&path, &root);

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

        let old_text = fs::read_to_string(&path)
            .with_context(|| format!("failed to read UTF-8 file {}", path.display()))?;
        let replace_all = input.replace_all.unwrap_or(false);
        let matches = old_text.matches(&input.old_string).count();
        if matches == 0 {
            anyhow::bail!("old_string was not found in {}", path.display());
        }
        if !replace_all && matches > 1 {
            anyhow::bail!(
                "old_string matches {matches} times in {}; set replace_all=true to replace all",
                path.display()
            );
        }

        let new_text = if replace_all {
            old_text.replace(&input.old_string, &input.new_string)
        } else {
            old_text.replacen(&input.old_string, &input.new_string, 1)
        };

        if new_text == old_text {
            return Ok(ToolOutput {
                content: format!("no changes: old_string and new_string are equivalent in {rel}\n"),
                metadata: json!({
                    "path": rel,
                    "replaced": 0,
                    "additions": 0,
                    "deletions": 0,
                }),
                truncated: false,
            });
        }

        let (diff, additions, deletions) = render_diff(&old_text, &new_text, &rel);
        let preview = EditPreview {
            path: path.clone(),
            diff: diff.clone(),
            additions,
            deletions,
        };

        match authorize_edit(&ctx, &preview, std::slice::from_ref(&path))? {
            EditOutcome::Skipped => Ok(ToolOutput {
                content: format!("edit skipped by user: {rel}\n"),
                metadata: json!({ "path": rel, "skipped": true }),
                truncated: false,
            }),
            EditOutcome::Apply { checkpoint_id } => {
                fs::write(&path, &new_text)
                    .with_context(|| format!("failed to write {}", path.display()))?;
                Ok(ToolOutput {
                    content: diff,
                    metadata: json!({
                        "path": rel,
                        "checkpoint_id": checkpoint_id,
                        "replaced": if replace_all { matches } else { 1 },
                        "additions": additions,
                        "deletions": deletions,
                    }),
                    truncated: false,
                })
            }
        }
    }
}

#[derive(Debug, Deserialize)]
struct EditFileInput {
    path: String,
    old_string: String,
    new_string: String,
    replace_all: Option<bool>,
}

pub struct WriteFileTool;

#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &'static str {
        "write_file"
    }

    fn description(&self) -> &'static str {
        "Create a new UTF-8 text file in the workspace. Existing files cannot be overwritten."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["path", "content"],
            "properties": {
                "path": { "type": "string", "description": "Workspace-relative path for the new file." },
                "content": { "type": "string", "description": "Full file contents to write." }
            }
        })
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        let input: WriteFileInput =
            serde_json::from_value(input).context("invalid write_file input")?;
        if input.path.is_empty() {
            anyhow::bail!("path must not be empty");
        }

        let path = resolve_new_path(&ctx, &input.path)?;
        if path.exists() {
            anyhow::bail!(
                "{} already exists; use edit_file to modify existing files",
                path.display()
            );
        }
        if is_sensitive_path(&path) {
            anyhow::bail!("refusing to write sensitive file {}", path.display());
        }
        if input.content.len() as u64 > ctx.max_file_size {
            anyhow::bail!(
                "content is too large: {} bytes exceeds limit {}",
                input.content.len(),
                ctx.max_file_size
            );
        }

        let root = canonical_workspace_root(&ctx)?;
        let rel = relative_path(&path, &root);
        let (diff, additions, deletions) = render_diff("", &input.content, &rel);
        let preview = EditPreview {
            path: path.clone(),
            diff: diff.clone(),
            additions,
            deletions,
        };

        match authorize_edit(&ctx, &preview, std::slice::from_ref(&path))? {
            EditOutcome::Skipped => Ok(ToolOutput {
                content: format!("write skipped by user: {rel}\n"),
                metadata: json!({ "path": rel, "skipped": true }),
                truncated: false,
            }),
            EditOutcome::Apply { checkpoint_id } => {
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent).with_context(|| {
                        format!("failed to create directory {}", parent.display())
                    })?;
                }
                fs::write(&path, &input.content)
                    .with_context(|| format!("failed to write {}", path.display()))?;
                Ok(ToolOutput {
                    content: format!("created {rel}\n{diff}"),
                    metadata: json!({
                        "path": rel,
                        "checkpoint_id": checkpoint_id,
                        "bytes": input.content.len(),
                        "additions": additions,
                        "deletions": deletions,
                    }),
                    truncated: false,
                })
            }
        }
    }
}

#[derive(Debug, Deserialize)]
struct WriteFileInput {
    path: String,
    content: String,
}

enum EditOutcome {
    Apply { checkpoint_id: Option<String> },
    Skipped,
}

/// Apply the configured approval policy and (if allowed) create a checkpoint.
fn authorize_edit(
    ctx: &ToolContext,
    preview: &EditPreview,
    files: &[PathBuf],
) -> Result<EditOutcome> {
    match ctx.edit_approval {
        EditApproval::Refuse => anyhow::bail!("editing is disabled in plan mode"),
        EditApproval::Auto => Ok(EditOutcome::Apply {
            checkpoint_id: create_checkpoint_for(ctx, files)?,
        }),
        EditApproval::Prompt => {
            let Some(confirm) = &ctx.confirm_edit else {
                anyhow::bail!(
                    "interactive confirmation is unavailable; rerun without --plan or pass --accept-edits"
                );
            };
            if !confirm(preview) {
                return Ok(EditOutcome::Skipped);
            }
            Ok(EditOutcome::Apply {
                checkpoint_id: create_checkpoint_for(ctx, files)?,
            })
        }
    }
}

fn create_checkpoint_for(ctx: &ToolContext, files: &[PathBuf]) -> Result<Option<String>> {
    match &ctx.create_checkpoint {
        Some(create) => Ok(Some(create(files)?)),
        None => Ok(None),
    }
}

fn relative_path(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

/// Render a unified-style +/- diff of two texts. Returns the diff and the addition/deletion counts.
fn render_diff(old: &str, new: &str, path: &str) -> (String, usize, usize) {
    let diff = TextDiff::from_lines(old, new);
    let mut out = String::new();
    out.push_str(&format!("--- a/{path}\n+++ b/{path}\n"));
    let mut additions = 0;
    let mut deletions = 0;

    for change in diff.iter_all_changes() {
        let (sign, is_add) = match change.tag() {
            ChangeTag::Equal => (' ', None),
            ChangeTag::Delete => ('-', Some(false)),
            ChangeTag::Insert => ('+', Some(true)),
        };
        out.push(sign);
        out.push_str(change.value());
        if !change.value().ends_with('\n') {
            out.push('\n');
        }
        match is_add {
            Some(true) => additions += 1,
            Some(false) => deletions += 1,
            None => {}
        }
    }

    (out, additions, deletions)
}

fn resolve_new_path(ctx: &ToolContext, requested: &str) -> Result<PathBuf> {
    let root = canonical_workspace_root(ctx)?;
    let requested = Path::new(requested);
    if requested
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        anyhow::bail!("path '{}' must not contain '..'", requested.display());
    }

    let candidate = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        root.join(requested)
    };

    if !candidate.starts_with(&root) {
        anyhow::bail!(
            "path {} is outside workspace {}",
            candidate.display(),
            root.display()
        );
    }

    Ok(candidate)
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

#[cfg(test)]
mod tests {
    use super::*;

    const MAX_FILE_SIZE: u64 = 200_000;

    fn auto_ctx(root: &Path) -> ToolContext {
        ToolContext::new(root, MAX_FILE_SIZE)
            .with_edit_approval(EditApproval::Auto)
            .with_create_checkpoint(Arc::new(|_| Ok("ckpt_test".to_string())))
    }

    async fn write_file(root: &Path, rel: &str, content: &str) -> PathBuf {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, content).unwrap();
        path
    }

    #[tokio::test]
    async fn edit_file_replaces_unique_substring() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        write_file(&root, "src/main.rs", "fn main() {}\n").await;

        let output = EditFileTool
            .execute(
                json!({
                    "path": "src/main.rs",
                    "old_string": "fn main() {}",
                    "new_string": "fn main() { println!(\"hi\"); }"
                }),
                auto_ctx(&root),
            )
            .await
            .expect("edit succeeds");

        assert_eq!(
            fs::read_to_string(root.join("src/main.rs")).unwrap(),
            "fn main() { println!(\"hi\"); }\n"
        );
        assert!(output.content.contains("--- a/src/main.rs"));
        assert!(output.content.contains("+fn main() { println!("));
        assert_eq!(output.metadata["replaced"], 1);
        assert_eq!(output.metadata["checkpoint_id"], "ckpt_test");
    }

    #[tokio::test]
    async fn edit_file_rejects_ambiguous_match_without_replace_all() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        write_file(&root, "a.txt", "dup\ndup\n").await;

        let err = EditFileTool
            .execute(
                json!({ "path": "a.txt", "old_string": "dup", "new_string": "x" }),
                auto_ctx(&root),
            )
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("matches 2 times"));
    }

    #[tokio::test]
    async fn edit_file_replace_all_replaces_every_match() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        write_file(&root, "a.txt", "dup\ndup\n").await;

        let output = EditFileTool
            .execute(
                json!({
                    "path": "a.txt",
                    "old_string": "dup",
                    "new_string": "x",
                    "replace_all": true
                }),
                auto_ctx(&root),
            )
            .await
            .unwrap();
        assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "x\nx\n");
        assert_eq!(output.metadata["replaced"], 2);
    }

    #[tokio::test]
    async fn edit_file_refuses_in_plan_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        write_file(&root, "a.txt", "hello\n").await;

        let ctx = ToolContext::new(&root, MAX_FILE_SIZE).with_edit_approval(EditApproval::Refuse);
        let err = EditFileTool
            .execute(
                json!({ "path": "a.txt", "old_string": "hello", "new_string": "world" }),
                ctx,
            )
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("disabled in plan mode"));
        assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "hello\n");
    }

    #[tokio::test]
    async fn edit_file_respects_skipped_confirmation() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        write_file(&root, "a.txt", "hello\n").await;

        let ctx = ToolContext::new(&root, MAX_FILE_SIZE)
            .with_edit_approval(EditApproval::Prompt)
            .with_confirm_edit(Arc::new(|_| false));
        let output = EditFileTool
            .execute(
                json!({ "path": "a.txt", "old_string": "hello", "new_string": "world" }),
                ctx,
            )
            .await
            .unwrap();
        assert!(output.content.contains("skipped by user"));
        assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "hello\n");
    }

    #[tokio::test]
    async fn write_file_creates_new_file() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();

        let output = WriteFileTool
            .execute(
                json!({ "path": "src/new.rs", "content": "pub fn x() {}\n" }),
                auto_ctx(&root),
            )
            .await
            .unwrap();
        assert!(root.join("src/new.rs").is_file());
        assert_eq!(
            fs::read_to_string(root.join("src/new.rs")).unwrap(),
            "pub fn x() {}\n"
        );
        assert!(output.content.contains("created src/new.rs"));
        assert_eq!(output.metadata["checkpoint_id"], "ckpt_test");
    }

    #[tokio::test]
    async fn write_file_refuses_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        write_file(&root, "a.txt", "old\n").await;

        let err = WriteFileTool
            .execute(
                json!({ "path": "a.txt", "content": "new\n" }),
                auto_ctx(&root),
            )
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("already exists"));
        assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "old\n");
    }

    #[tokio::test]
    async fn edit_file_refuses_sensitive_path() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        write_file(&root, ".env", "SECRET=1\n").await;

        let err = EditFileTool
            .execute(
                json!({ "path": ".env", "old_string": "SECRET=1", "new_string": "x" }),
                auto_ctx(&root),
            )
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("sensitive file"));
    }

    #[tokio::test]
    async fn edit_file_rejects_path_outside_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        write_file(&root, "a.txt", "hello\n").await;

        let err = EditFileTool
            .execute(
                json!({ "path": "../escape.txt", "old_string": "x", "new_string": "y" }),
                auto_ctx(&root),
            )
            .await
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("outside workspace")
                || format!("{err:#}").contains("failed to resolve")
        );
    }
}
