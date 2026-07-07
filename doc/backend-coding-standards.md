# LuckCode 后端代码实现规范

本规范是 LuckCode（Rust 编写的本地 CLI Coding Agent）后端代码的唯一实现标准。**后续所有 Rust 代码实现、重构、Code Review 都必须遵循本规范。** 当本规范与既有代码冲突时，以本规范为准并对旧代码做对齐；当本规范未覆盖某种情况时，遵循「最小惊讶」原则并与同一 crate 内既有写法保持一致。

- 规划背景见 `doc/luckcode-rust-cli-coding-agent-plan.md`。
- 项目硬性规则见 `AGENTS.md`（修改代码后跑测试、不自动提交、不读 `.env`、不 `sudo`、执行 shell 前展示等）。本规范在代码层面落实这些规则。
- 工具链：Rust 1.93+、edition 2024、Cargo workspace（`resolver = "2"`）。

---

## 1. 仓库与 crate 架构

### 1.1 workspace 组织

- 所有 crate 放在 `crates/<name>/`，crate 名统一前缀 `luckcode-`，目录名与 crate 名去掉前缀后的部分一致（`crates/luckcode-core` → crate `luckcode-core`）。
- crate 间只允许单向依赖，禁止循环依赖。当前的依赖方向：

  ```text
  luckcode-cli
    ├── luckcode-core      （Agent Loop、配置、初始化）
    │     ├── luckcode-model   （ModelProvider 抽象）
    │     ├── luckcode-tools   （内置工具）
    │     └── luckcode-storage （路径、session、标识）
    │             （model / tools / storage 之间尽量不互相依赖）
    ├── luckcode-model
    ├── luckcode-storage
    └── luckcode-tools
  ```

- `luckcode-cli` 是唯一的 `[[bin]]`（`name = "luckcode"`），不放业务逻辑，只做参数解析、装配和输出（见 §9）。
- `luckcode-core` 是业务编排层（Agent Loop、配置加载、项目初始化），可依赖其它 crate，但不应被 model/tools/storage 依赖。
- `luckcode-model` / `luckcode-tools` / `luckcode-storage` 是相对独立的「叶子能力」层，互相之间尽量解耦。

### 1.2 何时新增 crate

遵循计划文档的渐进拆分原则：**不要一次性创建空的 crate**。只有当下述条件同时满足时才新增 crate：

1. 该能力有清晰、稳定的外部接口（对外暴露的 trait / 结构体）。
2. 它会被至少一个其它 crate 依赖，或体量足够大（独立成 crate 能显著降低编译时间或职责边界）。
3. 与既有 crate 职责确实不同（例如后续的 `luckcode-permission`、`luckcode-sandbox`、`luckcode-mcp`、`luckcode-context`）。

新增 crate 必须先在本规范 §1.1 的依赖图里登记方向。

### 1.3 何时新增模块 / 拆分文件

当前每个 crate 用单个 `src/lib.rs`（cli 用 `src/main.rs`）。文件拆分规则：

- 单文件超过 **约 500 行** 或出现 **3 个以上独立职责** 时，按职责拆分为 `src/<topic>.rs` 并在 `lib.rs` 用 `mod <topic>;` 声明。
- 拆分时优先按「领域」而非「种类」：例如 `tools/git.rs`、`tools/fs.rs`，而不是 `tools/structs.rs` + `tools/impls.rs`。
- `lib.rs` / `main.rs` 负责 `mod` 声明、`pub use` 重导出和顶层装配；具体实现下沉到子模块。
- 跨 crate 复用的类型放到最合适的下游 crate；不要在两个 crate 里重复定义同名结构体。

---

## 2. 依赖管理

- **所有第三方依赖必须在根 `Cargo.toml` 的 `[workspace.dependencies]` 里声明版本**，子 crate 的 `Cargo.toml` 只写 `dep.workspace = true`，不得在子 crate 内钉版本号。
- 新增依赖前先确认：是否能用已有依赖或 std 解决。能不引就不引。
- 版本号写主次版本（如 `clap = "4"`、`reqwest = "0.12"`），与既有风格保持一致；引入新 major 前在 PR 里说明理由。
- 启用 feature 时只开必要的（参考既有写法，如 `tokio = { features = ["full"] }`、`reqwest = { features = ["json", "stream"] }`）。不要无脑开 `"full"`。
- `dev-dependencies` 同样走 workspace 声明；测试专用 crate（如 `tempfile`、`tokio` 的 test 宏）只放在需要它的 crate。
- 禁止引入需要联网编译、闭源或带不明确 license 的依赖。

