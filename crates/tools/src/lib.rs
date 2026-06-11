//! File, search, edit, and command execution tools.
//!
//! The crate exposes a small tool-calling protocol around workspace-scoped
//! Read, Write, Edit, Grep, and Shell operations.

mod edit;
mod grep;
mod path;
mod read;
mod registry;
mod shell;
mod tool;
mod write;

pub use edit::EditTool;
pub use grep::GrepTool;
pub use read::ReadTool;
pub use registry::{run_tool, ToolRegistry};
pub use shell::ShellTool;
pub use tool::{GrepMatch, Tool, ToolInput, ToolOutput, ToolRequest};
pub use write::WriteTool;

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use rust_codingagent_core::AgentContext;

    use super::{run_tool, ToolOutput, ToolRequest};

    #[test]
    fn read_write_and_edit_roundtrip() {
        let workspace = temp_workspace("tools-roundtrip");
        let context = context_for(&workspace);

        let write_output = run_tool(
            &context,
            ToolRequest::Write {
                path: PathBuf::from("notes/example.txt"),
                content: "hello tools\n".to_string(),
                overwrite: false,
            },
        )
        .unwrap();
        assert!(matches!(
            write_output,
            ToolOutput::Write {
                created: true,
                overwritten: false,
                ..
            }
        ));

        let read_output = run_tool(
            &context,
            ToolRequest::Read {
                path: PathBuf::from("notes/example.txt"),
                max_bytes: None,
            },
        )
        .unwrap();
        assert!(matches!(
            read_output,
            ToolOutput::Read { content, .. } if content == "hello tools\n"
        ));

        let edit_output = run_tool(
            &context,
            ToolRequest::Edit {
                path: PathBuf::from("notes/example.txt"),
                old: "hello".to_string(),
                new: "hi".to_string(),
            },
        )
        .unwrap();
        assert!(matches!(
            edit_output,
            ToolOutput::Edit {
                replacements: 1,
                ..
            }
        ));

        let content = fs::read_to_string(workspace.join("notes/example.txt")).unwrap();
        assert_eq!(content, "hi tools\n");

        remove_workspace(&workspace);
    }

    #[test]
    fn write_refuses_overwrite_unless_requested() {
        let workspace = temp_workspace("tools-overwrite");
        let context = context_for(&workspace);

        run_tool(
            &context,
            ToolRequest::Write {
                path: PathBuf::from("file.txt"),
                content: "first".to_string(),
                overwrite: false,
            },
        )
        .unwrap();

        let result = run_tool(
            &context,
            ToolRequest::Write {
                path: PathBuf::from("file.txt"),
                content: "second".to_string(),
                overwrite: false,
            },
        );
        assert!(result.is_err());

        let result = run_tool(
            &context,
            ToolRequest::Write {
                path: PathBuf::from("file.txt"),
                content: "second".to_string(),
                overwrite: true,
            },
        )
        .unwrap();
        assert!(matches!(
            result,
            ToolOutput::Write {
                created: false,
                overwritten: true,
                ..
            }
        ));

        remove_workspace(&workspace);
    }

    #[test]
    fn edit_requires_a_unique_match() {
        let workspace = temp_workspace("tools-edit-unique");
        let context = context_for(&workspace);
        fs::write(workspace.join("file.txt"), "same same").unwrap();

        let result = run_tool(
            &context,
            ToolRequest::Edit {
                path: PathBuf::from("file.txt"),
                old: "same".to_string(),
                new: "changed".to_string(),
            },
        );

        assert!(result.is_err());
        remove_workspace(&workspace);
    }

    #[test]
    fn read_rejects_paths_outside_workspace() {
        let base = temp_workspace("tools-escape-base");
        let workspace = base.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        fs::write(base.join("secret.txt"), "secret").unwrap();
        let context = context_for(&workspace);

        let result = run_tool(
            &context,
            ToolRequest::Read {
                path: PathBuf::from("../secret.txt"),
                max_bytes: None,
            },
        );

        assert!(result.is_err());
        remove_workspace(&base);
    }

    #[test]
    fn grep_returns_matching_lines() {
        let workspace = temp_workspace("tools-grep");
        let context = context_for(&workspace);
        fs::write(workspace.join("a.rs"), "fn main() {}\nlet session = 1;\n").unwrap();
        fs::write(workspace.join("b.txt"), "no match\n").unwrap();

        let output = run_tool(
            &context,
            ToolRequest::Grep {
                pattern: "session".to_string(),
                path: None,
                max_matches: Some(10),
            },
        )
        .unwrap();

        let ToolOutput::Grep { matches, truncated } = output else {
            panic!("expected grep output");
        };
        assert!(!truncated);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].path, PathBuf::from("a.rs"));
        assert_eq!(matches[0].line, 2);

        remove_workspace(&workspace);
    }

    #[test]
    fn shell_runs_command_in_workspace() {
        let workspace = temp_workspace("tools-shell");
        let context = context_for(&workspace);

        let output = run_tool(
            &context,
            ToolRequest::Shell {
                command: "echo hello-tools".to_string(),
                timeout_ms: Some(5_000),
                max_output_bytes: Some(1024),
            },
        )
        .unwrap();

        let ToolOutput::Shell {
            stdout, timed_out, ..
        } = output
        else {
            panic!("expected shell output");
        };
        assert!(!timed_out);
        assert!(stdout.contains("hello-tools"));

        remove_workspace(&workspace);
    }

    #[cfg(windows)]
    #[test]
    fn shell_handles_quoted_unicode_windows_paths() {
        let workspace = temp_workspace("tools-shell-中文");
        let context = context_for(&workspace);
        let target = workspace.join("子目录");

        let output = run_tool(
            &context,
            ToolRequest::Shell {
                command: format!("mkdir \"{}\"", target.display()),
                timeout_ms: Some(5_000),
                max_output_bytes: Some(1024),
            },
        )
        .unwrap();

        let ToolOutput::Shell {
            status_code,
            stderr,
            timed_out,
            ..
        } = output
        else {
            panic!("expected shell output");
        };

        assert_eq!(status_code, Some(0), "{stderr}");
        assert!(!timed_out);
        assert!(target.exists());

        remove_workspace(&workspace);
    }

    fn context_for(workspace: &Path) -> AgentContext {
        AgentContext {
            session_id: "test-session".to_string(),
            profile: "test".to_string(),
            workspace: workspace.to_path_buf(),
            provider: "local".to_string(),
            model: "stub".to_string(),
            turn_index: 0,
        }
    }

    fn temp_workspace(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("{prefix}-{nanos}"));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn remove_workspace(path: &Path) {
        if path.exists() {
            fs::remove_dir_all(path).unwrap();
        }
    }
}
