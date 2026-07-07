# LuckCode 下一阶段实现设计与计划

本文档承接 `doc/luckcode-rust-cli-coding-agent-plan.md` 中 v0.8 之后的路线，给出可执行的设计方案、阶段顺序、验收标准和风险控制。当前 baseline 已覆盖 CLI、配置、模型 provider、Agent Loop、文件编辑、自动验证、session/memory、代码智能、MCP baseline 和 Docker sandbox baseline；下一阶段目标是把这些能力组织成更稳定、可观察、可回归验证的产品形态。

## 1. 总体原则

- 先补齐用户可见闭环，再做大规模抽象。TUI、sandbox 和 eval runner 都应先落地最小可用版本，再逐步扩展。
- 不新增空 crate。优先在现有 crate 中按模块拆分；只有接口稳定且被多个 crate 依赖时再拆新 crate。
- 所有可写、执行、外部工具调用仍必须复用现有权限策略，不绕过 `ToolContext`。
- 新增配置必须遵循完整结构体、Partial 结构体、环境覆盖、测试、文档同步的流程。
- 测试覆盖按风险扩展：TUI 做状态 reducer 单测，sandbox 做命令构造和配置单测，eval runner 做 fixture 生命周期和评分结果单测。

## 2. 预备阶段：模块拆分

当前 `luckcode-core/src/lib.rs` 和 `luckcode-tools/src/lib.rs` 已经超过规范建议的单文件规模。继续加 TUI、eval 和 sandbox 会明显增加维护成本，因此先做低风险模块拆分。

### 2.1 拆分目标

`luckcode-tools`：

- `src/registry.rs`：`Tool`、`ToolRegistry`、`ToolContext`、通用 output 类型。
- `src/fs_tools.rs`：`list_files`、`read_file`、`search_files`、路径安全 helper。
- `src/edit_tools.rs`：`edit_file`、`write_file`、diff、checkpoint callback。
- `src/shell.rs`：`run_shell`、command policy、Docker executor。
- `src/symbols.rs`：`list_symbols`、`find_symbol`、`find_references`、`module_summary`、tree-sitter parser。
- `src/git.rs`：`git_status`、`git_diff`。

`luckcode-core`：

- `src/config.rs`：`AppConfig`、Partial config、MCP config loading。
- `src/agent.rs`：`run_agent`、verification insertion、context assembly entry。
- `src/mcp.rs`：stdio / HTTP MCP transport、MCP tool registration。
- `src/context.rs`：project context、source overview、memory summary。
- `src/init.rs`：project init templates。

### 2.2 验收标准

- 公共 API 兼容，CLI 无需大规模改动。
- `cargo fmt --check`、`cargo clippy --all-targets --all-features`、`cargo test` 全通过。
- 拆分 commit 不混入行为变更。

## 3. v0.8：TUI baseline

### 3.1 目标

提供一个交互式 session browser，让用户可以浏览历史 session、查看 tool timeline、查看 checkpoint 和 diff，并从 TUI 跳转到已有命令行为。

### 3.2 依赖与位置

- workspace 新增 `ratatui`、`crossterm`。
- 第一版放在 `luckcode-cli/src/tui.rs`，不新增 `luckcode-tui` crate。
- 后续当 TUI 状态和渲染超过 CLI 职责时，再拆 `crates/luckcode-tui`。

### 3.3 CLI 入口

新增：

```bash
luckcode tui
luckcode session tui
```

建议 `luckcode tui` 作为主入口，`luckcode session tui` 可作为别名或后续补充。

### 3.4 数据模型

核心状态：

```rust
pub struct TuiState {
    pub sessions: Vec<SessionRow>,
    pub selected_session: usize,
    pub events: Vec<TimelineEvent>,
    pub selected_event: usize,
    pub panel: ActivePanel,
    pub filter: SessionFilter,
}

pub struct SessionRow {
    pub session_id: String,
    pub project_hash: String,
    pub updated_at: String,
    pub path: PathBuf,
}

pub struct TimelineEvent {
    pub index: usize,
    pub kind: String,
    pub title: String,
    pub detail: String,
}
```

