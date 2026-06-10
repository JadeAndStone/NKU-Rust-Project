交接文档

已完成 `rust-codingagent` 的 CLI 骨架：程序可以从命令行启动、读取配置、初始化日志，并进入最小主循环。当前没有实现真实 LLM 调用、会话持久化、工具调用或回滚逻辑，只提供后续模块接入的启动底座。

## 运行与测试

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

## 配置对接点

配置入口在：

```text
crates/cli/src/config.rs
```

后续模块需要的启动配置都从 `AppConfig` 读取：

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

默认配置文件名：

```text
rust-codingagent.toml
```

配置文件不是必须的；没有配置文件时使用默认值。配置优先级是：

```text
默认值 < 配置文件 < 环境变量
```

环境变量前缀是：

```text
RUST_CODINGAGENT_*
```

## 主循环对接点

主循环在：

```text
crates/cli/src/repl.rs
```

现在的行为只是读取用户输入并回显：

```text
rust-codingagent> received: ...
```

后续接入 session、LLM provider、工具调用或回滚时，主要替换这里的回显逻辑即可。CLI 层已经负责启动、配置加载和日志初始化。

## 命令对接点

命令定义在：

```text
crates/cli/src/cli.rs
```

命令分发在：

```text
crates/cli/src/lib.rs
```

如果后续需要新增命令，例如回滚预览、历史查看、工具测试等，从这两个文件接入。

## 测试对接点

CLI 集成测试在：

```text
crates/cli/tests/cli.rs
```

当前覆盖：

- `--help`
- `--config`
- `run` 主循环

后续新增功能后，把端到端命令测试继续补在这里。