当前已确立的核心依赖（见根 `Cargo.toml`）：`clap`、`tokio`、`reqwest`、`serde`/`serde_json`、`anyhow`、`async-trait`、`async-stream`、`futures-core`/`futures-util`、`tracing`/`tracing-subscriber`、`toml`、`ignore`、`sha2`、`similar`、`tree-sitter`、`uuid`、`chrono`。`tempfile` 作为 workspace `dev-dependency`（测试专用）。后续按需补充 `thiserror`、`rusqlite`、`walkdir`、`globset`、`ratatui` 等。

---

## 3. 命名规范

| 对象 | 规则 | 示例 |
| --- | --- | --- |
| crate / 目录 / 模块 | `snake_case`，crate 用 `luckcode-` 前缀 | `luckcode-tools`、`mod git_status` |
| 结构体 / 枚举 / trait | `UpperCamelCase` | `ToolContext`、`ModelProvider`、`PermissionMode` |
| 枚举变体 | `UpperCamelCase` | `MessageRole::Tool`、`PermissionMode::AcceptEdits` |
| 函数 / 方法 / 变量 | `snake_case` | `resolve_provider_config`、`readonly_registry` |
| 常量 / 静态 | `SCREAMING_SNAKE_CASE` | `AGENTS_TEMPLATE`、`PROJECT_CONFIG_TEMPLATE` |
| 文件名 | `snake_case.rs` | `session.rs`、`tool_registry.rs` |
| 工具名（对模型暴露） | `snake_case` 字符串字面量，动词或动宾 | `"list_files"`、`"read_file"`、`"git_status"` |
| 配置 key（TOML / serde） | `snake_case`；枚举用 `kebab-case` | `[permission] mode = "accept-edits"` |
| 环境变量 | `LUCKCODE_` 前缀 + `SCREAMING_SNAKE_CASE` | `LUCKCODE_PROVIDER`、`LUCKCODE_OPENAI_API_KEY` |

补充：

- 类型名用名词，构造器/工厂用 `new` / `from_env` / `from_env_with_options` / `with_xxx`（builder 风格，返回 `Self`）。
- 布尔字段/变量用 `is_` / `has_` / `should_` 前缀（如 `is_sensitive_path`、`truncated` 这类约定俗成的可例外）。
- 同一概念在跨 crate 边界出现时（如 `ToolCall` 在 model 和 tools 各有一个），允许用 `as` 别名消除歧义：`use luckcode_tools::{ToolCall as LocalToolCall};`。

---

## 4. 错误处理

### 4.1 默认用 `anyhow`

- 函数返回类型统一写 `anyhow::Result<T>`，文件头 `use anyhow::{Context, Result};`，不要自己造 `Result` 别名。
- **任何可能失败的 IO / 解析 / 外部调用之后，必须用 `.with_context(...)` 或 `.context(...)` 附加上下文**，上下文要带「在做什么」+ 关键参数（通常是路径）：

  ```rust
  let text = fs::read_to_string(path)
      .with_context(|| format!("failed to read config file {}", path.display()))?;
  ```

- 提前失败用 `anyhow::bail!(...)`；构造错误值用 `anyhow::anyhow!(...)`。两者都支持格式化参数。
- 在 `Option` 上失败用 `.context("reason")?`（anyhow 为 `Option` 提供了 `Context`）或 `.with_context(...)?`，不要 `unwrap()`。

### 4.2 何时用 `thiserror`

- 默认**不**用 `thiserror`。库内传播错误一律 `anyhow`。
- 仅当**对外公共 API 的调用方需要 `match` 具体错误类型**时（例如未来的 `PermissionDecision`、`PermissionError`），才为该边界定义 `thiserror::Error` 枚举，并在该枚举里给出清晰的变体与 `#[error("...")]` 文案。
- `thiserror` 枚举只定义在需要的 crate，不要全局铺开。

### 4.3 入口与退出码

- `main` 用 `#[tokio::main] async fn main()`，内部 `run() -> Result<()>`，失败时：

  ```rust
  if let Err(error) = run().await {
      eprintln!("error: {error:#}");   // {:#} 让 anyhow 展开完整 cause 链
      std::process::exit(1);
  }
  ```

- 错误信息走 **stderr**（`eprintln!` / `tracing::error!`），正常输出走 stdout。
- 不使用 `process::exit` 之外的方式中止，不要在库代码里调用 `exit`。

