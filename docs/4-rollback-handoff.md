# Rollback 版本回滚模块交接文档

本文档说明成员 D 已完成的 `crates/rollback` 创新模块，包括快照记录、diff、回滚预览、按步骤恢复、按文件恢复，以及给成员 E 的集成建议。

## 已完成功能

当前已完成 `crates/rollback` 的核心库能力，能直接对接成员 B 的 `AgentContext` 和成员 C 的 `rust_codingagent_tools::ToolRequest` / `ToolOutput`。

已实现内容包括：

- 回滚记录管理器 `RollbackManager`
- 工具调用包装函数 `run_tool_with_rollback`
- 修改前快照 `before_snapshot`
- 修改后快照 `after_snapshot`
- 文件级变更记录 `changed_files`
- 文本 diff 记录 `diffs`
- 回滚预览 `preview`
- 按单次工具调用恢复 `restore`
- 按单个文件恢复 `restore_file`
- 本地持久化回滚历史

## 功能演示截图

下面截图展示了成员 D 新增回滚功能的完整演示流程：先通过 Write 新建文件，再通过 Edit 将 `41` 改为 `42`，随后预览回滚 diff，执行恢复后文件内容回到 `41`，最后对新建文件执行回滚并确认文件已删除。

![成员 D 回滚功能演示](assets/member-d-rollback-demo-screenshot.png)

## 相关文件

```text
crates/rollback/Cargo.toml
crates/rollback/src/lib.rs
docs/4-rollback-handoff.md
docs/assets/member-d-rollback-demo-screenshot.png
```

## 对外导出的核心类型

`crates/rollback/src/lib.rs` 目前直接导出以下公开类型：

```rust
RollbackManager
RollbackRecord
RollbackRecordSummary
RecordedToolOutput
RollbackPreview
FileRollbackPreview
RestoreReport
RestoredFile
FileSnapshot
ChangedFile
FileDiff
FileChangeKind
RestoreAction
run_tool_with_rollback
```

推荐上层优先使用：

```rust
use rust_codingagent_rollback::{run_tool_with_rollback, RollbackManager};
```

## 推荐调用方式

最简单的方式是直接包装工具调用：

```rust
use std::path::PathBuf;

use rust_codingagent_rollback::run_tool_with_rollback;
use rust_codingagent_tools::ToolRequest;

let result = run_tool_with_rollback(
    &context,
    ToolRequest::Edit {
        path: PathBuf::from("src/main.rs"),
        old: "old text".to_string(),
        new: "new text".to_string(),
    },
)?;

if let Some(record) = result.record {
    println!("rollback record: {}", record.id);
}
```

如果需要显式管理历史、预览和恢复，可以创建 manager：

```rust
use rust_codingagent_rollback::RollbackManager;

let manager = RollbackManager::new(context.clone())?;
let records = manager.list_records()?;
let preview = manager.preview(&records[0].id)?;
let report = manager.restore(&records[0].id)?;
```

## 当前记录哪些工具

当前自动记录：

```text
Write
Edit
```

不自动记录：

```text
Read
Grep
Shell
```

原因：

- `Read` 和 `Grep` 不修改文件。
- `Shell` 可能产生副作用，但当前 tools 层无法准确知道它修改了哪些文件；建议成员 E 后续在 CLI 层对 Shell 做用户确认，或单独实现全局快照策略。

## 持久化位置

回滚记录保存在 workspace 下：

```text
.rust-codingagent/
  rollback/
    <profile>/
      <session_id>/
        r-<turn_index>-<created_at_ms>.toml
```

`.rust-codingagent/` 已在 `.gitignore` 中，不应提交到仓库。

## 回滚记录字段

每条 `RollbackRecord` 包含：

```text
id
session_id
profile
turn_index
workspace
tool_name
changed_files
before_snapshot
after_snapshot
diffs
created_at_ms
```

其中：

- `before_snapshot` 用于真正恢复。
- `after_snapshot` 用于说明工具执行后的状态。
- `diffs` 保存执行前到执行后的文本 diff。
- `changed_files` 标明文件是 Created、Deleted、Modified 还是 Unchanged。

## 回滚预览

`preview(record_id)` 会读取当前工作区文件状态，并生成“当前状态 -> 回滚目标状态”的 diff。

这意味着即使文件在记录生成后又被改过，预览仍会展示本次恢复实际会造成的变化。

支持的恢复动作：

```text
RestorePreviousContent  恢复旧内容
DeleteCreatedFile       删除本次新建的文件
NoChange                当前内容已经等于回滚目标
NothingToRestore        文件当前不存在，且回滚目标也不存在
```

## 恢复方式

按步骤恢复，即恢复某一次工具调用涉及的全部文件：

```rust
manager.restore(record_id)?;
```

按文件恢复，即只恢复某条记录中的一个文件：

```rust
manager.restore_file(record_id, "src/main.rs")?;
```

## 路径安全

回滚层会将所有文件路径限制在 `context.workspace` 内：

- workspace 会先 canonicalize。
- 绝对路径和相对路径都会转换为 workspace 内相对路径保存。
- 试图通过 `../` 逃出 workspace 会返回错误。
- 回滚目标必须是文本文件；当前不处理二进制文件。

## 自动测试

测试位于：

```text
crates/rollback/src/lib.rs
```

运行：

```bash
cargo test -p rust-codingagent-rollback
cargo test --all
```

当前测试覆盖：

- Write 新建文件后可通过回滚删除。
- Edit 修改文件后可恢复旧内容。
- `restore_file` 只恢复指定文件。
- Read 不会创建回滚记录。
- 回滚记录可持久化并通过 `list_records` 读取。

## 给成员 E：测试与集成建议

建议成员 E 后续优先做三件事：

1. 在 CLI 或 agent 调度层中，把会修改文件的工具调用统一替换为 `run_tool_with_rollback`。
2. 增加用户可见命令，例如 `/rollback list`、`/rollback preview <id>`、`/rollback apply <id>`、`/rollback file <id> <path>`。
3. 为 Shell 的副作用单独设计策略：默认不自动回滚，或在执行前提示用户确认全局快照。

## 当前限制

- 当前只支持文本文件快照，不支持二进制文件。
- diff 是轻量级行 diff，适合课程展示和 CLI 预览，不等同于完整 git patch。
- 当前未直接接入 CLI 命令，需要成员 E 在集成阶段接入。
- 当前以单次 Write/Edit 为一个回滚步骤；如果未来一次 agent action 修改多个文件，可以在调度层扩展批量 checkpoint。

## 交付状态

成员 D 的版本回滚创新模块已经完成，可以交给成员 E 做测试、CLI 集成和最终演示整理。

完成标准对应情况：

```text
能自动快照：已完成
能记录 diff：已完成
能回滚预览：已完成
能按步骤恢复：已完成
能按文件恢复：已完成
能与 AgentContext / ToolRequest 对接：已完成
```
