use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolOutput {
    Read {
        path: PathBuf,
        content: String,
        bytes: usize,
        truncated: bool,
    },
    Write {
        path: PathBuf,
        bytes: usize,
        created: bool,
        overwritten: bool,
    },
    Edit {
        path: PathBuf,
        replacements: usize,
        bytes_before: usize,
        bytes_after: usize,
    },
    Grep {
        matches: Vec<GrepMatch>,
        truncated: bool,
    },
    Shell {
        status_code: Option<i32>,
        stdout: String,
        stderr: String,
        timed_out: bool,
        stdout_truncated: bool,
        stderr_truncated: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrepMatch {
    pub path: PathBuf,
    pub line: usize,
    pub column: usize,
    pub line_text: String,
}

pub(crate) fn truncate_to_bytes(mut text: String, max_bytes: Option<usize>) -> (String, bool) {
    let Some(max_bytes) = max_bytes else {
        return (text, false);
    };

    if text.len() <= max_bytes {
        return (text, false);
    }

    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    (text, true)
}