---

## 5. 异步与并发

### 5.1 运行时与 trait

- 异步入口 `#[tokio::main]`；workspace 已统一 `tokio = { features = ["full"] }`。
- 包含 `async fn` 的 trait 用 `#[async_trait::async_trait]` 标注，且 trait 需要加 `: Send + Sync` 约束（参考 `ModelProvider`、`Tool`）。
- 跨 await 边界持有的值必须 `Send`；trait object 用 `dyn Trait + Send + Sync`，存在 `Arc<dyn Trait>` 里（参考 `ToolRegistry::tools: HashMap<String, Arc<dyn Tool>>`）。

### 5.2 流式处理

- 模型/网络流统一返回类型别名 `ModelStream = Pin<Box<dyn Stream<Item = Result<ModelEvent>> + Send>>`。
- 用 `async_stream::try_stream! { ... }` 构造流，`yield` 产出事件；底层 chunk 用 `futures_util::StreamExt::next` 消费。
- 消费流时按 `ModelEvent` 变体 `match`，遇到 `Done` 即结束，错误用 `event?` 传播。

### 5.3 阻塞操作

- **不要在 async 上下文里直接做长阻塞 CPU/IO**。当前小文件读写用 `std::fs` 可接受；一旦涉及大文件遍历、AST 解析、subprocess 大量输出，改用 `tokio::task::spawn_blocking` 或 `tokio::process::Command` + `bytes_stream`。
- 子进程执行：只读轻量的 git 命令当前用 `std::process::Command`（参考 `tools` 里的 `run_git_command`）；后续 `run_shell`、构建/测试执行必须用 `tokio::process::Command` 并做超时控制与 stdout/stderr 截断。

### 5.4 并发安全

- 共享不可变状态用 `Arc<T>`；共享可变状态用 `Arc<Mutex<T>>` / `Arc<RwLock<T>>`，优先 `parking_lot`（引入前走 §2 评审）。
- 任何 `static` / `Lazy` 必须是 `Sync`；不要用 `Rc`/`RefCell` 跨 await。

---

## 6. 类型与序列化建模

### 6.1 derive 顺序

结构体/枚举的 derive 按以下顺序，缺哪项跳过，不要随意调换：

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
```

- 默认带上 `Debug`（所有公开和内部类型都要能打印，便于排错与测试）。
- 值类型（配置、消息、事件）加 `Clone + PartialEq`；含 `f32` 等不可 `Eq` 字段时只到 `PartialEq`。
- 需要序列化进 TOML/JSON/JSONL 的加 `Serialize, Deserialize`。
- 仅内部使用、不对外序列化的临时结构（如 stream chunk 解析用的 `OpenAiChatStreamChunk`）只需 `Debug`（+ `Deserialize` 如果是从 JSON 反序列化）。

### 6.2 serde 约定

- 枚举在序列化边界统一加 `#[serde(rename_all = "snake_case")]`（消息角色）或 `"kebab-case"`（权限模式/配置枚举），与该枚举既有写法保持一致，不要混用。
- 配置类结构体加 `#[serde(default)]` 并提供 `impl Default`，保证缺字段时用默认值（参考 `AppConfig` / `WorkspaceConfig`）。
- 用 `#[serde(skip_serializing_if = "Option::is_none")]` 避免输出冗余的 `None`。
- 字段重命名用 `#[serde(rename = "type")]`；JSON 工具参数解析用的结构体字段保持私有（小写），不必 `pub`。

### 6.3 配置的「Partial 合并」模式

LuckCode 的配置是多来源叠加（默认 → 全局 → 项目 → 环境变量 → CLI）。新增可配置项时必须：

1. 在「完整结构体」（如 `AppConfig`）里加字段并提供 `Default`。
2. 在对应的 `Partial*` 结构体里加 `Option<T>` 字段（`#[serde(default)]`）。
3. 在 `Partial*::apply_to` 里只覆盖 `Some` 的字段，`None` 不动默认值。
4. 如果该项支持环境变量覆盖，在 `apply_env_overrides` 里补充，环境变量名遵循 §3 的 `LUCKCODE_*` 规则。
5. 为该项加一个单元测试（参考 `lib.rs` 末尾 `tests` 模块）。

不要为了「方便」直接把 `AppConfig` 反序列化而绕过 Partial 合并——这会破坏「项目配置只覆盖部分字段」的语义。

### 6.4 构造与选项

