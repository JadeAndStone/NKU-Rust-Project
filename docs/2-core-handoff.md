# Core 会话层对接文档

本文档说明成员 B 已完成的 core 会话层功能、对外接口、使用方式和后续模块对接方式。

## 已完成功能

当前已完成 `crates/core` 的基础会话层，并已接入 `crates/cli` 的 REPL 主循环。

已实现内容包括：

- 会话数据结构 `Session`
- 消息数据结构 `Message`
- 消息角色 `MessageRole`
- 会话历史 `ConversationHistory`
- Provider 配置 `ProviderConfig`
- Provider 抽象 trait `LanguageProvider`
- 工具层/回滚层共享上下文 `AgentContext`
- 会话持久化存储 `SessionStore`
- CLI 中的会话恢复、历史记录、模型切换

## 相关文件

```text
crates/core/src/lib.rs
crates/core/src/context.rs
crates/core/src/message.rs
crates/core/src/provider.rs
crates/core/src/session.rs
crates/core/src/store.rs
crates/core/src/time.rs
crates/cli/src/repl.rs
```

## Core 对外导出的类型

`crates/core/src/lib.rs` 已统一导出以下类型：

```rust
pub use context::AgentContext;
pub use message::{ConversationHistory, Message, MessageRole};
pub use provider::{LanguageProvider, ProviderConfig, ProviderRequest, ProviderResponse};
pub use session::{Session, SessionId, SessionSummary};
pub use store::SessionStore;
```

后续模块建议只从 `rust_codingagent_core` crate 引入这些公开类型，不要直接依赖内部模块路径。

示例：

```rust
use rust_codingagent_core::{AgentContext, Message, SessionStore};
```

## 会话数据结构

核心会话类型为 `Session`：

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

字段说明：

- `id`：当前会话 ID
- `profile`：配置 profile
- `workspace`：当前工作目录
- `provider`：当前模型服务配置
- `history`：消息历史
- `created_at_ms`：创建时间，Unix 毫秒
- `updated_at_ms`：更新时间，Unix 毫秒

## 消息与历史记录

消息类型为 `Message`：

```rust
pub struct Message {
    pub role: MessageRole,
    pub content: String,
    pub created_at_ms: u64,
}
```

当前支持的消息角色：

```rust
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
}
```

创建消息的推荐方式：

```rust
let user_msg = Message::user("hello");
let assistant_msg = Message::assistant("received: hello");
let tool_msg = Message::tool("tool result");
```

添加消息：

```rust
session.add_message(Message::user("hello"));
```

读取历史：

```rust
for message in session.history.messages() {
    println!("{:?}: {}", message.role, message.content);
}
```

## 会话持久化

会话由 `SessionStore` 负责保存和恢复。

创建 store：

```rust
let store = SessionStore::new(&workspace, &profile);
```

获取当前 active session，如果不存在则创建：

```rust
let session = store.get_or_create_active_session(
    profile,
    workspace,
    provider_config,
)?;
```

保存会话：

```rust
store.save_session(&session)?;
```

加载 active session：

```rust
let session = store.load_active_session()?;
```

列出历史 session：

```rust
let sessions = store.list_sessions()?;
```

## 持久化文件位置

会话数据保存在当前 workspace 下：

```text
.rust-codingagent/
  active-default.toml
  sessions/
    default/
      s-xxxx.toml
```

说明：

- `.rust-codingagent/` 是本地运行状态目录
- `active-<profile>.toml` 记录当前 profile 的 active session
- `sessions/<profile>/` 保存该 profile 下的 session 文件
- 该目录已加入 `.gitignore`，不应提交到仓库

## Provider 抽象

当前 core 只提供 provider trait，不包含真实 LLM 调用。

```rust
pub trait LanguageProvider {
    fn name(&self) -> &str;

    fn model(&self) -> &str;

    fn complete(&self, request: ProviderRequest) -> Result<ProviderResponse>;
}
```

