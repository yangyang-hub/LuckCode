# LuckCode Rust CLI Coding Agent 学习与实现计划

LuckCode 的目标不是做一个玩具 Demo，而是逐步实现一个类似 Claude Code / Codex 的本地编程 Agent。它以 Rust 编写，通过稳定的 Agent Loop 连接模型、本地工具、权限系统、会话存储和项目上下文，最终支持分析项目、修改代码、运行验证、恢复会话和扩展外部工具。

典型使用方式：

```bash
luckcode "分析这个项目的启动流程"
luckcode "修复这个测试失败，并运行测试验证"
luckcode --plan "只分析，不修改代码"
luckcode --resume
luckcode --diff
luckcode --compact
```

## 1. 项目命名与定位

| 项目项 | 命名 |
| --- | --- |
| 产品名 | LuckCode |
| 正式命令 | `luckcode` |
| 仓库名 | `luckcode` |
| 项目规则文件 | `AGENTS.md` |
| 配置目录 | `~/.config/luckcode` |
| 数据目录 | `~/.local/share/luckcode` |

可以额外提供短别名：

```bash
lc "修复这个测试失败"
```

但 `lc` 可能和用户本地 alias 冲突，所以正式命令仍以 `luckcode` 为准。

LuckCode 的定位：

> 一个用 Rust 编写的本地 CLI Coding Agent。

核心能力：

| 能力 | 说明 |
| --- | --- |
| 读代码 | 读取文件、目录、配置、README |
| 搜代码 | 调用 ripgrep 或内部搜索 |
| 理解项目 | 识别 Rust、Java、Node、Terraform 等项目 |
| 修改代码 | 基于 patch / diff 修改文件 |
| 运行命令 | 执行测试、构建、lint |
| 验证结果 | 修改后自动运行测试确认 |
| 恢复会话 | 支持 resume、compact、session list |
| 权限控制 | 写文件、跑命令前做安全判断 |
| MCP 扩展 | 后期接入外部工具和数据源 |
| Sandbox | 后期支持 Docker / 隔离执行 |

这类工具的核心不是“多 Agent”，而是一个稳定的 Agent Loop：模型提出工具调用，本地执行工具，把结果回传给模型，模型继续决策。模型负责推理和选择工具，应用侧负责执行、权限、安全、上下文和持久化。

## 2. 技术选型

### 2.1 Rust 主栈

| 模块 | 推荐 |
| --- | --- |
| CLI 参数解析 | `clap` |
| 异步运行时 | `tokio` |
| HTTP 请求 | `reqwest` |
| JSON 序列化 | `serde` / `serde_json` |
| 错误处理 | `anyhow` / `thiserror` |
| 日志 | `tracing` / `tracing-subscriber` |
| 配置文件 | `toml` |
| 本地数据库 | `rusqlite` |
| 文件遍历 | `walkdir` / `ignore` |
| diff 展示 | `similar` |
| shell 执行 | `tokio::process::Command` |
| AST 解析 | `tree-sitter` |
| MCP | `rmcp` 或手写 JSON-RPC client |
| TUI | 后期使用 `ratatui` |

选型理由：

- `clap` 适合做 CLI，支持 derive 方式把参数、子命令、枚举参数映射到 Rust struct / enum。
- `tokio` 适合 LuckCode 这种工具，因为模型流式输出、MCP 通信、shell 执行和超时控制都需要异步能力。
- MCP 的 tools 规范允许 server 暴露可被模型调用的工具，适合作为 LuckCode 后期插件系统的基础。

## 3. 仓库结构设计

建议使用 Cargo workspace：

```text
luckcode/
├── Cargo.toml
├── crates/
│   ├── luckcode-cli/          # CLI 入口
│   ├── luckcode-core/         # Agent Loop 核心
│   ├── luckcode-model/        # 模型 Provider
│   ├── luckcode-tools/        # 内置工具
│   ├── luckcode-context/      # 上下文构建
│   ├── luckcode-storage/      # session / memory / sqlite
│   ├── luckcode-permission/   # 权限系统
│   ├── luckcode-sandbox/      # sandbox 执行
│   ├── luckcode-mcp/          # MCP client/server
│   └── luckcode-eval/         # 评测系统
├── docs/
├── examples/
├── evals/
├── AGENTS.md
└── README.md
```

根目录 `Cargo.toml`：

