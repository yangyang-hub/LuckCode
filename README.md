# LuckCode

LuckCode 是一个用 Rust 编写的本地 CLI Coding Agent。项目当前处于早期实现阶段：已经完成 workspace 骨架、CLI 入口、项目初始化、配置加载、只读本地工具、基础 session JSONL、第一版 Agent Loop、带 diff 预览/用户确认/checkpoint 的文件编辑系统（`edit_file` / `write_file` + `restore`）、编辑后的配置化自动测试、带命令权限策略/确认/超时/输出截断的 `run_shell`、第一版 resume / compact / project memory、tree-sitter-backed symbol index / function-level context、更完整的 system context、HTTP provider 超时/重试/错误展示、stdio / HTTP MCP tool/resource/prompt discovery、tool call、细粒度 tool policy 和 Agent registry 集成，以及 ratatui session browser 和 eval runner baseline。

目标不是做玩具 Demo，而是逐步实现一个类似 Claude Code / Codex 的本地编程 Agent：

```bash
luckcode "分析这个项目的启动流程"
luckcode "修复这个测试失败，并运行测试验证"
luckcode --plan "只分析，不修改代码"
luckcode --sandbox "运行测试但不要编辑文件"
luckcode --resume
luckcode --diff
luckcode --compact
```

## 当前能力

- `luckcode --version`：查看版本。
- `luckcode init`：初始化项目配置。
- `luckcode config show`：查看合并后的配置。
- `luckcode providers list`：列出已配置的模型 provider。
- `luckcode tools list`：列出内置工具。
- `luckcode tools call list_files`：列出工作区文件。
- `luckcode tools call read_file`：读取工作区文件。
- `luckcode tools call search_files`：搜索工作区文本。
- `luckcode tools call detect_project`：识别项目类型、manifest 和关键文件。
- `luckcode tools call git_status`：查看 Git 状态。
- `luckcode tools call git_diff`：查看 Git diff。
- `luckcode tools call list_symbols` / `luckcode symbols`：列出常见源码文件里的函数、类型和模块符号；Rust / TypeScript / TSX / Java 使用 tree-sitter 解析，其它支持语言使用轻量回退。
- `luckcode tools call find_symbol`：查找符号并返回函数/类型级源码上下文。
- `luckcode tools call find_references` / `luckcode references`：按标识符边界搜索符号引用，并返回源码上下文。
- `luckcode tools call module_summary` / `luckcode module-summary`：按文件汇总模块、类型、函数、方法和 impl 符号。
- `luckcode tools call edit_file`：用精确字符串替换修改已有文件（修改前展示 diff 并询问确认）。
- `luckcode tools call write_file`：创建新文件（不允许覆盖已有文件）。
- `luckcode tools call run_shell`：在工作区根目录执行 shell 命令；支持命令 allowlist / denylist / 默认策略配置，硬拒绝危险命令，并带超时和输出截断。
- `luckcode ask --provider mock`：使用 MockProvider 进行本地流式输出验证。
- `luckcode ask --provider openai`：使用 OpenAI-compatible Chat Completions provider。
- `luckcode ask --provider responses`：使用 OpenAI Responses API 请求格式。
- `luckcode ask --provider anthropic`：使用 Anthropic Messages API 请求格式。
- 普通 prompt 会进入 Agent Loop：`--plan` 模式只挂只读工具；其它模式下模型可调用 `edit_file` / `write_file` / `run_shell`。写文件前展示 diff、询问确认（`--accept-edits` 自动放行）并创建 checkpoint；shell 命令在 manual / accept-edits 下仍会询问，auto / dangerous 下会先展示命令再执行。
- 配置了 `[commands].test` 后，Agent 在 `edit_file` / `write_file` 真正改动文件后会自动调用 `run_shell` 执行测试命令；测试失败结果会回传给下一轮模型继续修复。
- Agent Loop 的 system context 会注入 `AGENTS.md`、项目类型、重要文件、工作区顶层概览、命令提示、源码概览、Git status/diff stat 和 project memory。
- HTTP provider 支持配置化 `timeout_seconds` / `retry_attempts`；非 2xx 错误会显示 HTTP 状态和截断后的响应体，超时、连接错误、429 和 5xx 会重试。
- `run_shell` 的权限策略优先级为：硬拒绝清单 → 配置 denylist → 配置 allowlist → 配置 default_policy 或当前权限模式。默认 allowlist 允许 `git status` / `git diff`，其它普通命令在 manual / sandbox 下仍会询问。
- `--sandbox` 默认是第一版权限策略 sandbox：禁用文件编辑，但允许 shell 命令在硬拒绝清单和用户确认后执行；加 `--sandbox-executor docker` 后，`run_shell` 会通过 Docker 容器执行并默认禁用网络。
- `luckcode restore [CHECKPOINT_ID]`：把当前项目最近一次 session 的最新 checkpoint（或指定 checkpoint）回滚。
- `luckcode --resume [SESSION_ID] ["继续任务"]`：查看或继续当前项目的 session。
- `luckcode --compact`：为当前项目最新 session 生成 compact summary 并写回 JSONL。
- `luckcode memory show` / `memory set` / `memory remove`：管理当前项目的持久记忆。
- `luckcode mcp list` / `mcp show`：检查 `.luckcode/mcp.json` 中配置的 MCP servers（env 值会被隐藏）。
- `luckcode mcp tools SERVER` / `mcp resources SERVER` / `mcp prompts SERVER` / `mcp call SERVER TOOL JSON`：通过 stdio 或 HTTP MCP transport 列出工具、资源、提示或调用工具。
- 非 `--plan` Agent 模式会把 MCP tools 注册为 `mcp_<server>_<tool>` 本地工具，并走与 shell 相同的确认 / allowlist / denylist 策略；`.luckcode/mcp.json` 可用 `tool_policies` 为单个 tool 配置 `allow` / `ask` / `deny`。
- session JSONL 会记录 user、assistant、tool_call、tool_result、checkpoint 和 compact_summary。
- `luckcode session list` / `session show` 可以浏览已有 session 和事件 timeline。
- `luckcode tui`：打开 ratatui session browser，浏览 session、timeline 和事件详情。
- `luckcode eval list` / `eval run`：发现并运行本地 eval fixture，支持 JSON report，并在执行测试命令前做危险命令拦截。
- `luckcode doctor`：检查 workspace、配置目录、数据目录、provider、MCP 配置和 Docker 可用性。

