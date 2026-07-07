use anyhow::{Context, Result};
use clap::Subcommand;
use luckcode_tools::{CommandPolicy, CommandPolicyConfig, PermissionEngine};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{process::Command as TokioCommand, time};

const DEFAULT_EVALS_DIR: &str = "evals";
const DEFAULT_EVAL_TIMEOUT_SECONDS: u64 = 600;

#[derive(Debug, Subcommand)]
pub enum EvalCommand {
    List {
        #[arg(long, default_value = DEFAULT_EVALS_DIR)]
        path: PathBuf,
    },
    Run {
        #[arg(value_name = "NAME")]
        name: Option<String>,
        #[arg(long)]
        filter: Option<String>,
        #[arg(long)]
        json: bool,
        #[arg(long, default_value = DEFAULT_EVALS_DIR)]
        path: PathBuf,
    },
}

pub async fn handle_eval(command: EvalCommand) -> Result<()> {
    match command {
        EvalCommand::List { path } => handle_eval_list(&path),
        EvalCommand::Run {
            name,
            filter,
            json,
            path,
        } => handle_eval_run(&path, name.as_deref(), filter.as_deref(), json).await,
    }
}

fn handle_eval_list(path: &Path) -> Result<()> {
    let evals = discover_evals(path)?;
    if evals.is_empty() {
        println!("No evals found under {}.", path.display());
        return Ok(());
    }

    println!("NAME\tTEST\tTIMEOUT\tPATH");
    for eval in evals {
        println!(
            "{}\t{}\t{}\t{}",
            eval.name(),
            eval.spec.test_command.as_deref().unwrap_or("-"),
            eval.spec
                .timeout_seconds
                .unwrap_or(DEFAULT_EVAL_TIMEOUT_SECONDS),
            eval.dir.display()
        );
    }
    Ok(())
}