- 构造器优先 `new(...)`；可选项多时用 `with_xxx(mut self, ...) -> Self` 链或专门的 `Options` 结构体（参考 `InitOptions`、`AgentOptions` + `impl Default`），而不是一长串位置参数。
- 公开结构体的字段如果要被外部构造，`pub`；否则保持私有并提供访问器/构造器（参考 `MockProvider` 的 `mode` 字段私有、提供 `new` / `agent`）。

---

## 7. 工具系统约定（`luckcode-tools`）

新增工具必须严格遵循既有 `Tool` 抽象：

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;        // 蛇形动词，与 §3 一致；动态 MCP 工具可返回实例字段
    fn description(&self) -> &str;  // 一句话英文说明，给模型看
    fn schema(&self) -> serde_json::Value;  // JSON Schema，见下
    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput>;
}
```

### 7.1 实现要点

- 内置工具实现成单元结构体 `pub struct XxxTool;`，`impl Tool for XxxTool`；动态外部工具（如 MCP）可用带字段的结构体保存名称、schema 和连接配置。
- `schema()` 用 `serde_json::json!({ "type": "object", "properties": {...} })`；必填字段写进 `"required"`；数值字段标 `"minimum"` 和合理 `"default"`（参考 `read_file`、`list_files`）。
- 入参反序列化成私有 `XxxInput` 结构体（`#[derive(Debug, Deserialize)]`，字段全 `Option` 除非必填），用 `serde_json::from_value(input).context("invalid <tool> input")?`。
- 输出统一返回 `ToolOutput { content, metadata, truncated }`：
  - `content`：给人/模型读的纯文本主体。
  - `metadata`：`serde_json::Value`，放结构化指标（`count`、`bytes`、`total_lines`、`exit_code` 等）。
  - `truncated`：是否因 `limit` 截断，**默认 `false`**，有分页语义时才置 `true`。
- 所有需要遍历文件的工具走 `ignore::WalkBuilder`（已统一），开启 `git_ignore` / `git_exclude`，按 `limit` 截断。

### 7.2 注册与只读/可写分离

- 只读工具注册到 `readonly_registry()`，写文件 / shell 执行工具注册到 `mutating_registry()`；`full_registry()` 是两者合集。**只读工具和可写 / 执行工具必须分集合注册**，`--plan` 模式只挂 `readonly_registry()`，其它允许修改或执行的模式挂 `full_registry()`。
- `ToolRegistry::list()` 已按名字排序，新增工具无需手动排。

### 7.3 路径安全（强制）

- 凡是接受用户/模型传入路径的工具，**必须**通过 `resolve_existing_path`（已存在文件）或 `resolve_new_path`（`write_file` 新建文件，禁止 `..`、校验 `starts_with(root)`）解析后做 `starts_with(root)` 校验，禁止越出 workspace（参考既有实现）。
- 敏感文件检测统一用 `is_sensitive_path`（`.env` / `*.pem` / `*.key` / `id_rsa` / `id_ed25519`），命中则跳过预览，不得读取或回传内容，编辑类工具命中直接 `bail!`（落实 `AGENTS.md`：不读 `.env`、私钥、凭据）。
- 文件大小用 `ctx.max_file_size` 做上限，超出 `bail!` 或跳过，不要无限制读入内存。

### 7.5 文件编辑工具约定（`edit_file` / `write_file`）

- `edit_file` 用**精确字符串替换**（`old_string` → `new_string`，可选 `replace_all`），而不是 unified-diff 输入：对模型更稳健，且天然满足「不允许直接覆盖整个文件」。`old_string` 必须唯一（除非 `replace_all`），为空或匹配 0 次直接 `bail!`。
- `write_file` **只允许新建**：目标已存在则 `bail!`（引导改用 `edit_file`）。
- 修改前必须经 `ToolContext.edit_approval`：`Refuse`（plan）直接 `bail!`；`Prompt`（manual）调用 `confirm_edit` 回调展示 diff 并询问；`Auto`（accept-edits/auto/dangerous）直接放行。`Prompt` 模式下若无 `confirm_edit` 回调，`bail!` 提示用户改用 agent 或 `--accept-edits`。
- 写入前调用 `create_checkpoint` 回调（由 CLI 注入，内部走 `luckcode_storage::create_checkpoint` + `append_session_checkpoint`）；`tools call` 调试路径无该回调时允许 `checkpoint_id = None`。
- diff 渲染统一用 `similar::TextDiff`（`render_diff`），事件回调 `EditPreview` 只携带 path/diff/计数，**不携带文件原文敏感内容**。

