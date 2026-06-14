<div align="center">

# NKU Rust Coding Agent

用 Rust 实现的命令行 Coding Agent，支持多轮 LLM 推理、工具调度、文件操作回滚及会话管理。

![Rust](https://img.shields.io/badge/Rust-2021-b7410e?logo=rust&logoColor=white)
![Cargo Workspace](https://img.shields.io/badge/Cargo-workspace-4b5563)
![CLI](https://img.shields.io/badge/CLI-rustyline-2563eb)
![License](https://img.shields.io/badge/License-MIT-blue)
![Status](https://img.shields.io/badge/Status-Complete-brightgreen)

</div>

## 📖 项目简介

`NKU Rust Coding Agent` 是一个完整的 CLI 编码助手。用户输入自然语言，Agent 自动调用 LLM 进行多轮推理，按需执行文件读写、代码搜索、Shell 命令、PDF/DOCX 文档提取等工具，所有文件修改自动记录快照并支持一键回滚。

## 🧱 模块结构

```text
NKU-Rust-Project/
├── Cargo.toml
├── README.md
├── LICENSE
├── crates/
│   ├── cli/               # 命令行入口、配置加载、REPL 主循环
│   ├── core/              # Session、Message、Provider trait、AgentContext、SessionStore
│   ├── tools/             # Read / Write / Edit / Grep / Shell（workspace 路径保护）
│   ├── tools-doc/         # PDF / DOCX 文本提取
│   ├── rollback/          # 快照、diff、回滚预览和恢复
│   ├── agent/             # Agent 多轮调度循环、工具调用、上下文管理
│   └── provider-remote/   # DeepSeek / OpenAI-compatible HTTP + SSE 流式 Provider
├── docs/
│   ├── 1-handoff.md
│   ├── 2-core-handoff.md
│   ├── 3-tools-handoff.md
│   ├── 4-rollback-handoff.md
│   └── assets/
└── tests/
    └── integration/
```

## 🔁 核心流程

```text
你> 帮我读一下 Cargo.toml
      │
      ▼
┌─────────────────────────────────────────┐
│  CLI / rustyline REPL                   │
│  ⏳ 思考中... → ⏳ 1.2s                  │
└──────────────┬──────────────────────────┘
               │
               ▼
┌─────────────────────────────────────────┐
│  Agent 调度循环                          │
│  user → Provider(LLM) → text? → 输出    │
│                       → tool_calls?     │
│                       → 执行工具        │
│                       → rollback 记录   │
│                       → 回传结果        │
│                       → 继续循环 ↩      │
└──────┬───────────────┬─────────────────┘
       │               │
       ▼               ▼
┌──────────────┐ ┌──────────────────────┐
│  Provider    │ │  ToolExecutor         │
│  DeepSeek    │ │  read / write / edit  │
│  HTTP + SSE  │ │  grep / shell         │
│  流式输出    │ │  read_pdf / read_docx │
└──────────────┘ └──────────┬───────────┘
                            │
                            ▼
                   ┌──────────────────┐
                   │  RollbackManager │
                   │  快照 → 预览 → 恢复 │
                   └──────────────────┘
```

## ✨ 核心功能

| 能力 | 说明 |
| --- | --- |
| 🧠 多轮 LLM 推理 | Agent 调度循环，自动多轮 tool-calling 直到获取文本回复 |
| 🛠️ 7 种工具 | read、write、edit、grep、shell、read_pdf、read_docx |
| 📄 文档提取 | 支持 PDF/DOCX 文本提取，可读取 workspace 外部文件 |
| ↩️ 文件回滚 | Write/Edit 自动记录快照，支持预览 diff、按步骤恢复、按文件恢复 |
| 💬 流式输出 | SSE token 级实时输出，⏳ 思考计时 |
| 🗂️ 会话管理 | 多 session 切换、历史恢复、/clear 新对话、上下文窗口自动截断 |
| ⌨️ rustyline 输入 | 光标移动、↑↓ 历史、Tab 补全、历史持久化 |
| 🎨 可视化反馈 | 启动卡片、loading 状态、简洁工具摘要、文件修改提示 |
| 🔒 路径保护 | Read/Write/Edit/Grep 默认限制在 workspace；PDF/DOCX 可读取用户指定路径；外部文件操作通过 Shell 审批执行 |

## 🛠️ 技术栈

| 分类 | 技术 |
| --- | --- |
| 语言与构建 | Rust 2021, Cargo workspace (7 crates) |
| CLI 输入 | `rustyline`（光标、历史、补全） |
| HTTP 客户端 | `reqwest` + `tokio` |
| LLM 协议 | OpenAI-compatible `/v1/chat/completions` + SSE streaming |
| 配置与持久化 | `serde`, `toml` |
| 文档解析 | `pdf-extract`, `zip`, `quick-xml` |
| 文件搜索 | `regex`, `walkdir` |
| 日志 | `tracing`, `tracing-subscriber` |
| 错误处理 | `anyhow` |

## 🚀 快速启动

### 1. 安装命令

在项目根目录执行：

```bash
cargo install --path crates/cli --force
```

安装完成后，确保 Cargo 的 bin 目录已经在 `PATH` 里：

- Windows: `%USERPROFILE%\.cargo\bin`
- macOS / Linux: `$HOME/.cargo/bin`

之后可以在任意目录直接启动：

```bash
nku-agent
```

项目仍保留兼容命令：

```bash
rust-codingagent run
```

### 2. 本机配置模型密钥

不要把密钥写进仓库文件。建议只设置在本机环境变量中。

PowerShell：

```powershell
setx RUST_CODINGAGENT_API_KEY "<your-api-key>"
setx RUST_CODINGAGENT_PROVIDER "deepseek"
setx RUST_CODINGAGENT_MODEL "deepseek-chat"
```

macOS / Linux：

```bash
export RUST_CODINGAGENT_API_KEY="<your-api-key>"
export RUST_CODINGAGENT_PROVIDER="deepseek"
export RUST_CODINGAGENT_MODEL="deepseek-chat"
```

`setx` 写入后需要重新打开终端才会生效。`rust-codingagent.toml`、`.env`、`.env.*` 已被 `.gitignore` 排除，避免误提交本机密钥。

### 3. 启动使用

```bash
nku-agent
```

进入 REPL 后直接输入中文需求即可。Agent 会按需调用读取、写入、搜索、命令执行和回滚工具。

```text
╭────────────────────────────────────────────────────────╮
│   NKU·RS                                               │
│   南开 Rust 编程助手                                   │
│   本地代码代理 · 文件读写 · 命令审批 · 可回滚           │
├────────────────────────────────────────────────────────┤
│ 模型  deepseek/deepseek-chat            会话  新会话   │
│ 工作区  C:\Users\you                                   │
│ 快捷命令  /帮助  /会话列表  /工具  /回滚               │
╰────────────────────────────────────────────────────────╯

╰─ 你> 帮我读一下 Cargo.toml
╭─ 思考中 ...
╭─ 回答 0.8s
│ 这是一个 Cargo workspace 配置...
╰─ 完成
```

### 4. 常用命令

| 命令 | 作用 |
| --- | --- |
| `/帮助` | 查看所有命令 |
| `/会话` | 查看当前会话信息 |
| `/会话列表` | 列出所有历史会话 |
| `/会话 切换 <会话ID>` | 切换到指定会话 |
| `/历史` | 查看消息历史 |
| `/模型 [模型名]` | 查看或切换模型 |
| `/清空` | 开启新对话 |
| `/工具` | 列出可用工具 |
| `/回滚 列表` | 列出所有回滚记录 |
| `/回滚 预览 <记录ID>` | 预览回滚 diff |
| `/回滚 恢复 <记录ID>` | 执行回滚恢复 |
| `/回滚 文件 <记录ID> <路径>` | 恢复单个文件 |
| `退出` / `exit` / `q` | 退出 |

### 5. 查看配置

```bash
nku-agent config
```

支持的环境变量：

| 变量名 | 作用 |
| --- | --- |
| `RUST_CODINGAGENT_PROFILE` | 当前 profile |
| `RUST_CODINGAGENT_WORKSPACE` | 工作区路径（默认 `~/workspace`） |
| `RUST_CODINGAGENT_LOG_LEVEL` | 日志等级 |
| `RUST_CODINGAGENT_PROVIDER` | Provider 名称 |
| `RUST_CODINGAGENT_MODEL` | 模型名称 |
| `RUST_CODINGAGENT_API_BASE` | Provider API 地址 |
| `RUST_CODINGAGENT_API_KEY` | API Key |

## 🧪 验证

```bash
cargo fmt --all -- --check
cargo test --all
cargo build --workspace
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

当前测试覆盖（共 19 个）：

| crate | 测试数 | 覆盖重点 |
| --- | --- | --- |
| `cli` | 3 | 配置读取、REPL 启动退出、历史持久化、模型切换 |
| `core` | 2 | session 保存恢复、列表排序 |
| `tools` | 6 | Read/Write/Edit/Grep/Shell、覆盖保护、路径越界拒绝 |
| `tools-doc` | 1 | DOCX 文本提取 |
| `rollback` | 4 | Write/Edit 回滚记录、预览、恢复、单文件恢复、Read 不记录 |
| `agent` | 0 | （调度循环通过集成测试覆盖） |
| `provider-remote` | 0 | （HTTP/SSE 需真实 API 环境） |

## 👥 开发团队

| 成员 | 负责方向 | 主要交付 |
| --- | --- | --- |
| 成员 A | CLI 工程与系统集成 | Agent 多轮调度循环（LLM ↔ tool_call ↔ 执行 → 循环），串联为完整 Agent。启动入口、命令解析、配置加载、日志、REPL；RemoteProvider（HTTP/SSE）；rustyline 交互（光标/历史/补全/⏳ 计时/工具反馈）；回滚命令与会话管理；tools-doc；上下文管理；7 crate 整合联调 |
| 成员 B | 核心状态与会话 | session、message、history、provider trait、AgentContext、SessionStore |
| 成员 C | 基础工具层 | Read、Write、Edit、Grep、Shell、workspace 路径保护 |
| 成员 D | 版本回滚创新 | 快照、diff、回滚预览、按步骤恢复、按文件恢复、本地持久化 |
| 成员 E | 测试与集成 | CLI/core/tools/rollback 测试与最终集成整理 |

## 📌 项目说明

- `.rust-codingagent/` 是本地运行状态目录，已加入 `.gitignore`。
- 默认 workspace 为 `~/workspace`（如存在），否则为当前目录。
- 回滚记录存储为 TOML 文件，无数量限制；`/rollback list` 可查看，`/rollback apply` 可恢复。
- `read`/`read_pdf`/`read_docx`/`grep` 可访问 workspace 外路径；`write`/`edit` 仅限 workspace 内。
- Shell 命令默认 15 秒超时，`find /` 会被拦截提示。

## 📄 License

本项目使用 MIT License，详见 [LICENSE](LICENSE)。

<div align="center">

Rust CLI Coding Agent — 从会话持久化到 LLM 多轮推理，完整的编码助手底座。

</div>
