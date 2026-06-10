<div align="center">

# NKU Rust Coding Agent

一个面向命令行场景的 Rust 版 Coding Agent 框架，当前已经完成 CLI 底座、会话持久化核心层、基础工具层和代码回滚核心库，为后续接入真实模型调用与完整 Agent 调度预留了清晰接口。

![Rust](https://img.shields.io/badge/Rust-2021-b7410e?logo=rust&logoColor=white)
![Cargo Workspace](https://img.shields.io/badge/Cargo-workspace-4b5563)
![License](https://img.shields.io/badge/License-MIT-blue)
![Status](https://img.shields.io/badge/Status-CLI%20%7C%20Core%20%7C%20Tools%20%7C%20Rollback%20Ready-green)

</div>

这份 `README.md` 有两个用途：

1. 作为项目使用说明，说明如何编译、运行、测试和继续开发。
2. 作为课程展示和报告素材，说明当前已经完成了什么、模块之间如何协作、哪些部分仍属于后续工作。

---

## 0. 当前自查结论

我已经按当前源码、`docs/` 交接文档和 Cargo 配置重新核对过项目状态。结论如下：

- 已完成 Rust workspace 工程结构，包含 `cli`、`core`、`tools`、`rollback` 四个 crate。
- 已完成命令行入口：支持 `run`、`config`、`--help`、`--config <FILE>`。
- 已完成配置加载：默认配置、TOML 配置文件和 `RUST_CODINGAGENT_*` 环境变量合并。
- 已完成 REPL 主循环：可以启动、读取用户输入、退出，并支持基础内部命令。
- 已完成会话层：消息历史、会话恢复、模型名切换、会话本地持久化。
- 已完成 Provider 抽象：已经定义 trait 和请求/响应结构，但还没有真实 LLM 调用实现。
- 已完成工具层：文件读取、写入、编辑、正则搜索、shell 命令执行。
- 已完成 workspace 路径保护：工具层默认拒绝访问工作区外路径。
- 已完成基础测试：CLI、core、tools、rollback 都有对应测试。
- 已完成回滚核心库：支持 Write/Edit 修改前后快照、diff 记录、回滚预览、按步骤恢复、按文件恢复和本地持久化历史。

因此，从“当前代码实现”角度看，本项目已经具备一个可运行、可测试、可继续接入 Agent 调度逻辑的底座；从“完整 Coding Agent 产品”角度看，真实模型调用、工具自动调度、回滚 CLI 命令和端到端集成仍是后续重点。

---

## 1. 快速开始

### 1.1 编译整个 workspace

```powershell
cargo build --workspace
```

### 1.2 查看命令帮助

```powershell
cargo run -- --help
```

当前 CLI 会输出：

```text
Rust Coding Agent CLI framework

Usage: rust-codingagent.exe [OPTIONS] [COMMAND]

Commands:
  run     Start the agent main loop
  config  Print the effective configuration after file and environment merging
  help    Print this message or the help of the given subcommand(s)
```

### 1.3 查看最终生效配置

```powershell
cargo run -- config
```

默认情况下会得到类似输出：

```toml
profile = "default"
workspace = '<当前项目目录>'
log_level = "info"

[provider]
name = "local"
model = "stub"
```

### 1.4 启动交互主循环

```powershell
cargo run -- run
```

进入 REPL 后可以输入：

```text
hello
/session
/history
/model better-model
/session
exit
```

普通输入会被保存为用户消息，同时生成一个占位 assistant 回复：

```text
received: hello
```

这里的回复只是当前阶段的 stub 行为，不代表已经接入真实大模型。

### 1.5 开发检查命令

```powershell
cargo fmt --all -- --check
cargo test --all
cargo build --workspace
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

---

## 一、项目定位

本项目围绕“用 Rust 构建一个命令行 Coding Agent 底座”展开。它不是直接做一个完整桌面端智能编程助手，而是先把 CLI 场景中最核心、最容易验证、最适合课程展示的部分拆出来实现：

1. 命令行程序如何启动和读取配置。
2. Agent 会话如何保存和恢复。
3. 用户输入、模型配置、消息历史如何组织。
4. Agent 后续修改代码时需要哪些基础工具能力。
5. 回滚模块应该如何与工具调用和会话上下文对接。

这种路线的好处是，项目不会一开始就被复杂模型调用、GUI 交互或异步任务拖住，而是先形成一个稳定底座。当前代码回滚核心库已经落地，后续无论是接 OpenAI-compatible API、本地模型，还是把工具调用和回滚命令真正接进 REPL，都可以在现有 crate 边界上继续扩展。

---

## 二、功能完成情况

### 1. CLI 与配置层

对应 crate：

```text
crates/cli
```

已实现功能：

- 使用 `clap` 解析命令行参数。
- 默认子命令为 `run`。
- 支持 `config` 子命令打印合并后的配置。
- 支持 `--config <FILE>` 指定 TOML 配置文件。
- 使用 `tracing` 和 `tracing-subscriber` 初始化日志。
- 通过 `App` 统一启动主循环。

当前命令：

| 命令 | 作用 |
| --- | --- |
| `cargo run -- --help` | 查看 CLI 帮助 |
| `cargo run -- config` | 打印最终生效配置 |
| `cargo run -- run` | 启动 REPL 主循环 |

配置优先级：

```text
默认值 < rust-codingagent.toml < --config 指定文件 < 环境变量
```

当前支持的环境变量：

| 变量名 | 作用 |
| --- | --- |
| `RUST_CODINGAGENT_PROFILE` | 当前 profile |
| `RUST_CODINGAGENT_WORKSPACE` | 工作区路径 |
| `RUST_CODINGAGENT_LOG_LEVEL` | 日志等级 |
| `RUST_CODINGAGENT_PROVIDER` | Provider 名称 |
| `RUST_CODINGAGENT_MODEL` | 模型名称 |
| `RUST_CODINGAGENT_API_BASE` | Provider API 地址 |

### 2. REPL 主循环

REPL 目前完成的是最小可运行闭环：

1. 启动时读取配置。
2. 创建或恢复 active session。
3. 打印 session、模型和 workspace 信息。
4. 循环读取用户输入。
5. 识别内部命令。
6. 普通输入写入会话历史。
7. 生成 `received: ...` 占位回复。
8. 保存会话。
9. 遇到 `exit` 或 `quit` 退出。

当前内部命令：

| REPL 命令 | 作用 |
| --- | --- |
| `/session` | 查看当前会话 ID、profile、消息数量、模型和 workspace |
| `/history` | 打印当前会话历史 |
| `/model` | 查看当前模型 |
| `/model <name>` | 修改当前 session 的模型名并持久化 |
| `exit` / `quit` | 退出主循环 |

### 3. Core 会话层

对应 crate：

```text
crates/core
```

core 层负责所有和状态相关的共享概念，不直接处理终端 IO，也不负责命令解析。

已实现类型：

| 类型 | 作用 |
| --- | --- |
| `Session` | 当前会话，包括 ID、profile、workspace、provider 和消息历史 |
| `SessionId` | 会话 ID 类型别名 |
| `SessionSummary` | 会话列表摘要 |
| `Message` | 单条消息 |
| `MessageRole` | `System`、`User`、`Assistant`、`Tool` |
| `ConversationHistory` | 消息历史容器 |
| `ProviderConfig` | 模型服务配置 |
| `ProviderRequest` | Provider 请求结构 |
| `ProviderResponse` | Provider 响应结构 |
| `LanguageProvider` | 后续真实 LLM 接入的 trait |
| `AgentContext` | tools 和 rollback 共用上下文 |
| `SessionStore` | 会话本地存储 |

会话保存位置：

```text
.rust-codingagent/
  active-default.toml
  sessions/
    default/
      s-xxxx.toml
```

`.rust-codingagent/` 已经写入 `.gitignore`，属于本地运行状态，不应该提交到仓库。

### 4. Tools 工具层

对应 crate：

```text
crates/tools
```

tools 层已经按照 core 的 `AgentContext` 对接，不直接依赖 CLI 配置。这样做的好处是：后续无论由 REPL、Agent 调度器还是 rollback 模块调用工具，都可以统一使用同一套上下文。

已实现工具：

| 工具 | 输入 | 输出 | 主要约束 |
| --- | --- | --- | --- |
| `ReadTool` | 文件路径、最大字节数 | 文件内容、字节数、是否截断 | 只能读取 workspace 内已有文件 |
| `WriteTool` | 文件路径、内容、是否覆盖 | 写入字节数、是否新建、是否覆盖 | 默认拒绝覆盖已有文件 |
| `EditTool` | 文件路径、旧文本、新文本 | 替换次数、修改前后字节数 | 旧文本必须且只能出现一次 |
| `GrepTool` | 正则、搜索路径、最大匹配数 | 匹配文件、行号、列号、行内容 | 搜索路径不能逃出 workspace |
| `ShellTool` | 命令、超时、最大输出 | 状态码、stdout、stderr、是否超时 | 在 workspace 目录下执行 |

统一调用入口：

```rust
use rust_codingagent_tools::{run_tool, ToolRequest};

let output = run_tool(
    &context,
    ToolRequest::Read {
        path: "Cargo.toml".into(),
        max_bytes: Some(4000),
    },
)?;
```

工具层最重要的设计点是路径保护。读取已有路径时使用 `resolve_existing_path`，写入新路径时使用 `resolve_write_path`，两者都会确认最终路径仍在 `context.workspace` 下。这样可以避免工具调用默认读写项目外文件。

### 5. Rollback 回滚模块

对应 crate：

```text
crates/rollback
```

rollback 层是当前项目的创新模块，已经完成核心库能力，能够直接包装 `rust_codingagent_tools::ToolRequest` / `ToolOutput`，并借助 `AgentContext` 把回滚记录关联到 session、profile、workspace 和 turn index。

已实现能力：

- `RollbackManager`：管理当前 session 下的回滚记录。
- `run_tool_with_rollback`：包装工具调用，在执行 `Write` 和 `Edit` 时自动记录回滚信息。
- `before_snapshot` / `after_snapshot`：保存修改前和修改后的文本快照。
- `changed_files`：记录文件是新建、删除、修改还是未变化。
- `diffs`：保存轻量级文本 diff，方便课程展示和 CLI 预览。
- `preview(record_id)`：展示“当前状态 -> 回滚目标状态”的实际变化。
- `restore(record_id)`：按一次工具调用恢复全部相关文件。
- `restore_file(record_id, path)`：只恢复某条记录中的单个文件。
- 本地持久化回滚历史。

当前自动记录的工具：

```text
Write
Edit
```

当前不自动记录的工具：

```text
Read
Grep
Shell
```

原因是 `Read` 和 `Grep` 不修改文件；`Shell` 可能产生副作用，但 tools 层无法准确知道命令内部修改了哪些文件，所以后续需要在 CLI 或 Agent 调度层单独设计确认或全局快照策略。

回滚记录保存在 workspace 下：

```text
.rust-codingagent/
  rollback/
    <profile>/
      <session_id>/
        r-<turn_index>-<created_at_ms>.toml
```

功能演示截图位于：

![成员 D 回滚功能演示](docs/assets/member-d-rollback-demo-screenshot.png)

---

## 三、核心设计与实现

### 1. 为什么拆成四个 crate

项目没有把所有代码都放进一个 `src/main.rs`，而是用 workspace 拆分成多个 crate：

```text
cli       -> 负责命令行入口、配置、REPL
core      -> 负责会话、消息、状态、Provider 抽象
tools     -> 负责文件与命令工具能力
rollback  -> 负责快照、diff、预览和恢复
```

这种拆分正好对应 Agent CLI 的执行链路：

```text
用户输入
  |
  v
cli 解析命令并进入 REPL
  |
  v
core 保存会话和上下文
  |
  v
provider 后续生成回复或工具调用计划
  |
  v
tools 执行文件读写、搜索、命令
  |
  v
rollback 记录修改并支持恢复
```

当前已经打通的是 `cli -> core`、`core -> tools` 的基础接口，以及 `rollback -> tools` 的包装式工具调用；还没有打通的是“真实 provider 生成工具调用计划 -> tools 执行 -> rollback 记录 -> provider 汇总回复”的完整 Agent 链路。

### 2. 会话持久化设计

会话的核心结构是 `Session`：

```rust
pub struct Session {
    pub id: SessionId,
    pub profile: String,
    pub workspace: PathBuf,
    pub provider: ProviderConfig,
    pub history: ConversationHistory,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}
```

设计要点：

- `id` 用来区分不同会话。
- `profile` 用来支持不同配置场景。
- `workspace` 让工具层知道自己应该在哪个目录工作。
- `provider` 保存当前模型配置。
- `history` 保存多轮输入输出。
- `created_at_ms` 和 `updated_at_ms` 支持后续列表排序和状态展示。

当前 session id 使用时间戳和进程 ID 生成，课程项目阶段够用；如果后续要做更严格的并发或跨机器同步，可以替换为 UUID。

### 3. Provider 抽象设计

core 层只定义 Provider trait，不绑定具体服务：

```rust
pub trait LanguageProvider {
    fn name(&self) -> &str;

    fn model(&self) -> &str;

    fn complete(&self, request: ProviderRequest) -> Result<ProviderResponse>;
}
```

这样可以避免一开始就把项目和某个具体 API 强绑定。后续如果接入真实模型，可以新增一个 provider crate 或模块实现这个 trait，例如：

- OpenAI-compatible HTTP provider
- 本地模型 provider
- 课程演示用 mock provider

当前 REPL 使用的是 `received: ...` 占位回复，没有调用该 trait。

### 4. 工具协议设计

工具层把输入和输出都设计成 enum：

```rust
pub enum ToolRequest {
    Read { path, max_bytes },
    Write { path, content, overwrite },
    Edit { path, old, new },
    Grep { pattern, path, max_matches },
    Shell { command, timeout_ms, max_output_bytes },
}
```

```rust
pub enum ToolOutput {
    Read { path, content, bytes, truncated },
    Write { path, bytes, created, overwritten },
    Edit { path, replacements, bytes_before, bytes_after },
    Grep { matches, truncated },
    Shell { status_code, stdout, stderr, timed_out, stdout_truncated, stderr_truncated },
}
```

这里的好处是：

- CLI 可以根据不同输出类型做格式化展示。
- Agent 调度器可以把工具结果重新放回消息历史。
- 回滚模块可以从 `Write` 和 `Edit` 输出里识别文件变化。
- 测试可以直接断言结构化结果，而不是解析字符串。

### 5. 路径安全设计

Coding Agent 的工具层必须小心文件路径。当前项目的路径处理原则是：

1. `context.workspace` 先 canonicalize。
2. 相对路径拼到 workspace 下。
3. 绝对路径必须仍然位于 workspace 内。
4. `../` 这种路径会被规范化后再检查。
5. 已存在路径和写入路径分开处理。

这不是完整的沙箱系统，但对于课程项目中的文件工具层来说，已经能防止最常见的误读、误写项目外文件问题。

---

## 四、项目结构

```text
NKU-Rust-Project/
├── Cargo.toml                  # workspace 配置
├── Cargo.lock
├── README.md
├── LICENSE
├── crates/
│   ├── cli/
│   │   ├── Cargo.toml
│   │   ├── src/
│   │   │   ├── app.rs          # App 启动封装
│   │   │   ├── cli.rs          # clap 命令定义
│   │   │   ├── config.rs       # 配置加载与合并
│   │   │   ├── lib.rs          # CLI 分发入口
│   │   │   ├── main.rs         # 二进制入口
│   │   │   ├── repl.rs         # REPL 主循环
│   │   │   └── telemetry.rs    # 日志初始化
│   │   └── tests/
│   │       └── cli.rs          # CLI 集成测试
│   ├── core/
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── context.rs      # AgentContext
│   │       ├── message.rs      # Message 与 ConversationHistory
│   │       ├── provider.rs     # Provider trait 与配置
│   │       ├── session.rs      # Session
│   │       ├── store.rs        # SessionStore
│   │       └── time.rs         # 时间戳工具
│   ├── tools/
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── edit.rs         # 编辑工具
│   │       ├── grep.rs         # 搜索工具
│   │       ├── path.rs         # workspace 路径保护
│   │       ├── read.rs         # 读取工具
│   │       ├── registry.rs     # 工具分发
│   │       ├── shell.rs        # shell 执行工具
│   │       ├── tool.rs         # 工具协议
│   │       └── write.rs        # 写入工具
│   └── rollback/
│       ├── Cargo.toml
│       └── src/
│           └── lib.rs          # 回滚记录、预览和恢复逻辑
├── docs/
│   ├── 1-handoff.md            # CLI 层交接文档
│   ├── 2-core-handoff.md       # Core 层交接文档
│   ├── 3-tools-handoff.md      # Tools 层交接文档
│   ├── 4-rollback-handoff.md   # Rollback 层交接文档
│   └── assets/
│       └── member-d-rollback-demo-screenshot.png
└── tests/
    └── integration/
        └── README.md
```

---

## 五、测试与验证

当前项目的测试分布如下：

| 位置 | 覆盖内容 |
| --- | --- |
| `crates/cli/src/config.rs` | TOML 配置读取 |
| `crates/cli/src/repl.rs` | REPL 启动、退出、历史持久化、模型切换 |
| `crates/cli/tests/cli.rs` | CLI 帮助、配置命令、主循环 |
| `crates/core/src/store.rs` | session 保存、恢复、列表排序 |
| `crates/tools/src/lib.rs` | Read/Write/Edit/Grep/Shell 与路径保护 |
| `crates/rollback/src/lib.rs` | Write/Edit 回滚记录、预览、恢复、单文件恢复、Read 不记录 |

推荐每次提交前执行：

```powershell
cargo fmt --all -- --check
cargo test --all
cargo build --workspace
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

当前本地验证结果：

```text
cargo fmt --all -- --check                          通过
cargo test --all                                    18 passed; 0 failed
cargo build --workspace                             通过
cargo clippy --workspace --all-targets --all-features -- -D warnings  通过
```

---

## 六、团队分工说明

根据项目已有 README 和交接文档，当前分工可以整理为：

| 成员 | 负责方向 | 当前状态 |
| --- | --- | --- |
| 成员 A | CLI 与工程骨架 | 已完成启动入口、命令解析、配置加载、日志和最小主循环 |
| 成员 B | 核心状态与会话 | 已完成 session、message、history、provider trait、AgentContext、SessionStore |
| 成员 C | 基础工具层 | 已完成 Read、Write、Edit、Grep、Shell 和 workspace 路径保护 |
| 成员 D | 版本回滚创新 | 已完成快照、diff、预览、按步骤恢复、按文件恢复和本地持久化 |
| 成员 E | 测试与集成 | 已有 CLI/core/tools/rollback 测试，后续还需补 CLI 回滚命令和端到端 Agent 流程测试 |

---

## 七、当前限制

这部分很重要，课程展示时建议直接说明，避免把规划误讲成已实现：

- 当前没有真实 LLM provider，只定义了 `LanguageProvider` trait。
- REPL 的 assistant 回复仍是 `received: ...` 占位逻辑。
- 工具层已经完成库接口，但尚未接入 REPL 命令或自动工具调用流程。
- `rollback` crate 已完成核心库能力，但还没有接入用户可见的 CLI 命令。
- `ShellTool` 无法知道命令内部修改了哪些文件，当前 rollback 不自动记录 Shell，后续如果要纳入回滚，需要单独设计策略。
- `EditTool` 是精确字符串替换，不是 patch/diff 语义。
- rollback diff 是轻量级文本 diff，适合课程展示和 CLI 预览，不等同于完整 git patch。
- rollback 当前只支持文本文件快照，不处理二进制文件。
- 当前 session id 生成方式适合课程项目，不是严格全局唯一方案。

---

## 八、后续开发建议

### 1. 接入真实 Provider

可以新增 provider 实现：

```text
crates/provider-openai-compatible
```

建议先实现最小能力：

1. 读取 `ProviderConfig`。
2. 把 `ConversationHistory` 转成模型请求。
3. 返回 `ProviderResponse`。
4. 在 REPL 中替换 `received: ...` 占位回复。

### 2. 把工具调用接入 Agent 流程

当前 tools 已经具备独立能力，下一步可以做一个调度层：

```text
用户输入 -> Provider 决定是否调用工具 -> ToolRegistry 执行 -> ToolOutput 写入历史 -> Provider 生成最终回复
```

课程演示阶段也可以先做手动工具命令，例如：

```text
/tool read Cargo.toml
/tool grep Session crates
/tool shell "cargo test --all"
```

### 3. 接入回滚 CLI 命令

rollback 核心库已经完成，下一步重点是让用户能从 REPL 或 CLI 里直接看到和使用这些记录。

建议新增命令：

```text
/rollback list
/rollback preview <id>
/rollback apply <id>
/rollback file <id> <path>
```

后续再考虑把一次 Agent action 中的多文件修改合并成批量 checkpoint，以及是否为 `ShellTool` 增加全局快照或执行前确认。

### 4. 补充端到端测试

当 provider、tools、rollback 在 CLI 层打通后，建议增加端到端测试：

1. 启动临时 workspace。
2. 创建 session。
3. 调用工具修改文件。
4. 保存 checkpoint。
5. 执行回滚。
6. 断言文件恢复。

---

## 九、报告中可直接使用的项目总结

本项目实现了一个基于 Rust 的命令行 Coding Agent 框架，采用 Cargo workspace 进行模块化组织，其中 `cli` crate 负责命令行入口、配置加载和 REPL 主循环，`core` crate 负责会话、消息历史、Provider 抽象和本地状态持久化，`tools` crate 负责文件读取、写入、编辑、搜索和 shell 命令执行，`rollback` crate 负责代码修改前后快照、diff 记录、回滚预览、按步骤恢复和按文件恢复。当前项目已经能够启动命令行程序、读取配置、恢复会话、记录对话历史、切换模型名，并通过统一 `AgentContext` 支撑工具层和回滚层协作。项目通过了格式检查、测试、workspace 编译和 clippy 静态检查，具备继续接入真实模型调用、工具自动调度和回滚 CLI 命令的工程基础。

---

## 十、大模型辅助使用说明

如果课程报告需要说明大模型辅助情况，可以按实际情况参考下面这段：

> 本项目在开发和文档整理过程中使用了大模型辅助工具进行需求梳理、README 改写、代码结构核查和验证命令整理。最终提交前已根据本地源码、Cargo 配置、交接文档和实际验证结果进行人工复核，确保 README 中的功能描述与当前代码实现保持一致；其中回滚核心库已按源码说明为已实现功能，真实 LLM 调用和回滚 CLI 集成仍按后续工作处理。

---

## License

本项目使用 MIT License，详见 [LICENSE](LICENSE)。