```toml
[workspace]
resolver = "2"
members = [
  "crates/luckcode-cli",
  "crates/luckcode-core",
  "crates/luckcode-model",
  "crates/luckcode-tools",
  "crates/luckcode-context",
  "crates/luckcode-storage",
  "crates/luckcode-permission",
  "crates/luckcode-sandbox",
  "crates/luckcode-mcp",
  "crates/luckcode-eval"
]
```

前期不要一次性创建过多空 crate。第一阶段只创建：

- `luckcode-cli`
- `luckcode-core`
- `luckcode-model`
- `luckcode-tools`
- `luckcode-storage`

等代码复杂度上来后，再拆分：

- `luckcode-context`
- `luckcode-permission`
- `luckcode-sandbox`
- `luckcode-mcp`
- `luckcode-eval`

## 4. 核心架构

LuckCode 的核心结构：

```text
User Input
   ↓
CLI
   ↓
Agent Core
   ├── Context Builder
   ├── Model Provider
   ├── Tool Registry
   ├── Permission Engine
   ├── Session Manager
   └── Output Renderer
   ↓
Tools
   ├── read_file
   ├── search_files
   ├── edit_file
   ├── run_shell
   ├── git_diff
   └── git_status
```

一次完整任务流程：

1. 用户输入任务。
2. LuckCode 扫描项目上下文。
3. 构造模型请求。
4. 模型决定调用工具。
5. 权限系统检查工具调用。
6. 本地执行工具。
7. 工具结果写入 session。
8. 结果回传给模型。
9. 模型继续下一步。
10. 完成任务并输出总结。

## 5. 核心抽象设计

### 5.1 ModelProvider

不要把 LuckCode 绑定死在某一个模型。

```rust
#[async_trait::async_trait]
pub trait ModelProvider: Send + Sync {
    async fn stream(
        &self,
        request: ModelRequest,
    ) -> anyhow::Result<ModelStream>;
}

pub struct ModelRequest {
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSchema>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
}

pub enum ModelEvent {
    TextDelta(String),
    ToolCallDelta(ToolCallDelta),
    ToolCallDone(ToolCall),
    Done,
}
```

第一版支持：

- OpenAI-compatible provider
- Anthropic provider
- Mock provider

`MockProvider` 很重要，用来写测试。否则每次跑 Agent Loop 测试都要请求真实模型，成本高，而且结果不稳定。

### 5.2 Tool

```rust
#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;

    fn description(&self) -> &'static str;

    fn schema(&self) -> serde_json::Value;

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: ToolContext,
    ) -> anyhow::Result<ToolOutput>;
}

pub struct ToolOutput {
    pub content: String,
    pub metadata: serde_json::Value,
    pub truncated: bool,
}
```

第一批工具：

- `list_files`
- `read_file`
- `search_files`
- `git_status`
- `git_diff`
- `edit_file`
- `run_shell`
- `ask_user`

### 5.3 ToolRegistry

```rust
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn register<T: Tool + 'static>(&mut self, tool: T) {
        self.tools.insert(tool.name().to_string(), Arc::new(tool));
    }

    pub async fn execute(
        &self,
        call: ToolCall,
        ctx: ToolContext,
    ) -> anyhow::Result<ToolOutput> {
        let tool = self
            .tools
            .get(&call.name)
            .ok_or_else(|| anyhow::anyhow!("unknown tool: {}", call.name))?;

        tool.execute(call.arguments, ctx).await
    }
}
```

### 5.4 Agent Loop

核心循环可以先设计成：

```rust
pub async fn run_agent(
    task: UserTask,
    ctx: AgentContext,
) -> anyhow::Result<AgentResult> {
    let mut step = 0;

    loop {
        if step >= ctx.max_steps {
            return Ok(AgentResult::Stopped {
                reason: "max steps exceeded".to_string(),
            });
        }

        let request = ctx.context_builder.build(&task).await?;
        let mut stream = ctx.model.stream(request).await?;

        while let Some(event) = stream.next().await {
            match event? {
                ModelEvent::TextDelta(delta) => {
                    ctx.ui.render_delta(&delta).await?;
                }

                ModelEvent::ToolCallDone(call) => {
                    let decision = ctx.permission.check(&call).await?;

                    match decision {
                        PermissionDecision::Allow => {}
                        PermissionDecision::AskUser { reason } => {
                            let approved = ctx.ui.ask_approval(&call, &reason).await?;
                            if !approved {
                                return Ok(AgentResult::Cancelled);
                            }
                        }
                        PermissionDecision::Deny { reason } => {
                            ctx.session.append_denied_tool_call(&call, &reason).await?;
                            continue;
                        }
                    }

                    let output = ctx.tools.execute(call, ctx.tool_context()).await?;
                    ctx.session.append_tool_result(&output).await?;
                }

                ModelEvent::Done => {
                    return Ok(AgentResult::Done);
                }

                _ => {}
            }
        }

        step += 1;
    }
}
```

