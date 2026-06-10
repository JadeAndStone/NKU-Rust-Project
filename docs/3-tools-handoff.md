# Tools 工具层对接文档

本文档说明成员 C 已完成的基础工具层功能、对外接口、使用方式，以及给成员 D 回滚模块的对接建议。

## 已完成功能

当前已完成 `crates/tools` 的基础工具层，并严格按 `docs/2-core-handoff.md` 的要求接收 `rust_codingagent_core::AgentContext`。

已实现内容包括：

- 工具统一 trait `Tool`
- 工具输入协议 `ToolInput` / `ToolRequest`
- 工具输出协议 `ToolOutput`
- 工具分发入口 `ToolRegistry`
- 便捷调用函数 `run_tool`
- 文件读取工具 `ReadTool`
- 文件写入工具 `WriteTool`
- 文件编辑工具 `EditTool`
- 代码搜索工具 `GrepTool`
- 命令执行工具 `ShellTool`
- workspace 路径保护，避免读写项目目录外的路径

## 相关文件

```text
crates/tools/src/lib.rs
crates/tools/src/tool.rs
crates/tools/src/registry.rs
crates/tools/src/path.rs
crates/tools/src/read.rs
crates/tools/src/write.rs
crates/tools/src/edit.rs
crates/tools/src/grep.rs
crates/tools/src/shell.rs
```

## 对外导出的类型

`crates/tools/src/lib.rs` 已统一导出以下类型：

```rust
pub use edit::EditTool;
pub use grep::GrepTool;
pub use read::ReadTool;
pub use registry::{run_tool, ToolRegistry};
pub use shell::ShellTool;
pub use tool::{GrepMatch, Tool, ToolInput, ToolOutput, ToolRequest};
pub use write::WriteTool;
```

后续模块建议只从 `rust_codingagent_tools` crate 引入这些公开类型，不要直接依赖内部模块路径。

示例：

```rust
use rust_codingagent_tools::{run_tool, ToolRequest, ToolOutput};
```

## 工具接口

工具层按 core 交接文档提供统一 trait：

```rust
pub trait Tool {
    fn name(&self) -> &str;

    fn run(
        &self,
        context: &AgentContext,
        input: ToolInput,
    ) -> anyhow::Result<ToolOutput>;
}
```

当前 `ToolInput` 是 `ToolRequest` 的类型别名：

```rust
pub type ToolInput = ToolRequest;
```

每个具体工具都实现了 `Tool`：

```text
ReadTool
WriteTool
EditTool
GrepTool
ShellTool
```

## 推荐调用方式

后续 CLI、agent 调度器或回滚模块可以优先使用统一入口：

```rust
use rust_codingagent_core::AgentContext;
use rust_codingagent_tools::{run_tool, ToolRequest};

let output = run_tool(
    &context,
    ToolRequest::Read {
        path: "Cargo.toml".into(),
        max_bytes: Some(4000),
    },
)?;
```

如果需要显式控制工具对象，也可以使用 `ToolRegistry`：

```rust
use rust_codingagent_tools::{ToolRegistry, ToolRequest};

let registry = ToolRegistry::new();
let output = registry.run(&context, ToolRequest::Grep {
    pattern: "Session".to_string(),
    path: Some("crates".into()),
    max_matches: Some(20),
})?;
```

## 工具输入协议

当前支持五类请求：

```rust
pub enum ToolRequest {
    Read {
        path: PathBuf,
        max_bytes: Option<usize>,
    },
    Write {
        path: PathBuf,
        content: String,
        overwrite: bool,
    },
    Edit {
        path: PathBuf,
        old: String,
        new: String,
    },
    Grep {
        pattern: String,
        path: Option<PathBuf>,
        max_matches: Option<usize>,
    },
    Shell {
        command: String,
        timeout_ms: Option<u64>,
        max_output_bytes: Option<usize>,
    },
}
```

## 工具输出协议

当前输出为结构化 enum，方便 CLI 展示，也方便回滚模块读取 changed path、字节数、匹配结果和命令状态：

```rust
pub enum ToolOutput {
    Read { path, content, bytes, truncated },
    Write { path, bytes, created, overwritten },
    Edit { path, replacements, bytes_before, bytes_after },
    Grep { matches, truncated },
    Shell {
        status_code,
        stdout,
        stderr,
        timed_out,
        stdout_truncated,
        stderr_truncated,
    },
}
```