## 常用命令

```bash
cargo run -p luckcode-cli -- --version
cargo run -p luckcode-cli -- init
cargo run -p luckcode-cli -- config show
cargo run -p luckcode-cli -- providers list
cargo run -p luckcode-cli -- tools list
cargo run -p luckcode-cli -- tools call list_files '{"path":"."}'
cargo run -p luckcode-cli -- tools call read_file '{"path":"Cargo.toml"}'
cargo run -p luckcode-cli -- tools call search_files '{"query":"LuckCode"}'
cargo run -p luckcode-cli -- tools call detect_project '{"include_previews":true}'
cargo run -p luckcode-cli -- tools call git_status '{}'
cargo run -p luckcode-cli -- tools call git_diff '{}'
cargo run -p luckcode-cli -- tools call list_symbols '{"path":"crates","limit":100}'
cargo run -p luckcode-cli -- tools call find_symbol '{"name":"run_agent","path":"crates/luckcode-core/src/lib.rs"}'
cargo run -p luckcode-cli -- tools call find_references '{"name":"run_agent","path":"crates","limit":20}'
cargo run -p luckcode-cli -- tools call module_summary '{"path":"crates/luckcode-core/src/lib.rs"}'
cargo run -p luckcode-cli -- symbols crates --limit 100
cargo run -p luckcode-cli -- references run_agent crates --limit 20
cargo run -p luckcode-cli -- module-summary crates/luckcode-core/src/lib.rs
cargo run -p luckcode-cli -- tools call edit_file '{"path":"README.md","old_string":"LuckCode","new_string":"LuckCode"}'
cargo run -p luckcode-cli -- tools call write_file '{"path":"notes.txt","content":"hello\n"}'
cargo run -p luckcode-cli -- tools call run_shell '{"command":"cargo test","timeout_seconds":120}'
cargo run -p luckcode-cli -- ask --provider mock "用一句话解释 LuckCode"
cargo run -p luckcode-cli -- ask --provider openai "用一句话解释 Rust ownership"
cargo run -p luckcode-cli -- ask --provider responses "用一句话解释 Rust ownership"
cargo run -p luckcode-cli -- ask --provider anthropic "用一句话解释 Rust ownership"
cargo run -p luckcode-cli -- --provider mock --model mock "分析这个项目"
cargo run -p luckcode-cli -- --plan "分析这个项目"
cargo run -p luckcode-cli -- --accept-edits "修复这个 clippy warning"
cargo run -p luckcode-cli -- --sandbox "运行 cargo test 并总结失败"
cargo run -p luckcode-cli -- --sandbox --sandbox-executor docker --sandbox-image rust:1.93 "运行 cargo test 并总结失败"
cargo run -p luckcode-cli -- restore
cargo run -p luckcode-cli -- session list
cargo run -p luckcode-cli -- session show
cargo run -p luckcode-cli -- tui
cargo run -p luckcode-cli -- --compact
cargo run -p luckcode-cli -- --resume
cargo run -p luckcode-cli -- --resume ses_xxx "继续完成下一步"
cargo run -p luckcode-cli -- memory show
cargo run -p luckcode-cli -- memory set project.test_command "cargo test"
cargo run -p luckcode-cli -- mcp list
cargo run -p luckcode-cli -- mcp show local
cargo run -p luckcode-cli -- mcp tools local
cargo run -p luckcode-cli -- mcp resources local
cargo run -p luckcode-cli -- mcp prompts local
cargo run -p luckcode-cli -- mcp call local lookup '{"key":"value"}'
cargo run -p luckcode-cli -- eval list
cargo run -p luckcode-cli -- eval run shell_safety --json
cargo run -p luckcode-cli -- doctor
```