### 7.4 工具错误 = 内容而非崩溃

- 工具执行失败时，Agent Loop 会把错误写进 `tool_result` 并继续（见 `execute_tool_call` 的 `Err` 分支）。因此工具内部对「可恢复的失败」（如某个文件读不出）应尽量跳过/部分返回，只有「无法继续」（路径越界、入参非法）才 `bail!`。

---

## 8. 模型 Provider 约定（`luckcode-model`）

### 8.1 抽象

- 所有 provider 实现 `ModelProvider` trait，`stream(request) -> Result<ModelStream>`。
- 事件统一走 `ModelEvent { TextDelta, ToolCallDelta, ToolCallDone, Done }`。**新 provider 不得发明新事件类型**；有需要先在本规范登记。
- 工具调用的增量解析用「pending 累积器」模式（参考 `PendingToolCalls` / `PendingAnthropicToolCalls` / `PendingResponseToolCalls`）：每条 SSE delta 累积进缓冲，达到完成条件后 `drain_done` 产出 `ToolCallDone`，参数 JSON 非法时返回 `Err`。

### 8.2 新增 provider 的要求

- 必须提供 `new(base_url, api_key, model, ...)`、`from_env(model)`、`from_env_with_options(...)` 三档构造（参考 `OpenAiCompatibleProvider`、`AnthropicProvider`）。
- API key 读取统一走 `read_api_key(configured_env, fallback_envs, missing_message)`，支持 `api_key_env` 配置 + `LUCKCODE_*` / 原厂前缀回退；**不得把 key 打进日志或错误信息**。
- `base_url` 末尾的 `/` 用 `trim_end_matches('/')` 统一去掉。
- 流式解析必须有单元测试覆盖「增量到达 → 完整工具调用」路径（参考 `tests` 模块里的 `*_stream_parser_*` 用例）。

### 8.3 请求格式

- 三种格式由 `ModelRequestFormat` 枚举管理：`OpenAiChatCompletions` / `OpenAiResponses` / `AnthropicMessages`。新格式必须加入该枚举和 `ModelRequestFormat::parse`，并提供与既有等价的 `*_request` 构造函数。
- `tools` 为空时不输出 `tools` 字段；`temperature` / `max_tokens` 为 `None` 时不输出。

### 8.4 MockProvider

- 任何依赖模型的能力，其单元测试必须用 `MockProvider`（`MockProvider::new(static)` 或 `MockProvider::agent()`），**禁止在单元测试里请求真实模型**（成本、不稳定、需 key）。

---

## 9. CLI 约定（`luckcode-cli`）

- 参数解析用 `clap` derive：`#[derive(Debug, Parser)]` + `#[command(name = "luckcode", version, about = ...)]`，子命令用 `#[derive(Debug, Subcommand)]` 枚举。
- `main.rs` 只做：解析 → 初始化 tracing → 按 `command` 分发到 `handle_*` 函数 → 打印结果。**业务逻辑下沉到 `luckcode-core` 或对应 crate**，CLI 层不写工具/模型实现。
- 全局参数加 `#[arg(global = true)]`（如 `--debug`、`--verbose`、`--provider`、`--model`）。
- 未实现的能力（如 `--resume`、`--compact` 当前阶段）打印一句明确的「planned for vX.Y」提示并正常返回，**不要 panic 或静默成功**。
- 输出约定：正常信息 `println!`，流式 token 边输出边 `io::stdout().flush()`（参考 `handle_ask`），错误走 `eprintln!` + exit 1。
- 子命令处理器签名统一 `fn handle_xxx(...) -> Result<()>`（异步的加 `async`），返回 `Result` 由顶层统一处理退出码。

---

## 10. 配置系统约定（落实计划 §7）

- 配置优先级（高到低）：**命令行参数 → 环境变量 → 项目 `.luckcode/config.toml` → 全局 `~/.config/luckcode/config.toml` → 默认值**。新增任何配置项都必须明确它落在哪一层，且高层覆盖低层。
- 环境变量统一 `LUCKCODE_` 前缀；既有约定：`LUCKCODE_PROVIDER` / `LUCKCODE_MODEL_PROVIDER`、`LUCKCODE_MODEL`、`LUCKCODE_PERMISSION_MODE`、`LUCKCODE_MODEL_REQUEST_FORMAT`、`LUCKCODE_MODEL_TIMEOUT_SECONDS`、`LUCKCODE_MODEL_RETRY_ATTEMPTS`、`LUCKCODE_<PROVIDER>_API_KEY`、`LUCKCODE_<PROVIDER>_BASE_URL`。新增变量遵循同样命名。
- `init` 生成的模板（`AGENTS_TEMPLATE` / `PROJECT_CONFIG_TEMPLATE` / `MCP_CONFIG_TEMPLATE` / `IGNORE_TEMPLATE`）以 `const &str` 集中维护，修改模板要同步更新 `README.md` 和本规范相关示例。
- `config show` 必须能输出合并后的最终配置 + 各来源加载状态（`ConfigSource { path, loaded }`），便于排错。

