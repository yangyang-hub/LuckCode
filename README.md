# LuckCode

LuckCode 是一个用 Rust 编写的本地 CLI Coding Agent。项目当前处于早期实现阶段：已经完成 workspace 骨架、CLI 入口、项目初始化、配置加载、只读本地工具、基础 session JSONL，以及第一版只读 Agent Loop。

目标不是做玩具 Demo，而是逐步实现一个类似 Claude Code / Codex 的本地编程 Agent：

```bash
luckcode "分析这个项目的启动流程"
luckcode "修复这个测试失败，并运行测试验证"
luckcode --plan "只分析，不修改代码"
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
- `luckcode ask --provider mock`：使用 MockProvider 进行本地流式输出验证。
- `luckcode ask --provider openai`：使用 OpenAI-compatible Chat Completions provider。
- `luckcode ask --provider responses`：使用 OpenAI Responses API 请求格式。
- `luckcode ask --provider anthropic`：使用 Anthropic Messages API 请求格式。
- 普通 prompt 会进入只读 Agent Loop，MockProvider 可以调用只读工具并基于工具结果输出摘要。
- session JSONL 会记录 user、assistant、tool_call 和 tool_result。
- `luckcode session list` 可以列出已有 session 文件。

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
cargo run -p luckcode-cli -- ask --provider mock "用一句话解释 LuckCode"
cargo run -p luckcode-cli -- ask --provider openai "用一句话解释 Rust ownership"
cargo run -p luckcode-cli -- ask --provider responses "用一句话解释 Rust ownership"
cargo run -p luckcode-cli -- ask --provider anthropic "用一句话解释 Rust ownership"
cargo run -p luckcode-cli -- --provider mock --model mock "分析这个项目"
cargo run -p luckcode-cli -- --plan "分析这个项目"
cargo run -p luckcode-cli -- session list
```

`ask --provider mock`、`ask --provider openai`、`ask --provider responses` 和 `ask --provider anthropic` 都会走 `ModelProvider` 流式输出；普通 prompt 会走第一版只读 Agent Loop。Agent Loop 会在 system context 中注入 `AGENTS.md`、项目类型、关键 manifest / README 预览、Git status 和 diff stat。

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
- 只读工具和编辑 / shell 工具分离。
- 不允许模型输出直接覆盖整个文件。
- 后续文件编辑和 shell 命令都必须经过权限系统。
- session 和 checkpoint 默认存放在 `~/.local/share/luckcode`。
- 第一版里除 `git status` / `git diff` 外，其它 shell 命令默认应先询问用户。

## 下一阶段

1. 增加更完整的 context builder。
2. 完善三种 HTTP provider 的错误展示、重试和超时控制。
3. 增加权限控制下的 `edit_file`、diff preview 和 checkpoint。
4. 增加 `run_shell` 和第一版权限系统。
5. 实现 resume / compact 的可用版本。