`ask --provider mock`、`ask --provider openai`、`ask --provider responses` 和 `ask --provider anthropic` 都会走 `ModelProvider` 流式输出；普通 prompt 会走 Agent Loop。`--plan` 只挂只读工具；`--sandbox` 禁止编辑但允许经确认的 shell；其它模式下工具集包含 `edit_file` / `write_file` / `run_shell`，写文件前会展示 diff、按权限模式询问确认并创建 checkpoint，shell 命令会经过 `CommandPolicy` / `PermissionEngine` 检查。Agent Loop 会在 system context 中注入 `AGENTS.md`、项目类型、关键 manifest / README 预览、Git status、diff stat 和 project memory；`--resume` 会额外注入上一轮 compact summary。

## 模型配置

默认使用本地 `mock` provider，不需要 API key。

LuckCode 支持像 opencode 一样在配置里同时定义多个 provider profile，然后运行时选择：

```toml
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
timeout_seconds = 120
retry_attempts = 2
enabled = true

[providers.responses]
kind = "openai"
model = "gpt-4.1"
request_format = "responses"
api_key_env = "OPENAI_API_KEY"
base_url = "https://api.openai.com/v1"
timeout_seconds = 120
retry_attempts = 2
enabled = true

[providers.anthropic]
kind = "anthropic"
model = "claude-sonnet-4-5"
api_key_env = "ANTHROPIC_API_KEY"
base_url = "https://api.anthropic.com"
timeout_seconds = 120
retry_attempts = 2
enabled = true
```

命令策略可在配置中调整：

```toml
[commands]
# Agent 会在文件被实际修改后自动运行该测试命令。
test = "cargo test"
check = "cargo check"
lint = "cargo clippy"

[commands.policy]
default_policy = "ask"
allowlist = ["git status", "git diff"]
denylist = []
```

选择 provider：

```bash
cargo run -p luckcode-cli -- providers list
cargo run -p luckcode-cli -- --provider mock "分析这个项目"
cargo run -p luckcode-cli -- --provider openai --model gpt-4.1 "分析这个项目"
cargo run -p luckcode-cli -- ask --provider anthropic --model claude-sonnet-4-5 "解释这个函数"
```

也支持 `provider/model` 形式：

```toml
[model]
model = "anthropic/claude-sonnet-4-5"
```

命令行和环境变量优先级高于配置文件：

```bash
export LUCKCODE_PROVIDER=responses
export LUCKCODE_MODEL=gpt-4.1
export LUCKCODE_MODEL_TIMEOUT_SECONDS=120
export LUCKCODE_MODEL_RETRY_ATTEMPTS=2
```

使用 OpenAI-compatible Chat Completions provider：

```bash
export OPENAI_API_KEY=你的 API key
export LUCKCODE_MODEL_PROVIDER=openai
export LUCKCODE_MODEL=gpt-4.1

cargo run -p luckcode-cli -- ask --provider openai "用一句话解释 Rust ownership"
```

使用 OpenAI Responses 请求格式：