---

## 11. 存储与路径约定（`luckcode-storage`）

- 用户目录遵循 XDG：配置 `XDG_CONFIG_HOME/luckcode`（回退 `~/.config/luckcode`），数据 `XDG_DATA_HOME/luckcode`（回退 `~/.local/share/luckcode`）。统一通过 `config_dir()` / `data_dir()` 获取，**禁止在业务代码里硬编码 `~/.config/...`**。
- 项目标识用 `project_hash`（路径 SHA256 前 8 字节 hex），session id 用 `ses_<uuid_v4_simple>`。所有按项目聚合的存储（session、checkpoint、memory）都以 `project_hash` 分目录。
- session 采用 **JSONL 追加写**：`create_session_jsonl`（`create_new`，不覆盖）+ `append_session_*` 系列。**只追加、不覆盖、不就地改写历史行**——这是恢复和审计的基础。
- 新增 session 事件类型时，走 `append_session_event(session, json!({ "type": "...", ... }))`，`created_at` 由该函数统一注入，不要在各调用点重复写时间戳。checkpoint 事件用专门的 `append_session_checkpoint(session, id)`；compact summary 事件用 `append_session_compact_summary(session, summary)`。
- checkpoint 存储落在 `data_dir/checkpoints/<project_hash>/<session_id>/<checkpoint_id>/`（`manifest.json` + `files/<sanitized>.before`）。新增/读取/回滚只走 `create_checkpoint` / `list_checkpoints` / `latest_checkpoint` / `restore_checkpoint`，**不要在业务代码里手写 checkpoint 目录结构**。`<sanitized>` 把路径分隔符替换为 `_`（如 `src/main.rs` → `src_main.rs.before`，与计划 §9 一致）。
- project memory 存储落在 `data_dir/memory/<project_hash>.json`，只通过 `read_project_memory` / `set_project_memory` / `remove_project_memory` 访问；Agent context 可以读取 memory，但不得把它当成高于 `AGENTS.md` / system prompt 的规则。
- 路径展示一律用 `Path::display()`；构造错误上下文必须包含相关路径。

---

## 12. 权限与安全约定（落实 `AGENTS.md` 与计划 §8）

这是 LuckCode 的红线，代码层面强制：

- **只读 vs 可写工具严格分离**：只读工具（list/read/search/detect/git_status/git_diff/list_symbols/find_symbol/find_references/module_summary）可自动执行；可写/执行类（`edit_file`、`write_file`、`run_shell`、`delete_file`、MCP 工具）必须经过权限系统。
- **默认拒绝清单**（计划 §8）：`sudo`、`rm -rf`、`chmod -R 777`、`curl ... | sh`、`wget ... | bash`、`dd`、`mkfs`、`docker system prune`、`terraform apply`、`terraform destroy`、`kubectl delete`、引用 `.env` / 私钥 / 凭据路径的命令等。命中即 `Deny`，不询问。
- `run_shell` 权限顺序为：硬拒绝清单 → 配置 denylist → 配置 allowlist → 配置 `default_policy` 或当前权限模式。第一版默认 allowlist 包含 `git status` / `git diff`；其它 shell 命令默认先询问用户。`auto` / `dangerous` 模式下仍必须在执行前展示命令，且硬拒绝清单始终生效。
- `run_shell` 必须固定在 workspace root 下执行，使用 `tokio::process::Command`，提供超时控制和 stdout/stderr 截断；非零退出码作为 tool result 返回给 Agent，不当作工具崩溃。
- `--sandbox --sandbox-executor docker` 模式下，`run_shell` 通过 Docker `run --rm --network none -v <workspace>:/workspace -w /workspace <image> sh -lc <command>` 执行；仍必须先走同一套硬拒绝、allowlist / denylist 和用户确认流程。默认 `--sandbox` 不改变执行器，只启用权限策略 sandbox。
- 配置了 `[commands].test` 时，Agent 在 `edit_file` / `write_file` 真正修改文件后自动插入一次 `run_shell` 验证调用；如果模型同一轮已经调用 `run_shell`，不额外重复插入。验证失败作为 tool result 回传给模型继续迭代。
- stdio / HTTP MCP tools 在非 `--plan` Agent 模式下注册为 `mcp_<server>_<tool>`，执行前复用 command policy、确认回调和自动展示流程；`.luckcode/mcp.json` 的 `tool_policies` 可对单个 MCP tool 配置 `allow` / `ask` / `deny`，且 `headers` / `env` 值展示时必须隐藏。
- 文件编辑流程已落地：`read_file` → 精确字符串替换 → 本地校验 → 展示 diff → 按 `EditApproval` 确认 → 建 checkpoint → apply（计划 §9）。**不允许模型输出直接覆盖整个文件**。审批不通过独立的 PermissionEngine（那是第 7 周），而是由 CLI 把 `PermissionMode` 映射成 `EditApproval` 注入 `ToolContext`，编辑工具内部据此 Refuse/Prompt/Auto。
- 路径越界检查、敏感文件跳过、文件大小上限见 §7.3。
- 不读取 `.env`、私钥、凭据（`AGENTS.md`）；不在日志/错误/session 中回传这些文件内容。
- checkpoint 与 session 默认放在 `~/.local/share/luckcode`，不放项目仓库里。

