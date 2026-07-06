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
}