```bash
export OPENAI_API_KEY=你的 API key
export LUCKCODE_MODEL_PROVIDER=responses
export LUCKCODE_MODEL=gpt-4.1

cargo run -p luckcode-cli -- ask --provider responses "用一句话解释 Rust ownership"
```

也可以使用统一 provider 名称并显式指定请求格式：

```bash
export LUCKCODE_MODEL_PROVIDER=openai
export LUCKCODE_MODEL_REQUEST_FORMAT=responses
```

可选环境变量：

```bash
export LUCKCODE_OPENAI_API_KEY=你的 API key
export LUCKCODE_OPENAI_BASE_URL=https://api.openai.com/v1
```

`openai-compatible` 也可用于兼容 Chat Completions 接口的本地或第三方服务。

使用 Anthropic Messages 请求格式：

```bash
export ANTHROPIC_API_KEY=你的 API key
export LUCKCODE_MODEL_PROVIDER=anthropic
export LUCKCODE_MODEL=claude-sonnet-4-5

cargo run -p luckcode-cli -- ask --provider anthropic "用一句话解释 Rust ownership"
```

可选环境变量：

```bash
export LUCKCODE_ANTHROPIC_API_KEY=你的 API key
export LUCKCODE_ANTHROPIC_BASE_URL=https://api.anthropic.com
export LUCKCODE_ANTHROPIC_VERSION=2023-06-01
```

当前三种请求格式：

| Provider | Endpoint | 请求格式 |
| --- | --- | --- |
| `openai` / `openai-chat` / `openai-compatible` | `/chat/completions` | OpenAI Chat Completions |
| `responses` / `openai-responses` | `/responses` | OpenAI Responses |
| `anthropic` / `claude` | `/v1/messages` | Anthropic Messages |

## 项目结构

```text
crates/
├── luckcode-cli/       # CLI 入口
├── luckcode-core/      # 配置加载和项目初始化
├── luckcode-model/     # 模型 Provider 抽象
├── luckcode-storage/   # 项目路径、session 路径和标识
└── luckcode-tools/     # 内置本地工具
```

## 配置文件

`luckcode init` 会生成：

```text
AGENTS.md
.luckcode/config.toml
.luckcode/mcp.json
.luckcode/ignore
```

`.luckcode/mcp.json` 支持 stdio 和 HTTP MCP server：

```json
{
  "mcpServers": {
    "local": {
      "command": "node",
      "args": ["server.js"],
      "env": { "API_KEY": "..." },
      "tool_policies": { "lookup": "allow", "delete": "deny" }
    },
    "remote": {
      "url": "https://example.com/mcp",
      "headers": { "Authorization": "Bearer ..." },
      "tool_policies": { "*": "ask" }
    }
  }
}
```

配置优先级：

1. 默认配置。
2. `~/.config/luckcode/config.toml`。
3. 项目 `.luckcode/config.toml`。
4. 环境变量。
5. 命令行参数。

## 安全原则

- 使用 `AGENTS.md` 保存项目规则。
- 只读工具和编辑 / shell 工具严格分离：`--plan` 模式只挂只读工具。
- 不允许模型输出直接覆盖整个文件：`edit_file` 用精确字符串替换，`write_file` 只能新建文件。
- 文件编辑流程为 read → 校验 → 展示 diff → 用户确认（或 `--accept-edits` 自动放行）→ 创建 checkpoint → 写入。
- 编辑写入前会在 `~/.local/share/luckcode/checkpoints/<project_hash>/<session_id>/` 下创建 checkpoint，`luckcode restore` 可回滚。
- 不读取 `.env`、私钥和凭据；敏感文件不参与编辑。
- shell 命令必须经过权限系统；除 allowlist 命中的命令外，shell 命令默认先询问用户，并硬拒绝 `sudo`、`rm -rf`、`chmod -R 777`、`curl|sh`、`wget|bash`、`dd`、`mkfs`、`docker system prune`、`terraform apply/destroy`、`kubectl delete` 以及引用敏感路径的命令。

## 下一阶段

详细设计见 `doc/luckcode-next-phase-implementation-design.md`。

1. 完善 Docker sandbox 的 copy workspace、镜像选择、缓存挂载和跨平台路径处理。
2. 扩展 eval runner：更多 fixtures、Agent 驱动模式、CI report 归档。
3. 完善 release 打包、安装文档和 MCP HTTP/SSE 强化。