## 6. LuckCode 命令设计

### 6.1 主命令

```bash
luckcode "分析这个项目的启动流程"
```

默认行为：

- 读取当前目录。
- 加载 `AGENTS.md`。
- 加载 `.luckcode/config.toml`。
- 创建 session。
- 进入 Agent Loop。

### 6.2 只读模式

```bash
luckcode --plan "分析这个 bug 怎么修，不要改代码"
```

允许：

- `list_files`
- `read_file`
- `search_files`
- `git_status`
- `git_diff`

禁止：

- `edit_file`
- `write_file`
- `run_shell`
- `delete_file`

### 6.3 自动接受编辑

```bash
luckcode --accept-edits "修复 clippy warning"
```

行为：

- 文件编辑自动允许。
- shell 命令仍然需要确认。
- 危险路径仍然拒绝。

### 6.4 恢复会话

```bash
luckcode --resume
luckcode --resume <session-id>
```

### 6.5 查看 diff

```bash
luckcode --diff
```

显示当前 session 造成的文件变更。

### 6.6 压缩上下文

```bash
luckcode --compact
```

生成摘要：

- 任务目标
- 已查看文件
- 已修改文件
- 已运行命令
- 当前状态
- 下一步建议
- 风险点

### 6.7 初始化项目

```bash
luckcode init
```

生成：

- `AGENTS.md`
- `.luckcode/config.toml`
- `.luckcode/mcp.json`
- `.luckcode/ignore`

## 7. 配置设计

全局配置：

```toml
# ~/.config/luckcode/config.toml

[model]
provider = "openai"
model = "gpt-5.5"

[permission]
mode = "manual"

[workspace]
max_file_size = 200000
ignore = [
  ".git",
  "node_modules",
  "target",
  "dist",
  ".env"
]

[ui]
stream = true
show_tool_calls = true
```

项目配置：

```toml
# .luckcode/config.toml

[project]
name = "my-project"
language = "rust"

[commands]
test = "cargo test"
check = "cargo check"
lint = "cargo clippy"

[permission]
mode = "manual"
```

项目规则文件：

```markdown
# AGENTS.md

- 修改代码后必须运行 cargo test
- 不要自动 git commit
- 不要读取 .env
- 不要执行 sudo
- 不要直接运行 terraform apply / terraform destroy
- 所有 shell 命令执行前必须展示给用户确认
```

配置优先级：

1. 默认配置。
2. `~/.config/luckcode/config.toml`。
3. 项目 `.luckcode/config.toml`。
4. 环境变量。
5. 命令行参数。

## 8. 权限系统设计

权限模式：

| 模式 | 说明 |
| --- | --- |
| `plan` | 只读，不能修改文件，不能执行命令 |
| `manual` | 默认模式，写文件和 shell 都要确认 |
| `accept-edits` | 自动接受文件编辑，shell 仍确认 |
| `auto` | 低风险命令自动执行，高风险确认 |
| `sandbox` | 在隔离环境中自动执行 |
| `dangerous` | 全自动，不建议默认提供 |

权限结果：

```rust
pub enum PermissionDecision {
    Allow,
    AskUser {
        reason: String,
    },
    Deny {
        reason: String,
    },
}
```

默认拒绝：

- `sudo`
- `rm -rf`
- `chmod -R 777`
- `curl xxx | sh`
- `wget xxx | bash`
- `dd`
- `mkfs`
- `docker system prune`
- `terraform destroy`
- `kubectl delete`

默认可自动允许：

- `git status`
- `git diff`
- `cargo check`
- `cargo test`
- `npm test`
- `pnpm test`
- `go test ./...`
- `mvn test`

第一版建议只有 `git status` 和 `git diff` 自动允许，其它 shell 命令先询问用户。

## 9. 文件编辑系统

LuckCode 不应该让模型直接覆盖整个文件。推荐流程：