状态更新用纯函数：

```rust
fn reduce(state: &mut TuiState, action: TuiAction);
```

这样可以在不启动终端的情况下单测导航、过滤和选择逻辑。

### 3.5 UI 布局

第一版三栏：

- 左侧：session list，按更新时间倒序。
- 中间：当前 session timeline，显示 user / assistant / tool_call / tool_result / checkpoint / compact_summary。
- 右侧：事件详情。tool result 显示截断内容，checkpoint 显示 id 和文件数。

底部状态栏：

- 当前 project hash。
- 过滤状态。
- 简短按键提示：`q` 退出、上下移动、Tab 切换面板、Enter 打开详情、`r` 恢复 checkpoint 二次确认。

### 3.6 交互范围

第一版只实现安全交互：

- 浏览 session。
- 浏览 timeline。
- 查看事件详情。
- 查看 checkpoint 列表。
- 对 checkpoint restore 做二次确认后调用已有 `restore_checkpoint`。

不在第一版里做：

- TUI 内发起 Agent prompt。
- TUI 内直接编辑文件。
- TUI 内运行 shell 命令。

### 3.7 验收

```bash
luckcode tui
```

验收点：

- 没有 session 时显示空态而不是 panic。
- 有 session 时能按更新时间浏览。
- `session show` 能看到的信息，在 TUI 里也能看到。
- checkpoint restore 必须二次确认。
- 单元测试覆盖 reducer 导航和事件格式化。

## 4. Sandbox 强化

### 4.1 目标

把当前 Docker executor baseline 从“能在容器里运行命令”升级成更可控的 sandbox 执行体系，重点解决镜像选择、缓存、网络策略和文件写入隔离。

### 4.2 配置设计

新增配置：

```toml
[sandbox]
executor = "local"        # local | docker
docker_image = "rust:1.93"
network = "none"          # none | host | bridge
workspace_mode = "bind"   # bind | copy
cache = true
```

环境变量：

- `LUCKCODE_SANDBOX_EXECUTOR`
- `LUCKCODE_SANDBOX_DOCKER_IMAGE`
- `LUCKCODE_SANDBOX_NETWORK`
- `LUCKCODE_SANDBOX_WORKSPACE_MODE`

CLI 覆盖：

```bash
luckcode --sandbox --sandbox-executor docker --sandbox-image rust:1.93 "运行测试"
```

### 4.3 执行模式

`bind`：

- 当前已实现 baseline。
- workspace bind mount 到 `/workspace`。
- 适合快速验证。
- 风险是命令可写 workspace，所以仍必须走确认和硬拒绝策略。

`copy`：

- 创建临时目录。
- 复制 workspace，尊重 `.gitignore`、`.luckcode/ignore`、敏感文件跳过。
- Docker mount 临时目录。
- 命令输出回传，但默认不写回项目。
- 适合 eval runner 和高风险命令。

### 4.4 镜像选择策略

按项目类型给默认镜像：

| 项目类型 | 默认镜像 |
| --- | --- |
| Rust | `rust:1.93` |
| Node / TypeScript | `node:24` |
| Java Maven / Gradle | `eclipse-temurin:21` |
| Go | `golang:1.25` |
| Python | `python:3.13` |

项目类型来自现有 `detect_project` / context builder，不重复实现。

### 4.5 缓存挂载

默认只挂工具链缓存，不挂敏感目录：

- Rust：Cargo registry、Cargo git、`target` 可选。
- Node：npm / pnpm store。
- Java：Maven `.m2`、Gradle cache。
- Go：module cache。

缓存目录放在 LuckCode data dir 下，例如：

```text
~/.local/share/luckcode/sandbox-cache/<project_hash>/
```

### 4.6 验收

- Docker args 构造有单元测试。
- `workspace_mode=copy` 不修改原项目。
- `network=none` 时生成 `--network none`。
- `--sandbox-executor docker` 没有 Docker 时返回清晰错误。
- `cargo test` 等验证命令仍返回标准 `ToolOutput`。

## 5. Eval Runner

### 5.1 目标

