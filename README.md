# LuckCode

LuckCode 是一个用 Rust 编写的本地 CLI Coding Agent。项目当前处于早期实现阶段：已经完成 workspace 骨架、CLI 入口、项目初始化、配置加载、只读本地工具、基础 session JSONL、第一版 Agent Loop、带 diff 预览/用户确认/checkpoint 的文件编辑系统（`edit_file` / `write_file` + `restore`）、带命令权限策略/确认/超时/输出截断的 `run_shell`、第一版 resume / compact / project memory，以及基础 symbol index。

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
- `luckcode tools call list_symbols` / `luckcode symbols`：列出常见源码文件里的函数、类型和模块符号。
- `luckcode tools call edit_file`：用精确字符串替换修改已有文件（修改前展示 diff 并询问确认）。
- `luckcode tools call write_file`：创建新文件（不允许覆盖已有文件）。
- `luckcode tools call run_shell`：在工作区根目录执行 shell 命令；默认先询问，硬拒绝危险命令，并带超时和输出截断。
- `luckcode ask --provider mock`：使用 MockProvider 进行本地流式输出验证。
- `luckcode ask --provider openai`：使用 OpenAI-compatible Chat Completions provider。
- `luckcode ask --provider responses`：使用 OpenAI Responses API 请求格式。
- `luckcode ask --provider anthropic`：使用 Anthropic Messages API 请求格式。
- 普通 prompt 会进入 Agent Loop：`--plan` 模式只挂只读工具；其它模式下模型可调用 `edit_file` / `write_file` / `run_shell`。写文件前展示 diff、询问确认（`--accept-edits` 自动放行）并创建 checkpoint；shell 命令在 manual / accept-edits 下仍会询问，auto / dangerous 下会先展示命令再执行。
- `--sandbox` 是第一版权限策略 sandbox：禁用文件编辑，但允许 shell 命令在硬拒绝清单和用户确认后执行。它还不是 Docker / container 隔离。
- `luckcode restore [CHECKPOINT_ID]`：把当前项目最近一次 session 的最新 checkpoint（或指定 checkpoint）回滚。
- `luckcode --resume [SESSION_ID] ["继续任务"]`：查看或继续当前项目的 session。
- `luckcode --compact`：为当前项目最新 session 生成 compact summary 并写回 JSONL。
- `luckcode memory show` / `memory set` / `memory remove`：管理当前项目的持久记忆。
- `luckcode mcp list` / `mcp show`：检查 `.luckcode/mcp.json` 中配置的 MCP servers（env 值会被隐藏）。
- session JSONL 会记录 user、assistant、tool_call、tool_result、checkpoint 和 compact_summary。
- `luckcode session list` / `session show` 可以浏览已有 session 和事件 timeline。

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
cargo run -p luckcode-cli -- symbols crates --limit 100
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
cargo run -p luckcode-cli -- restore
cargo run -p luckcode-cli -- session list
cargo run -p luckcode-cli -- session show
cargo run -p luckcode-cli -- --compact
cargo run -p luckcode-cli -- --resume
cargo run -p luckcode-cli -- --resume ses_xxx "继续完成下一步"
cargo run -p luckcode-cli -- memory show
cargo run -p luckcode-cli -- memory set project.test_command "cargo test"
cargo run -p luckcode-cli -- mcp list
cargo run -p luckcode-cli -- mcp show local
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
enabled = true

[providers.responses]
kind = "openai"
model = "gpt-4.1"
request_format = "responses"
api_key_env = "OPENAI_API_KEY"
base_url = "https://api.openai.com/v1"
enabled = true

[providers.anthropic]
kind = "anthropic"
model = "claude-sonnet-4-5"
api_key_env = "ANTHROPIC_API_KEY"
base_url = "https://api.anthropic.com"
enabled = true
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
- shell 命令必须经过权限系统；第一版里除 `git status` / `git diff` 外的 shell 命令默认先询问用户，并硬拒绝 `sudo`、`rm -rf`、`chmod -R 777`、`curl|sh`、`wget|bash`、`dd`、`mkfs`、`docker system prune`、`terraform apply/destroy`、`kubectl delete` 以及引用敏感路径的命令。

## 下一阶段

1. 增加更完整的 context builder。
2. 完善三种 HTTP provider 的错误展示、重试和超时控制。
3. 扩展 `run_shell` 权限系统（命令 allowlist、可配置默认策略、sandbox）并完善测试失败后的自动重试。
4. 接入 ratatui TUI，提供交互式 diff、tool call timeline 和 session browser。