1. `read_file`
2. 模型生成 patch。
3. 本地校验 patch。
4. 展示 diff。
5. 用户确认。
6. 创建 checkpoint。
7. apply patch。
8. `git diff`。
9. 必要时运行测试。

`edit_file` 输入：

```json
{
  "path": "src/main.rs",
  "patch": "--- a/src/main.rs\n+++ b/src/main.rs\n..."
}
```

Checkpoint 路径：

```text
~/.local/share/luckcode/checkpoints/
└── <project-hash>/
    └── <session-id>/
        └── 2026-07-06T12-00-00/
            ├── manifest.json
            └── files/
                └── src_main.rs.before
```

恢复命令：

```bash
luckcode restore
luckcode restore <checkpoint-id>
```

## 10. 上下文管理

不要把整个项目一次性塞给模型。

上下文优先级：

1. 用户当前任务。
2. `AGENTS.md` 项目规则。
3. 最近工具调用结果。
4. 当前 git diff。
5. 相关源码片段。
6. `README` / `Cargo.toml` / `package.json` / `pom.xml`。
7. 历史 session summary。
8. 项目 memory。

项目识别：

| 文件 | 项目类型 |
| --- | --- |
| `Cargo.toml` | Rust |
| `package.json` | Node / TypeScript |
| `pom.xml` | Java Maven |
| `build.gradle` | Java Gradle |
| `go.mod` | Go |
| `pyproject.toml` | Python |
| `*.tf` | Terraform |
| `docker-compose*` | Docker |

默认忽略：

- `.git`
- `node_modules`
- `target`
- `dist`
- `build`
- `.idea`
- `.vscode`
- `.env`
- `*.pem`
- `*.key`

## 11. Session 和存储

数据目录：

```text
~/.local/share/luckcode/
├── sessions/
│   └── <project-hash>/
│       └── <session-id>.jsonl
├── checkpoints/
├── memory/
├── index.sqlite
└── logs/
```

JSONL 示例：

```jsonl
{"type":"user","content":"修复测试失败","created_at":"2026-07-06T12:00:00+08:00"}
{"type":"assistant","content":"我先运行 cargo test 查看错误"}
{"type":"tool_call","name":"run_shell","args":{"cmd":"cargo test"}}
{"type":"tool_result","name":"run_shell","exit_code":1,"output":"..."}
{"type":"tool_call","name":"edit_file","args":{"path":"src/lib.rs"}}
{"type":"checkpoint","id":"ck_123"}
```

SQLite 表：

```sql
CREATE TABLE sessions (
  id TEXT PRIMARY KEY,
  project_hash TEXT NOT NULL,
  project_path TEXT NOT NULL,
  title TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

CREATE TABLE messages (
  id TEXT PRIMARY KEY,
  session_id TEXT NOT NULL,
  role TEXT NOT NULL,
  content TEXT NOT NULL,
  created_at TEXT NOT NULL
);

CREATE TABLE checkpoints (
  id TEXT PRIMARY KEY,
  session_id TEXT NOT NULL,
  path TEXT NOT NULL,
  created_at TEXT NOT NULL
);

CREATE TABLE project_memory (
  project_hash TEXT NOT NULL,
  key TEXT NOT NULL,
  value TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  PRIMARY KEY (project_hash, key)
);
```

## 12. 12 周学习和实现计划

### 第 1 周：Rust 基础 + LuckCode 骨架

目标：创建项目结构。

学习重点：

- ownership
- borrowing
- trait
- `Result` / `Option`
- module
- crate
- workspace
- `serde`
- `anyhow`
- `thiserror`

实现：

- 创建 Cargo workspace。
- 创建 `luckcode-cli`。
- 创建 `luckcode-core`。
- 创建 `luckcode-model`。
- 创建 `luckcode-tools`。
- 创建 `luckcode-storage`。
- 实现 `luckcode --version`。
- 实现 `luckcode init`。

验收：

```bash
luckcode --version
luckcode init
```

### 第 2 周：CLI + 配置系统

目标：LuckCode 作为 CLI 能稳定运行。

实现：

- `clap` 参数解析。
- `run` / `init` / `config` / `session` 子命令。
- 读取 `~/.config/luckcode/config.toml`。
- 读取 `.luckcode/config.toml`。
- 环境变量覆盖配置。
- `tracing` 日志。
- 统一错误输出。

命令：

```bash
luckcode config show
luckcode config set model.provider openai
luckcode run "hello"
```