## 各工具行为

### ReadTool

读取 workspace 内的文本文件。

- 输入：`path`、可选 `max_bytes`
- 输出：完整或截断后的文本内容
- 约束：目标必须是 workspace 内已有文件

### WriteTool

写入 workspace 内的文件。

- 输入：`path`、`content`、`overwrite`
- 输出：写入字节数、是否新建、是否覆盖
- 约束：如果文件已存在且 `overwrite = false`，会返回错误

### EditTool

对 workspace 内文件做唯一文本替换。

- 输入：`path`、`old`、`new`
- 输出：替换次数、修改前后字节数
- 约束：`old` 必须出现且只能出现一次，避免误改多处

### GrepTool

在 workspace 内搜索正则表达式。

- 输入：`pattern`、可选搜索路径、可选最大匹配数
- 输出：匹配文件、行号、列号、整行文本
- 约束：搜索路径必须在 workspace 内

### ShellTool

在 workspace 目录下执行 shell 命令。

- 输入：命令、可选超时时间、可选输出截断大小
- 输出：状态码、stdout、stderr、是否超时
- 约束：不支持交互式命令；默认超时 30 秒

## 路径安全

工具层统一通过 `path.rs` 处理路径：

- `context.workspace` 会先 canonicalize
- 已存在路径通过 `resolve_existing_path` 校验
- 写入路径通过 `resolve_write_path` 校验
- 相对路径会拼到 workspace 下
- `../` 或绝对路径如果逃出 workspace，会返回错误

这保证工具层不会默认读写项目目录外的文件。

## 给成员 D：回滚模块对接建议

回滚层建议在调用会修改文件的工具之前创建 checkpoint，重点关注：

```text
Write
Edit
```

推荐对接流程：

1. 从 `AgentContext` 读取 `session_id`、`workspace`、`turn_index`
2. 根据 `ToolRequest` 判断是否会修改文件
3. 修改前读取目标文件状态，创建 checkpoint
4. 调用 `run_tool(&context, request)`
5. 从 `ToolOutput` 读取修改后的文件路径和字节变化
6. 记录本次操作的 changed_files、tool_name、created_at_ms

推荐保存的回滚记录字段：

```text
session_id
turn_index
workspace
tool_name
changed_files
before_snapshot
after_snapshot
created_at_ms
```

其中：

- `Write` 输出里的 `created` 可以帮助判断回滚时是删除新文件还是恢复旧内容。
- `Write` 输出里的 `overwritten` 可以帮助判断是否需要保存旧文件快照。
- `Edit` 输出里的 `bytes_before` 和 `bytes_after` 可以用于回滚预览。
- `Grep` 和 `Read` 不修改文件，一般不需要 checkpoint。
- `Shell` 可能产生副作用，但当前工具层无法精确知道它改了哪些文件，建议回滚层对 `Shell` 单独做全局快照或要求用户确认。

## 当前限制

- 工具层目前只处理文本文件，不支持二进制文件编辑。
- `EditTool` 是精确字符串替换，不是 diff/patch 语义。
- `GrepTool` 使用正则表达式，不是固定字符串搜索。
- `ShellTool` 无法追踪命令内部修改了哪些文件。
- 当前尚未接入 CLI 的正式交互命令，也尚未接入真实 LLM provider。

## 自动测试

工具层测试位于：

```text
crates/tools/src/lib.rs
```

运行：

```bash
cargo test -p rust-codingagent-tools
cargo test --all
```

当前测试覆盖：

- Read/Write/Edit 基础流程
- Write 拒绝误覆盖
- Edit 要求唯一匹配
- Read 拒绝 workspace 外路径
- Grep 返回匹配行
- Shell 在 workspace 下执行命令

## 交付状态

成员 C 的基础工具层已经完成，可以交给 CLI、agent 调度器和回滚模块继续对接。

完成标准对应情况：

```text
能读文件：已完成
能写文件：已完成
能编辑文件：已完成
能搜代码：已完成
能执行命令：已完成
能通过 AgentContext 使用 workspace：已完成
```