---

## 13. 日志与输出约定

- 日志统一用 `tracing` 宏（`tracing::debug!` / `info!` / `warn!` / `error!`），**库代码不得用 `println!`/`eprintln!`**——只有 `luckcode-cli` 的面向用户输出可以用。
- 日志级别约定：
  - `ERROR`：不可恢复的错误、应当告警的安全事件。
  - `WARN`：可恢复的异常、降级行为、跳过的文件。
  - `INFO`：`--verbose` 下用户可能关心的进度（启动、加载了哪个 provider、创建了哪个 session）。
  - `DEBUG`：`--debug` 下的诊断细节（请求体摘要、工具入参、流事件计数）。
- **永远不要把 API key、token、`.env` 内容、文件私钥写进日志**。需要打印请求时只打印 model/messages 摘要。
- 初始化在 `init_tracing(debug, verbose)`（参考 `main.rs`）：默认 `WARN`，`-v` → `INFO`，`--debug` → `DEBUG`，且 `without_time()`。
- 面向用户的流式输出需要 `io::stdout().flush()` 保证逐 token 显示。

---

## 14. 测试规范

### 14.1 位置与组织

- 单元测试与被测代码同文件，置于文件末尾的 `#[cfg(test)] mod tests { use super::*; ... }`。
- 跨 crate 的集成测试放在 `tests/` 目录（按需创建），但优先用单元测试。
- 测试函数名用描述性 `snake_case`，表达「在什么场景下应得到什么结果」：`resolves_provider_model_shorthand`、`anthropic_stream_parser_handles_tool_use`。

### 14.2 写法

- 断言：值相等用 `assert_eq!`，布尔用 `assert!`，模式用 `assert!(matches!(...))`。
- 测试内允许 `unwrap()`，但更推荐 `.expect("human readable reason")` 表明意图。
- **禁止在单元测试里发真实网络请求或调用真实模型**——所有模型相关测试用 `MockProvider` 或直接测纯函数（如 `openai_chat_request`、`parse_anthropic_stream_event`）。
- 涉及文件系统/临时目录的测试用 `tempfile`（走 §2 的 dev-dependency）。
- 每个新配置项、每个新工具、每个新 provider 解析分支、每个新权限规则都要配至少一个测试。

### 14.3 运行

- 修改代码后必须运行测试（`AGENTS.md` 强制）：`cargo test`。影响特定 crate 时 `cargo test -p <crate>`。
- 提交前确保 `cargo fmt --check`、`cargo clippy` 通过（见 §15）。

---

## 15. 代码风格