验收：

```bash
luckcode config show
luckcode --debug run "hello"
```

### 第 3 周：Model Provider 层

目标：能请求模型并流式输出。

实现：

- `ModelProvider` trait。
- OpenAI-compatible provider。
- Anthropic provider。
- Mock provider。
- streaming parser。
- 超时控制。
- 错误重试。

环境变量：

```bash
export OPENAI_API_KEY=xxx
export ANTHROPIC_API_KEY=xxx
export LUCKCODE_MODEL_PROVIDER=openai
export LUCKCODE_MODEL=gpt-5.5
```

验收：

```bash
luckcode ask "用一句话解释 Rust ownership"
luckcode ask --provider mock "hello"
```

### 第 4 周：只读工具系统

目标：LuckCode 可以理解项目，但不能修改。

实现工具：

- `list_files`
- `read_file`
- `search_files`
- `git_status`
- `git_diff`
- `detect_project`

工具调试命令：

```bash
luckcode tools list
luckcode tools call list_files '{"path":"."}'
luckcode tools call read_file '{"path":"Cargo.toml"}'
```

验收：

```bash
luckcode "这个项目是什么技术栈？"
luckcode "入口文件在哪里？"
luckcode "搜索所有 login 相关代码"
```

### 第 5 周：Agent Loop 第一版

目标：模型可以自主调用工具。

实现：

- `ToolRegistry`。
- Tool schema 生成。
- Tool call 解析。
- Tool result 回填。
- Agent step loop。
- `max_steps`。
- tool output truncation。
- basic context builder。

验收：

```bash
luckcode "帮我找出这个项目的配置文件，并总结它们的作用"
```

预期执行链：

```text
list_files
read_file README.md
read_file Cargo.toml
search_files config
final answer
```

### 第 6 周：文件编辑系统

> 状态：已实现（v0.2）。`edit_file`（精确字符串替换 + `replace_all`）、`write_file`（仅新建）、checkpoint（`create_checkpoint` / `restore_checkpoint` / `list_checkpoints`）、diff preview、按权限模式确认、`luckcode restore [CHECKPOINT_ID]`。`edit_file` 采用精确字符串替换而非 unified-diff 输入，对模型更稳健；diff 用 `similar` 渲染。

目标：可以修改文件，但修改前必须确认。

实现：

- `edit_file`
- `apply_patch`
- `write_file`
- `create_checkpoint`
- `restore_checkpoint`
- diff preview
- 用户确认

验收：

```bash
luckcode "把这个函数重构一下，保持行为不变"
luckcode --accept-edits "修复 clippy warning"
luckcode restore
```

要求：

- 修改前展示 diff。
- 确认后才写入。
- 写入前创建 checkpoint。
- 失败时可 restore。

### 第 7 周：Shell 执行器 + 权限系统

目标：可以运行测试和构建。

实现：

- `run_shell`
- `CommandPolicy`
- `PathPolicy`
- `PermissionEngine`
- 超时控制
- stdout / stderr 截断
- 危险命令拦截
- 用户确认

验收：

```bash
luckcode "运行测试，分析失败原因"
luckcode "修复这个测试失败，并运行 cargo test 验证"
```

要求：

- 能运行测试。
- 能读取错误。
- 能修改代码。
- 能再次运行测试。
- 能输出验证结果。

### 第 8 周：Session Resume

目标：长任务可恢复。

实现：

- JSONL session。
- session list。
- session resume。
- session title 生成。
- tool call 记录。
- tool result 记录。
- checkpoint 关联。

命令：

```bash
luckcode session list
luckcode --resume
luckcode --resume <session-id>
```

验收：

- 退出终端后重新进入，可以恢复上次任务。
- 能看到上次读过哪些文件。
- 能看到上次修改了哪些文件。
- 能继续执行下一步。

### 第 9 周：上下文压缩与项目记忆

目标：处理长任务。

实现：

- compact summary。
- project memory。
- recent tool result priority。
- git diff priority。
- `AGENTS.md` priority。
- token budget。

命令：

```bash
luckcode --compact
luckcode memory show
luckcode memory set project.test_command "cargo test"
```

摘要格式：

```text
任务目标：
已完成：
已查看文件：
已修改文件：
已运行命令：
当前状态：
下一步建议：
风险点：
```

验收：

```bash
luckcode --resume
luckcode --compact
luckcode "继续刚才的修复"
```

