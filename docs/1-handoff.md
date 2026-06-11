# CLI 工程与系统集成 交接文档

## 阶段一：CLI 骨架

已完成 `rust-codingagent` 的 CLI 骨架：程序可以从命令行启动、读取配置、初始化日志，并进入最小主循环。此时没有实现真实 LLM 调用、会话持久化、工具调用或回滚逻辑，只提供后续模块接入的启动底座。

### 运行与测试

```bash
cargo run -- --help
cargo run -- config
cargo run -- run
```

完整检查：

```bash
cargo fmt --all -- --check
cargo test --all
cargo build
```

主循环验证：

```bash
printf 'ping\nexit\n' | cargo run -- run
```

### 配置对接点

配置入口在 `crates/cli/src/config.rs`，后续模块需要的启动配置都从 `AppConfig` 读取：

```rust
pub struct AppConfig {
    pub profile: String,
    pub workspace: PathBuf,
    pub log_level: String,
    pub provider: ProviderConfig,
}

pub struct ProviderConfig {
    pub name: String,
    pub model: String,
    pub api_base: Option<String>,
}
```

配置文件 `rust-codingagent.toml` 不是必须的；没有时使用默认值。优先级：`默认值 < 配置文件 < 环境变量`，环境变量前缀 `RUST_CODINGAGENT_*`。

### 主循环对接点

主循环在 `crates/cli/src/repl.rs`，此时行为是读取用户输入并回显 `received: ...`。后续接入 session、LLM provider、工具调用或回滚时，替换这里的回显逻辑即可。

## 阶段二：系统集成 — 串联全部 crate 为完整 Agent

在 B/C/D 各自交付独立 crate 后，CLI 层完成以下集成工作，将四个孤立模块串联为一个可用的 Coding Agent。

### Agent 多轮调度循环

新建 `crates/agent/`，实现核心调度逻辑：

```
用户输入 → Agent.run_streaming()
  → Provider.complete_streaming(messages + tools)
    → Text? → 流式输出给用户，结束
    → ToolCalls? → execute_tool_call() → rollback 记录
      → 工具结果追加到 messages → 回到 Provider 继续循环
```

- 最大 50 轮工具调用保护，防止死循环
- 系统提示注入（`build_messages_with_system`），每次 API 调用自动检查并补入
- 上下文管理：token 估算（chars/3），超过 50K 自动截断旧消息，保留 system prompt + 最后 4 条
- sanitize_messages：发 API 前检查 messages，要求 tool_calls 与 tool_results 紧邻配对，不配对则丢弃，防止 400 错误
- 工具失败不回退：错误信息作为 tool_result 回传 LLM，由 LLM 自行纠正重试

### RemoteProvider — LLM 接入

新建 `crates/provider-remote/`，实现 `LanguageProvider` trait：

- HTTP POST `/v1/chat/completions`，OpenAI 兼容协议，默认 DeepSeek API
- 消息格式转换：内部 Message → OpenAI messages（含 tool_calls、tool_call_id）
- 工具定义序列化：ToolDefinition → OpenAI function-calling 格式
- SSE 流式解析：按 `index` 独立累积多个并行 tool_call 的 arguments
- 空 arguments 默认 `{}` 容错

### REPL 交互重建

`crates/cli/src/repl.rs` 和 `app.rs` 全面重写：

- **rustyline 替换裸 stdin**：光标移动、↑↓ 历史导航、Tab 补全、历史持久化到 `~/.rust-codingagent-history`
- **流式输出**：调用 `agent.run_streaming()`，SSE token 级实时打印
- **⏳ 思考计时**：用户输入后立即显示 `⏳ 思考中...`，首 token 到达后替换为 `⏳ X.Xs`（耗时）
- **工具调用可视化**：每个工具执行时显示图标 + 灰底亮字描述
  - 📖 read / 📝 write / ✏️ edit / 🔍 grep / ⚡ shell / 📖 read_pdf / read_docx
  - 执行结果：✅ 成功 + 文件路径/字节数/替换数
  - Write/Edit 自动显示回滚 ID：`↩️ /rollback apply r-X-XXXXX`
- **启动横幅**：显示 workspace、provider/model、session 信息、常用命令提示

### 命令体系

| 命令 | 功能 |
| --- | --- |
| `/help` | 查看所有命令 |
| `/session` | 当前会话信息 + 其他可用会话列表 |
| `/sessions` | 列出所有历史会话 |
| `/session resume <id>` | 切换到指定历史会话 |
| `/history` | 查看消息历史（最近 50 条），显示 tool_calls 标注 |
| `/model [name]` | 查看或切换模型 |
| `/clear` | 清空消息历史，开启新会话 |
| `/tools` | 列出可用工具（read/write/edit/grep/shell/read_pdf/read_docx） |
| `/rollback list` | 列出所有回滚记录 |
| `/rollback preview <id>` | 预览回滚 diff（当前 vs 回滚目标） |
| `/rollback apply <id>` | 执行回滚，恢复文件 |
| `/rollback file <id> <path>` | 恢复单条记录中的单个文件 |

### 配置扩展

- `AppConfig.provider` 增加 `api_key: Option<String>` 字段
- 环境变量 `RUST_CODINGAGENT_API_KEY` 支持
- 默认 workspace 改为 `~/workspace`（如存在），否则为当前目录
- Provider 工厂函数 `create_provider()`，根据配置自动选择 RemoteProvider

### 路径权限模型

| 工具 | 权限 |
| --- | --- |
| `read` / `read_pdf` / `read_docx` / `grep` | 任意路径（含 workspace 外） |
| `write` / `edit` | 仅限 workspace 内，受 rollback 保护 |
| `shell` | workspace 内执行，`find /` 拦截提示 |

### 鲁棒性修复

- **JSON 容错**：LLM 生成的 tool_call arguments 可能带有 trailing text，`parse_json_robust()` 提取第一个完整 `{}` 对象
- **write 大文件 fallback**：content 字段含未转义引号/花括号时，手动提取 path/content/overwrite
- **中文 UTF-8 边界保护**：所有字符串截断用 `is_char_boundary` 回退，防止在多字节字符中间切片 panic
- **tool_calls + tool_results 原子写入**：先收集全部工具执行结果到内存，再一次性 add_message + save_session

### 文档工具

新建 `crates/tools-doc/`：

- `extract_pdf_text(path)` — 基于 pdf-extract
- `extract_docx_text(path)` — zip + quick-xml 解析 word/document.xml，按 `<w:p>` 分段

### 最终状态

```bash
cargo build --workspace   # 编译通过
cargo test --all          # 19 passed, 0 failed
```

```
你> 帮我读一下 Cargo.toml
⏳ 思考中...
⏳ 1.3s
 📖 TOOL: 读取 Cargo.toml
 ✅ File read, 257 bytes

这是一个 Cargo workspace 配置，包含 7 个 crate...
── 1 个工具执行完毕 ──
```
