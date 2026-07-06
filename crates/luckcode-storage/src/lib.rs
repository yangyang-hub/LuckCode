use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    env,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProjectInfo {
    pub root: PathBuf,
    pub hash: String,
}

impl ProjectInfo {
    pub fn discover(cwd: impl AsRef<Path>) -> Result<Self> {
        let root = cwd
            .as_ref()
            .canonicalize()
            .with_context(|| format!("failed to canonicalize {}", cwd.as_ref().display()))?;

        Ok(Self {
            hash: project_hash(&root),
            root,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionInfo {
    pub id: String,
    pub project_hash: String,
    pub project_path: PathBuf,
    pub created_at: DateTime<Utc>,
}

impl SessionInfo {
    pub fn new(project: &ProjectInfo) -> Self {
        Self {
            id: new_session_id(),
            project_hash: project.hash.clone(),
            project_path: project.root.clone(),
            created_at: Utc::now(),
        }
    }
}

pub fn new_session_id() -> String {
    format!("ses_{}", Uuid::new_v4().simple())
}

pub fn project_hash(path: impl AsRef<Path>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(path.as_ref().to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn config_dir() -> Result<PathBuf> {
    if let Some(path) = env::var_os("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(path).join("luckcode"));
    }

    Ok(home_dir()?.join(".config").join("luckcode"))
}

pub fn config_file() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.toml"))
}

pub fn data_dir() -> Result<PathBuf> {
    if let Some(path) = env::var_os("XDG_DATA_HOME") {
        return Ok(PathBuf::from(path).join("luckcode"));
    }

    Ok(home_dir()?.join(".local").join("share").join("luckcode"))
}

pub fn session_dir(project_hash: &str) -> Result<PathBuf> {
    Ok(data_dir()?.join("sessions").join(project_hash))
}

pub fn sessions_root() -> Result<PathBuf> {
    Ok(data_dir()?.join("sessions"))
}

pub fn session_jsonl_path(project_hash: &str, session_id: &str) -> Result<PathBuf> {
    Ok(session_dir(project_hash)?.join(format!("{session_id}.jsonl")))
}

pub fn checkpoints_root() -> Result<PathBuf> {
    Ok(data_dir()?.join("checkpoints"))
}

pub fn checkpoint_dir(
    project_hash: &str,
    session_id: &str,
    checkpoint_id: &str,
) -> Result<PathBuf> {
    Ok(checkpoints_root()?
        .join(project_hash)
        .join(session_id)
        .join(checkpoint_id))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CheckpointSummary {
    pub id: String,
    pub created_at: DateTime<Utc>,
    pub file_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CheckpointManifest {
    checkpoint_id: String,
    session_id: String,
    project_hash: String,
    created_at: DateTime<Utc>,
    files: Vec<CheckpointFileEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CheckpointFileEntry {
    path: String,
    existed: bool,
}

/// Create a checkpoint capturing the current on-disk contents of `files`.
///
/// Each existing file is copied under `<checkpoint>/files/<sanitized>.before`; files that
/// do not yet exist are recorded as `existed: false` so a later restore can delete them.
/// Returns the new checkpoint id.
pub fn create_checkpoint(session: &SessionInfo, files: &[PathBuf]) -> Result<String> {
    let checkpoint_id = new_checkpoint_id();
    let base = checkpoint_dir(&session.project_hash, &session.id, &checkpoint_id)?;
    let files_dir = base.join("files");
    fs::create_dir_all(&files_dir).with_context(|| {
        format!(
            "failed to create checkpoint directory {}",
            files_dir.display()
        )
    })?;

    let mut entries = Vec::with_capacity(files.len());
    for file in files {
        let relative = file
            .strip_prefix(&session.project_path)
            .unwrap_or(file)
            .to_path_buf();
        let existed = file.is_file();
        if existed {
            let before = files_dir.join(format!("{}.before", sanitize_relative(&relative)));
            fs::copy(file, &before).with_context(|| {
                format!(
                    "failed to checkpoint {} -> {}",
                    file.display(),
                    before.display()
                )
            })?;
        }
        entries.push(CheckpointFileEntry {
            path: relative.to_string_lossy().into_owned(),
            existed,
        });
    }

    let manifest = CheckpointManifest {
        checkpoint_id: checkpoint_id.clone(),
        session_id: session.id.clone(),
        project_hash: session.project_hash.clone(),
        created_at: Utc::now(),
        files: entries,
    };
    let manifest_path = base.join("manifest.json");
    fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest)?).with_context(|| {
        format!(
            "failed to write checkpoint manifest {}",
            manifest_path.display()
        )
    })?;

    Ok(checkpoint_id)
}

/// List checkpoints for a session, newest first.
pub fn list_checkpoints(project_hash: &str, session_id: &str) -> Result<Vec<CheckpointSummary>> {
    let base = checkpoints_root()?.join(project_hash).join(session_id);
    if !base.is_dir() {
        return Ok(Vec::new());
    }

    let mut summaries = Vec::new();
    for entry in
        fs::read_dir(&base).with_context(|| format!("failed to read {}", base.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let manifest_path = entry.path().join("manifest.json");
        let Ok(text) = fs::read_to_string(&manifest_path) else {
            continue;
        };
        let manifest: CheckpointManifest = serde_json::from_str(&text)
            .with_context(|| format!("failed to parse {}", manifest_path.display()))?;
        summaries.push(CheckpointSummary {
            id: manifest.checkpoint_id,
            created_at: manifest.created_at,
            file_count: manifest.files.len(),
        });
    }

    summaries.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Ok(summaries)
}

/// Return the newest checkpoint for a session, if any.
pub fn latest_checkpoint(
    project_hash: &str,
    session_id: &str,
) -> Result<Option<CheckpointSummary>> {
    Ok(list_checkpoints(project_hash, session_id)?
        .into_iter()
        .next())
}

/// Restore a checkpoint by id, rewriting each affected file in `project_root` to its
/// pre-edit state. Returns the relative paths that were restored.
pub fn restore_checkpoint(
    project_root: &Path,
    project_hash: &str,
    session_id: &str,
    checkpoint_id: &str,
) -> Result<Vec<String>> {
    let base = checkpoint_dir(project_hash, session_id, checkpoint_id)?;
    let manifest_path = base.join("manifest.json");
    let text = fs::read_to_string(&manifest_path).with_context(|| {
        format!(
            "failed to read checkpoint manifest {}",
            manifest_path.display()
        )
    })?;
    let manifest: CheckpointManifest =
        serde_json::from_str(&text).context("failed to parse checkpoint manifest")?;
    let files_dir = base.join("files");

    let mut restored = Vec::new();
    for entry in manifest.files {
        let target = project_root.join(&entry.path);
        if entry.existed {
            let before = files_dir.join(format!(
                "{}.before",
                sanitize_relative(Path::new(&entry.path))
            ));
            fs::copy(&before, &target).with_context(|| {
                format!(
                    "failed to restore {} from {}",
                    target.display(),
                    before.display()
                )
            })?;
        } else if target.exists() {
            fs::remove_file(&target)
                .with_context(|| format!("failed to remove {}", target.display()))?;
        }
        restored.push(entry.path);
    }

    Ok(restored)
}

fn new_checkpoint_id() -> String {
    format!(
        "ckpt_{}_{}",
        Utc::now().format("%Y%m%dT%H%M%S"),
        &Uuid::new_v4().simple().to_string()[..6]
    )
}

fn sanitize_relative(path: &Path) -> String {
    path.to_string_lossy().replace(['/', '\\'], "_")
}

pub fn create_session_jsonl(session: &SessionInfo, initial_prompt: &str) -> Result<PathBuf> {
    let path = session_jsonl_path(&session.project_hash, &session.id)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create session directory {}", parent.display()))?;
    }

    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("failed to create session file {}", path.display()))?;

    let event = SessionJsonlEvent {
        kind: "user",
        content: initial_prompt,
        created_at: session.created_at,
    };
    writeln!(file, "{}", serde_json::to_string(&event)?)
        .with_context(|| format!("failed to write session file {}", path.display()))?;

    Ok(path)
}

pub fn append_session_message(session: &SessionInfo, kind: &str, content: &str) -> Result<()> {
    append_session_event(
        session,
        json!({
            "type": kind,
            "content": content,
        }),
    )
}

pub fn append_session_tool_call(session: &SessionInfo, name: &str, args: &Value) -> Result<()> {
    append_session_event(
        session,
        json!({
            "type": "tool_call",
            "name": name,
            "args": args,
        }),
    )
}

pub fn append_session_tool_result(
    session: &SessionInfo,
    name: &str,
    content: &str,
    metadata: &Value,
    truncated: bool,
) -> Result<()> {
    append_session_event(
        session,
        json!({
            "type": "tool_result",
            "name": name,
            "content": content,
            "metadata": metadata,
            "truncated": truncated,
        }),
    )
}

pub fn append_session_checkpoint(session: &SessionInfo, checkpoint_id: &str) -> Result<()> {
    append_session_event(
        session,
        json!({
            "type": "checkpoint",
            "id": checkpoint_id,
        }),
    )
}

pub fn append_session_event(session: &SessionInfo, mut event: Value) -> Result<()> {
    let path = session_jsonl_path(&session.project_hash, &session.id)?;
    let mut file = OpenOptions::new()
        .append(true)
        .open(&path)
        .with_context(|| format!("failed to open session file {}", path.display()))?;

    if let Value::Object(ref mut object) = event {
        object.insert("created_at".to_string(), json!(Utc::now()));
    }

    writeln!(file, "{}", serde_json::to_string(&event)?)
        .with_context(|| format!("failed to append session file {}", path.display()))?;

    Ok(())
}

#[derive(Debug, Serialize)]
struct SessionJsonlEvent<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
    content: &'a str,
    created_at: DateTime<Utc>,
}

fn home_dir() -> Result<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set; cannot resolve LuckCode config/data directories")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_hash_is_short_sha256_prefix() {
        let hash = project_hash("/tmp/luckcode");
        assert_eq!(hash.len(), 16);
    }

    #[test]
    fn checkpoint_roundtrips_existing_and_new_files() {
        let tmp = tempfile::tempdir().expect("tmp dir");
        let root = tmp.path().canonicalize().expect("canonicalize");
        let existing = root.join("src").join("main.rs");
        fs::create_dir_all(existing.parent().unwrap()).unwrap();
        fs::write(&existing, "fn main() {}\n").unwrap();

        let project = ProjectInfo {
            root: root.clone(),
            hash: project_hash(&root),
        };
        let session = SessionInfo::new(&project);

        // existing file gets a `.before` snapshot; new file is recorded as not-yet-existing.
        let new_file = root.join("src").join("new.rs");
        let checkpoint_id = create_checkpoint(&session, &[existing.clone(), new_file.clone()])
            .expect("create checkpoint");

        let summaries = list_checkpoints(&project.hash, &session.id).expect("list");
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].id, checkpoint_id);
        assert_eq!(summaries[0].file_count, 2);

        // simulate the edit: change the existing file and create the new one.
        fs::write(&existing, "fn main() { changed }\n").unwrap();
        fs::write(&new_file, "pub fn new() {}\n").unwrap();

        let restored =
            restore_checkpoint(&root, &project.hash, &session.id, &checkpoint_id).expect("restore");
        assert_eq!(restored.len(), 2);

        // existing file rolled back, new file removed.
        assert_eq!(fs::read_to_string(&existing).unwrap(), "fn main() {}\n");
        assert!(!new_file.exists());
    }

    #[test]
    fn latest_checkpoint_is_newest() {
        let tmp = tempfile::tempdir().expect("tmp dir");
        let root = tmp.path().canonicalize().expect("canonicalize");
        let project = ProjectInfo {
            root: root.clone(),
            hash: project_hash(&root),
        };
        let session = SessionInfo::new(&project);
        let file = root.join("a.txt");
        fs::write(&file, "first").unwrap();

        let first = create_checkpoint(&session, std::slice::from_ref(&file)).unwrap();
        // force a distinct timestamp bucket by mutating then creating a second checkpoint.
        std::thread::sleep(std::time::Duration::from_secs(1));
        fs::write(&file, "second").unwrap();
        let second = create_checkpoint(&session, std::slice::from_ref(&file)).unwrap();

        let latest = latest_checkpoint(&project.hash, &session.id)
            .unwrap()
            .unwrap();
        assert_eq!(latest.id, second);
        assert_ne!(first, second);
    }
}