async fn handle_eval_run(
    path: &Path,
    name: Option<&str>,
    filter: Option<&str>,
    json_output: bool,
) -> Result<()> {
    let mut evals = discover_evals(path)?;
    if let Some(name) = name {
        evals.retain(|eval| eval.name() == name);
    }
    if let Some(filter) = filter {
        evals.retain(|eval| eval.name().contains(filter));
    }
    if evals.is_empty() {
        anyhow::bail!("no evals matched the requested selector");
    }

    let mut reports = Vec::new();
    for eval in evals {
        reports.push(run_eval(&eval).await?);
    }

    if json_output {
        println!("{}", serde_json::to_string_pretty(&reports)?);
    } else {
        println!("NAME\tSTATUS\tEXIT\tDURATION_MS\tWORKSPACE");
        for report in reports {
            let exit = report
                .test_exit_code
                .map(|code| code.to_string())
                .unwrap_or_else(|| "-".to_string());
            println!(
                "{}\t{}\t{}\t{}\t{}",
                report.name,
                report.status,
                exit,
                report.duration_ms,
                report.workspace.display()
            );
        }
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct EvalDefinition {
    dir: PathBuf,
    spec: EvalSpec,
}

impl EvalDefinition {
    fn name(&self) -> &str {
        self.spec
            .name
            .as_deref()
            .or_else(|| self.dir.file_name().and_then(|name| name.to_str()))
            .unwrap_or("unknown")
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
struct EvalSpec {
    name: Option<String>,
    task: String,
    test_command: Option<String>,
    permission_mode: String,
    timeout_seconds: Option<u64>,
    scoring: EvalScoring,
}

impl Default for EvalSpec {
    fn default() -> Self {
        Self {
            name: None,
            task: String::new(),
            test_command: None,
            permission_mode: "accept-edits".to_string(),
            timeout_seconds: None,
            scoring: EvalScoring::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
struct EvalScoring {
    requires_successful_test: bool,
    forbidden_commands: Vec<String>,
}

impl Default for EvalScoring {
    fn default() -> Self {
        Self {
            requires_successful_test: true,
            forbidden_commands: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct EvalReport {
    name: String,
    status: String,
    duration_ms: u128,
    test_exit_code: Option<i32>,
    workspace: PathBuf,
    stdout: String,
    stderr: String,
}

fn discover_evals(path: &Path) -> Result<Vec<EvalDefinition>> {
    if !path.exists() {
        return Ok(Vec::new());
    }

    let mut evals = Vec::new();
    for entry in fs::read_dir(path).with_context(|| format!("failed to read {}", path.display()))? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let dir = entry.path();
        let spec_path = dir.join("eval.toml");
        if !spec_path.is_file() {
            continue;
        }
        let text = fs::read_to_string(&spec_path)
            .with_context(|| format!("failed to read {}", spec_path.display()))?;
        let spec = toml::from_str(&text)
            .with_context(|| format!("failed to parse {}", spec_path.display()))?;
        evals.push(EvalDefinition { dir, spec });
    }
    evals.sort_by(|a, b| a.name().cmp(b.name()));
    Ok(evals)
}

async fn run_eval(eval: &EvalDefinition) -> Result<EvalReport> {
    let start = Instant::now();
    let workspace = prepare_eval_workspace(eval)?;
    let Some(test_command) = eval.spec.test_command.as_deref() else {
        return Ok(EvalReport {
            name: eval.name().to_string(),
            status: "skipped".to_string(),
            duration_ms: start.elapsed().as_millis(),
            test_exit_code: None,
            workspace,
            stdout: String::new(),
            stderr: "eval has no test_command".to_string(),
        });
    };

    validate_eval_command(test_command, &eval.spec.scoring)?;
    let command_output = run_eval_command(
        test_command,
        &workspace,
        eval.spec
            .timeout_seconds
            .unwrap_or(DEFAULT_EVAL_TIMEOUT_SECONDS),
    )
    .await?;

    let passed = if eval.spec.scoring.requires_successful_test {
        command_output.success
    } else {
        true
    };

    Ok(EvalReport {
        name: eval.name().to_string(),
        status: if passed { "passed" } else { "failed" }.to_string(),
        duration_ms: start.elapsed().as_millis(),
        test_exit_code: command_output.exit_code,
        workspace,
        stdout: command_output.stdout,
        stderr: command_output.stderr,
    })
}

fn prepare_eval_workspace(eval: &EvalDefinition) -> Result<PathBuf> {
    let workspace = std::env::temp_dir().join("luckcode-evals").join(format!(
        "{}_{}",
        eval.name(),
        unique_suffix()
    ));
    fs::create_dir_all(&workspace)
        .with_context(|| format!("failed to create eval workspace {}", workspace.display()))?;

    let input = eval.dir.join("input");
    if input.is_dir() {
        copy_dir_contents(&input, &workspace)?;
    }
    Ok(workspace)
}

fn copy_dir_contents(source: &Path, target: &Path) -> Result<()> {
    fs::create_dir_all(target).with_context(|| format!("failed to create {}", target.display()))?;
    for entry in
        fs::read_dir(source).with_context(|| format!("failed to read {}", source.display()))?
    {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let source_path = entry.path();
        let target_path = target.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_contents(&source_path, &target_path)?;
        } else if file_type.is_file() {
            fs::copy(&source_path, &target_path).with_context(|| {
                format!(
                    "failed to copy {} -> {}",
                    source_path.display(),
                    target_path.display()
                )
            })?;
        }
    }
    Ok(())
}

fn validate_eval_command(command: &str, scoring: &EvalScoring) -> Result<()> {
    let decision = PermissionEngine::new(CommandPolicy::Allow)
        .evaluate_command(command, &CommandPolicyConfig::default());
    if decision.policy == CommandPolicy::Deny {
        anyhow::bail!("eval command denied: {}", decision.reason);
    }
    for forbidden in &scoring.forbidden_commands {
        if command.contains(forbidden) {
            anyhow::bail!("eval command contains forbidden pattern '{forbidden}'");
        }
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct EvalCommandOutput {
    success: bool,
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
}

async fn run_eval_command(
    command: &str,
    workspace: &Path,
    timeout_seconds: u64,
) -> Result<EvalCommandOutput> {
    let mut shell = shell_command(command);
    shell.current_dir(workspace);
    shell.kill_on_drop(true);

    let output = match time::timeout(
        Duration::from_secs(timeout_seconds.clamp(1, 3_600)),
        shell.output(),
    )
    .await
    {
        Ok(output) => output.with_context(|| format!("failed to run eval command '{command}'"))?,
        Err(_) => {
            return Ok(EvalCommandOutput {
                success: false,
                exit_code: None,
                stdout: String::new(),
                stderr: format!("eval command timed out after {timeout_seconds}s"),
            });
        }
    };

    Ok(EvalCommandOutput {
        success: output.status.success(),
        exit_code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
    })
}

#[cfg(windows)]
fn shell_command(command: &str) -> TokioCommand {
    let mut shell = TokioCommand::new("cmd");
    shell.arg("/C").arg(command);
    shell
}

#[cfg(not(windows))]
fn shell_command(command: &str) -> TokioCommand {
    let mut shell = TokioCommand::new("sh");
    shell.arg("-c").arg(command);
    shell
}

fn unique_suffix() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default();
    format!("{}_{}", std::process::id(), millis)
}

#[allow(dead_code)]
fn report_json(report: &EvalReport) -> serde_json::Value {
    json!({
        "name": report.name,
        "status": report.status,
        "duration_ms": report.duration_ms,
        "test_exit_code": report.test_exit_code,
        "workspace": report.workspace,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_evals_sorted_by_name() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        fs::create_dir_all(root.join("b_eval")).expect("mkdir");
        fs::write(
            root.join("b_eval/eval.toml"),
            "name = \"b\"\ntest_command = \"true\"\n",
        )
        .expect("write spec");
        fs::create_dir_all(root.join("a_eval")).expect("mkdir");
        fs::write(
            root.join("a_eval/eval.toml"),
            "name = \"a\"\ntest_command = \"true\"\n",
        )
        .expect("write spec");

        let evals = discover_evals(root).expect("discover");

        assert_eq!(
            evals.iter().map(EvalDefinition::name).collect::<Vec<_>>(),
            vec!["a", "b"]
        );
    }

    #[test]
    fn validate_eval_command_denies_dangerous_commands() {
        let error = validate_eval_command("sudo rm -rf .", &EvalScoring::default())
            .expect_err("dangerous command should be denied");

        assert!(format!("{error:#}").contains("denied"));
    }

    #[test]
    fn copy_dir_contents_copies_nested_files() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let source = tmp.path().join("source");
        let target = tmp.path().join("target");
        fs::create_dir_all(source.join("nested")).expect("mkdir");
        fs::write(source.join("nested/file.txt"), "hello").expect("write");

        copy_dir_contents(&source, &target).expect("copy");

        assert_eq!(
            fs::read_to_string(target.join("nested/file.txt")).expect("read"),
            "hello"
        );
    }
}