- **格式化以 `cargo fmt` 为准**（默认 4 空格缩进、行宽 100）。提交前 `cargo fmt`。
- **`cargo clippy` 必须无警告**（默认 lint 集）。未来可在根 `Cargo.toml` 加 `[workspace.lints]` 把 `warnings` 设为 `deny` 以强制（见 §17）。
- `use` 分组顺序：`std` → 第三方 → 本 workspace crate，组内尽量按字母序；用 `use a::{b, c};` 合并同源导入，避免散落。允许 `cargo`/rustfmt 自动整理，不要手写怪异顺序。
- 字符串格式化用内联格式参数（`format!("{path}")`、`println!("mode: {mode}")`），不要用 `format!("{}", path)`。
- 充分使用 edition 2024 的便利：`let else`、`let chains`（`if cond && let Some(x) = opt { ... }`）、`if let`、`Option::is_some_and`、`Result::is_ok_and`、`bool::then`。优先这些而非冗长 `match`。
- 注释：
  - **文档注释 `///`** 用于公开项（`pub fn` / `pub struct` / `pub trait` / `pub enum`），描述「做什么」和「为什么」，不描述「怎么实现」。
  - 代码注释 `//` 只写「为什么」，不写「做什么」（做什么看代码就知道）。中文/英文均可，**同一文件内保持一致**，优先与该文件既有注释语言一致。
  - 不写无信息量的注释（如 `// 创建变量 i`）。
- `match` 必须穷尽；新增枚举变体后立刻处理所有 `match` 分支（编译器会强制）。
- 避免 `unwrap()` / `expect()` 出现在非测试、非 `main` 的库代码里；确信不会失败时用 `.with_context(...)?` 或 `.unwrap_or_default()`。

---

## 16. 文档与变更同步

- 新增/修改对外能力（CLI 命令、工具、provider、配置项）时，**同步更新**：
  1. `README.md` 的「当前能力」和「常用命令」。
  2. 本规范对应的章节（如有约定变化）。
  3. `doc/luckcode-rust-cli-coding-agent-plan.md` 中对应阶段的状态（如已实现）。
- 配置模板（`*.TEMPLATE`）改动要同步到 `README.md` 的配置示例和 `init` 行为说明。
- 新增 crate 必须更新本规范 §1.1 的依赖图和 `doc/...plan.md` §3 的结构图。

---

## 17. 工作流与提交约定（落实 `AGENTS.md`）

- **修改代码后必须运行 `cargo test`**（或受影响 crate 的 `cargo test -p <crate>`）。
- **未经用户明确要求，不要创建 git commit**（`AGENTS.md`）。需要提交时另起分支，不在 `master` 上直接提交。
- **执行任何 shell 命令前先展示给用户**（`AGENTS.md`）；落地到 `run_shell` 工具就是「默认询问、展示、确认」。
- 提交信息沿用 Conventional Commits 风格（`feat:` / `fix:` / `refactor:` / `docs:` / `test:` / `chore:`），首行简短，正文说明动机与影响。
- 一个 PR/提交只做一件事；纯格式化（`cargo fmt`）单独提交，不与逻辑改动混在一起。
- 不读取 `.env`、不执行 `sudo`、不运行 `terraform apply`/`destroy` 等破坏性基础设施命令（`AGENTS.md`）。

---

## 18. 新增功能标准自检清单

实现任何新功能前/后，逐条对照：

- [ ] 放在正确的 crate（依赖方向符合 §1.1），不在 CLI 层写业务逻辑。
- [ ] 依赖走 workspace 声明，未在子 crate 钉版本（§2）。
- [ ] 命名符合 §3；类型有 `Debug`（必要时 `Clone/Serialize/...`）。
- [ ] 错误用 `anyhow::Result` + `.with_context()`；不在库内 `unwrap`/`exit`（§4）。
- [ ] 异步用 `#[async_trait]` + `Send + Sync`；流走 `ModelStream`/`try_stream!`（§5）。
- [ ] 配置项同时更新 完整结构体 / Partial 结构体 / `apply_to` / 环境变量 / 测试（§6.3、§10）。
- [ ] 新工具：`Tool` trait + `XxxInput` + `ToolOutput` + 注册到正确 registry + 路径/大小/敏感文件校验（§7）。
- [ ] 新 provider：三档构造 + `read_api_key` + pending 解析 + 单测；无新事件类型（§8）。
- [ ] 路径走 XDG API；session 只追加；project_hash 分目录（§11）。
- [ ] 可写/执行能力接权限系统，命中默认拒绝清单直接 Deny（§12）。
- [ ] 日志用 `tracing`，无敏感信息泄漏；用户输出在 CLI 层（§13）。
- [ ] 配齐单元测试，无真实网络/模型依赖（§14）。
- [ ] `cargo fmt` + `cargo clippy` + `cargo test` 全绿（§15、§17）。
- [ ] 同步更新 `README.md` 和本规范相关章节（§16）。

---

本规范为活文档：发现更好的约定或既有代码暴露出新模式时，先改本规范，再据此对齐代码。