后续如果实现真实模型调用，可以由对应模块实现该 trait。

请求和响应结构：

```rust
pub struct ProviderRequest {
    pub context: AgentContext,
    pub messages: Vec<Message>,
}

pub struct ProviderResponse {
    pub message: Message,
}
```

## AgentContext 对接方式

`AgentContext` 是后续 tools 和 rollback 层推荐使用的统一上下文。

```rust
pub struct AgentContext {
    pub session_id: String,
    pub profile: String,
    pub workspace: PathBuf,
    pub provider: String,
    pub model: String,
    pub turn_index: usize,
}
```

从 session 生成上下文：

```rust
let context = session.context();
```

后续工具层可以通过 `context.workspace` 确认工作目录，通过 `context.session_id` 关联工具调用和回滚记录。

## CLI 中已接入的命令

启动主循环：

```bash
cargo run -- run
```

进入 REPL 后可以使用：

```text
/session
/history
/model
/model better-model
exit
```

命令说明：

- `/session`：显示当前 session、profile、消息数量、模型和 workspace
- `/history`：显示当前会话历史
- `/model`：查看当前模型
- `/model <name>`：切换当前会话模型，并保存到 session
- `exit` 或 `quit`：退出主循环

普通输入会保存为用户消息，同时生成一个占位 assistant 响应：

```text
rust-codingagent> hello
received: hello
```

保存的历史为：

```text
User: hello
Assistant: received: hello
```

## 手动验证流程

在项目根目录运行：

```bash
cargo run -- run
```

输入：

```text
hello
/session
/history
/model better-model
/session
exit
```

预期现象：

- `hello` 会得到 `received: hello`
- `/session` 会显示当前 session id、消息数量和模型
- `/history` 会显示 `User: hello` 和 `Assistant: received: hello`
- `/model better-model` 后，模型会变成 `local/better-model`

再次运行：

```bash
cargo run -- run
```

输入：

```text
/session
/history
exit
```

预期现象：

- 能恢复上一次的 session
- 消息数量仍然存在
- `/history` 能看到上一次的对话
- 模型仍然是之前切换后的模型

## 自动测试

完整测试命令：

```bash
cargo fmt --all -- --check
cargo test --all
cargo build
```

当前测试覆盖：

- CLI 配置加载
- CLI 主循环启动
- REPL 输入和退出
- 会话保存和恢复
- 模型切换持久化
- session 列表按更新时间排序

正常结果应为：

```text
8 passed; 0 failed
```

## 后续模块对接建议

### 给成员 C：工具层

工具层建议接收 `AgentContext`，不要直接依赖 CLI 的 `AppConfig`。

推荐接口形态：

```rust
pub trait Tool {
    fn name(&self) -> &str;
    fn run(&self, context: &AgentContext, input: ToolInput) -> anyhow::Result<ToolOutput>;
}
```

这样工具层可以通过 `context.workspace` 限定文件操作范围，通过 `context.session_id` 关联当前会话。

### 给成员 D：回滚层

回滚层建议使用：

- `context.session_id`：关联某次会话的修改历史
- `context.workspace`：定位工作区
- `context.turn_index`：关联第几轮对话产生的修改

推荐保存回滚记录时包含：

```text
session_id
turn_index
workspace
changed_files
created_at_ms
```

这样可以实现按会话、按轮次、按文件恢复。

## 当前限制

- 当前没有真实 LLM provider，只提供 trait 抽象。
- 当前没有新建/切换历史 session 的 CLI 命令。
- 当前 `/history` 没有分页。
- 当前 session id 使用时间戳和进程 ID 生成，课程项目够用，后续可替换为 UUID。
- 当前工具层和回滚层尚未实现，因此 `AgentContext` 后续可能还需要补充字段。

## 交付状态

成员 B 的核心会话层已经完成，可以交给后续成员继续对接。

完成标准对应情况：

```text
能保存和恢复会话：已完成
能切换模型配置：已完成
能让工具层拿到统一上下文：已完成
```