### 第 10 周：Tree-sitter 代码智能

目标：从文本搜索升级到结构化代码理解。

第一批语言：

- Rust
- TypeScript
- Java

实现：

- `parse_file`
- `extract_symbols`
- `find_symbol`
- `find_references` 简化版
- function-level context
- module summary

Symbol 结构：

```rust
pub struct Symbol {
    pub name: String,
    pub kind: SymbolKind,
    pub file: PathBuf,
    pub start_line: usize,
    pub end_line: usize,
}
```

命令：

```bash
luckcode symbols
luckcode symbols src/main.rs
luckcode "解释 AuthService.login 的调用链"
```

### 第 11 周：MCP Client

目标：LuckCode 能接入外部 MCP 工具。

MCP 规范除了 tools，也支持 prompts。prompts 可以让 server 提供结构化提示模板给 client 使用。

实现：

- 读取 `.luckcode/mcp.json`。
- 启动 MCP server process。
- `initialize`。
- `tools/list`。
- `tools/call`。
- `resources/list`。
- `prompts/list`。
- 把 MCP tool 转成本地 Tool。
- 权限系统接管 MCP tool。

配置：

```json
{
  "mcpServers": {
    "filesystem": {
      "command": "npx",
      "args": ["@modelcontextprotocol/server-filesystem", "."]
    }
  }
}
```

命令：

```bash
luckcode mcp list
luckcode mcp tools
luckcode mcp call <server> <tool>
```

安全原则：

- MCP tool 不默认信任。
- MCP tool 调用也走权限系统。
- MCP 返回内容不能覆盖 system prompt。
- MCP server 来源必须展示给用户。

### 第 12 周：Sandbox + Eval + Release

目标：从“能用”变成“可发布”。

实现：

- `DockerExecutor`
- `LocalExecutor`
- readonly mode
- workspace mount
- network policy
- eval runner
- release build

命令：

```bash
luckcode --sandbox docker "运行测试并修复"
luckcode eval run
cargo build --release
```

Eval 目录：

```text
evals/
├── rust_compile_error/
├── rust_test_failure/
├── ts_test_failure/
├── java_spring_error/
├── terraform_module_error/
└── shell_safety/
```

每个 eval 包含：

- 初始项目。
- 用户任务。
- 期望行为。
- 测试命令。
- 评分脚本。

## 13. 阶段性版本路线

### v0.1：只读代码助手

能力：

- CLI
- 配置
- 模型调用
- list / read / search
- 只读 Agent Loop

命令：

```bash
luckcode "分析这个项目"
luckcode --plan "解释这个模块"
```

### v0.2：可编辑代码助手

> 状态：已实现。

能力：

- `edit_file`
- diff preview
- checkpoint
- restore
- git diff

命令：

```bash
luckcode "修复这个简单 bug"
luckcode restore
```

### v0.3：可验证 Coding Agent

> 状态：第一版已实现 `run_shell`、命令权限策略、危险命令拦截、超时和输出截断；测试失败后的自动重试策略仍需继续细化。

能力：

- `run_shell`（已实现）
- 第一版权限系统（`CommandPolicy` / `PermissionEngine`、已实现）
- 测试验证（可通过 `run_shell` 执行配置的测试命令）
- 失败重试（Agent Loop 可基于 tool result 继续迭代，后续补更明确策略）

命令：

```bash
luckcode "修复测试失败，并运行测试验证"
```

### v0.4：会话恢复

> 状态：第一版已实现。`--resume` 可查看或继续当前项目 session，`--compact` 会生成 deterministic compact summary 并写回 JSONL，`memory show/set/remove` 提供项目记忆。

能力：

- session JSONL（已实现）
- resume（已实现）
- compact（已实现）
- project memory（已实现）

命令：

```bash
luckcode --resume
luckcode --compact
```

### v0.5：代码智能

> 状态：基础版已实现。当前提供 parser-free symbol index（`luckcode symbols` / `list_symbols`），覆盖常见 Rust/TS/JS/Python/Go/Java 函数和类型定义；tree-sitter 精准解析仍是后续增强。

能力：

- tree-sitter（后续增强）
- symbol index（基础版已实现）
- function-level context（后续增强）

命令：

```bash
luckcode symbols
luckcode "解释这个函数的调用链"
```

### v0.6：MCP