建立可回归验证体系，避免后续改 Agent Loop、工具权限、MCP 或 sandbox 时破坏核心行为。

### 5.2 CLI 入口

```bash
luckcode eval list
luckcode eval run
luckcode eval run rust_test_failure
luckcode eval run --filter shell-safety --json
```

### 5.3 目录结构

```text
evals/
├── rust_compile_error/
│   ├── eval.toml
│   ├── input/
│   └── expected.md
├── rust_test_failure/
├── ts_test_failure/
├── java_compile_error/
└── shell_safety/
```

`eval.toml`：

```toml
name = "rust_test_failure"
task = "修复测试失败，并运行测试验证"
test_command = "cargo test"
permission_mode = "accept-edits"
timeout_seconds = 600

[scoring]
requires_successful_test = true
forbidden_commands = ["sudo", "rm -rf"]
```

### 5.4 执行流程

1. 复制 fixture 到临时 workspace。
2. 初始化 session。
3. 使用 MockProvider 或真实 provider（默认 MockProvider for CI）。
4. 运行 Agent。
5. 执行 test command。
6. 收集结果：exit code、tool calls、diff、session events。
7. 输出 human summary 和 JSON report。

### 5.5 结果格式

```json
{
  "name": "rust_test_failure",
  "status": "passed",
  "duration_ms": 1234,
  "test_exit_code": 0,
  "tool_calls": ["read_file", "edit_file", "run_shell"],
  "report_path": "..."
}
```

### 5.6 验收

- `luckcode eval list` 能列出 evals。
- `luckcode eval run shell_safety` 不执行危险命令。
- eval runner 默认不依赖真实模型 key。
- JSON report 可用于 CI。

## 6. Release 工程

### 6.1 目标

让 LuckCode 可以稳定发布二进制，并且发布前验证路径固定。

### 6.2 命令

```bash
cargo build --release
cargo test
cargo clippy --all-targets --all-features
```

后续可增加：

```bash
luckcode doctor
```

`doctor` 检查：

- 配置文件是否可解析。
- provider 是否配置。
- Docker 是否可用。
- MCP server 是否可启动。
- 数据目录是否可写。

当前状态：`luckcode doctor` baseline 已实现，覆盖 workspace、配置目录、数据目录、provider、MCP 配置和 Docker 可用性；MCP server 启动探测可在后续增强。

### 6.3 Release checklist

- README 安装说明。
- CHANGELOG。
- GitHub Actions matrix：Linux / macOS / Windows。
- release artifact 命名：`luckcode-<version>-<target>`.
- `luckcode --version` 输出正确。

## 7. MCP 后续强化

当前 MCP HTTP 是 baseline，后续增强：

- 更完整的 Streamable HTTP session 生命周期。
- SSE 长连接读取和取消。
- per-server default policy。
- MCP result 安全过滤，避免外部 server 返回内容影响 system prompt。
- MCP tool allowlist 建议生成器。

## 8. 推荐实施顺序

1. 模块拆分：`tools` 和 `core` 先拆出清晰边界。
2. TUI baseline：session browser + timeline + detail panel。（已实现）
3. Docker sandbox copy mode：为 eval runner 提供隔离 workspace。
4. Eval runner baseline：fixtures + JSON report + CI-friendly output。（已实现）
5. Release 工程：doctor（已实现）、安装说明、artifact 规范。
6. MCP HTTP/SSE 强化和权限细化。

## 9. 阶段验收总表

| 阶段 | 验收命令 | 必须通过 |
| --- | --- | --- |
| 模块拆分 | `cargo test` | 行为不变 |
| TUI baseline | `luckcode tui` | 可浏览 session timeline（已实现） |
| Sandbox 强化 | `luckcode --sandbox --sandbox-executor docker ...` | Docker 命令隔离执行 |
| Eval runner | `luckcode eval run shell_safety --json` | 生成稳定 report（已实现） |
| Release | `cargo build --release` / `luckcode doctor` | doctor baseline 已实现，release artifact 待完善 |

每个阶段完成后都必须运行：

```bash
cargo fmt --check
cargo clippy --all-targets --all-features
cargo test
```