> 状态：配置检查基础版已实现。当前支持 `luckcode mcp list/show` 读取 `.luckcode/mcp.json` 并隐藏 env 值；stdio tool discovery / tool call 仍需继续实现。

能力：

- MCP client（配置检查基础版已实现）
- MCP tool registry（后续）
- MCP permission（后续）

### v0.7：Sandbox

> 状态：权限策略 baseline 已实现。`--sandbox` 禁用文件编辑，shell 命令仍需确认并经过硬拒绝策略；Docker / container 隔离仍需后续实现。

能力：

- Docker executor（后续）
- readonly mode（权限策略 baseline 已实现）
- network policy（后续）

### v0.8：TUI

> 状态：命令行 session browser baseline 已实现。`luckcode session list/show` 可查看 session 和事件 timeline；ratatui 交互式界面仍需后续实现。

能力：

- `ratatui`（后续）
- interactive diff（后续）
- tool call timeline（命令行 baseline 已实现）
- session browser（命令行 baseline 已实现）

### v1.0：稳定版

能力：

- 插件系统
- eval benchmark
- 跨平台 release
- 文档完善
- 安全策略稳定

## 14. 第一版依赖建议

初始 `Cargo.toml` 可以先用这些：

```toml
[dependencies]
tokio = { version = "1", features = ["full"] }
clap = { version = "4", features = ["derive"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
reqwest = { version = "0.12", features = ["json", "stream"] }
anyhow = "1"
thiserror = "2"
async-trait = "0.1"
tracing = "0.1"
tracing-subscriber = "0.3"
toml = "0.8"
uuid = { version = "1", features = ["v4", "serde"] }
chrono = { version = "0.4", features = ["serde"] }
rusqlite = { version = "0.32", features = ["bundled"] }
walkdir = "2"
ignore = "0.4"
globset = "0.4"
similar = "2"
tempfile = "3"
```

后期再加：

```toml
tree-sitter = "0.24"
ratatui = "0.29"
crossterm = "0.28"
```

具体版本创建项目前用 `cargo add` 或 crates.io 再确认一次。

## 15. 第一周具体执行清单

### Day 1：建仓库

```bash
mkdir luckcode
cd luckcode
git init

cargo new crates/luckcode-cli --bin
cargo new crates/luckcode-core --lib
cargo new crates/luckcode-model --lib
cargo new crates/luckcode-tools --lib
cargo new crates/luckcode-storage --lib
```

写根目录 `Cargo.toml` workspace。

### Day 2：CLI 基础

实现：

- `luckcode --version`
- `luckcode init`
- `luckcode config show`

### Day 3：配置系统

实现配置优先级：

1. 默认配置。
2. `~/.config/luckcode/config.toml`。
3. 项目 `.luckcode/config.toml`。
4. 环境变量。
5. 命令行参数。

### Day 4：日志和错误处理

实现：

- `tracing` 初始化。
- `--verbose`。
- `--debug`。
- 统一 `anyhow` error 输出。

### Day 5：Session ID 和项目 hash

实现：

- 当前项目路径 canonicalize。
- `project_hash`。
- `session_id`。
- 创建 session JSONL。

### Day 6：`read_file` / `list_files`

实现两个本地工具，但暂时不接模型：

```bash
luckcode tools call list_files '{"path":"."}'
luckcode tools call read_file '{"path":"Cargo.toml"}'
```

### Day 7：整理 README

README 先写清楚：

- LuckCode 是什么。
- 当前支持什么。
- 计划支持什么。
- 安全原则。
- 开发路线。

## 16. 最终建议

开发顺序：

1. CLI 和配置。
2. 模型 Provider。
3. 只读工具。
4. Agent Loop。
5. 文件编辑。
6. Shell 执行。
7. 权限系统。
8. Session Resume。
9. 上下文压缩。
10. Tree-sitter。
11. MCP。
12. Sandbox。
13. TUI。
14. 发布。

第一阶段不要做：

- 多 Agent。
- IDE 插件。
- Web UI。
- 向量数据库。
- 自动 PR。
- 自动部署。
- 完整 LSP。
- 云端 workspace。

LuckCode 的第一个成功标准：

```bash
luckcode "分析这个项目"
luckcode "找到这个报错可能来自哪里"
luckcode "修复一个简单测试失败，并运行测试确认"
```

这三件事稳定完成后，LuckCode 就已经不是 Demo，而是一个真正可用的 Rust CLI Coding Agent。
