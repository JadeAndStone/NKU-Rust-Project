//! Snapshot, diff, preview, and restore logic for code rollback.

use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use rust_codingagent_core::AgentContext;
use rust_codingagent_tools::{run_tool, ToolOutput, ToolRequest};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileSnapshot {
    pub path: PathBuf,
    pub existed: bool,
    pub content: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangedFile {
    pub path: PathBuf,
    pub kind: FileChangeKind,
    pub bytes_before: Option<usize>,
    pub bytes_after: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileChangeKind {
    Created,
    Deleted,
    Modified,
    Unchanged,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDiff {
    pub path: PathBuf,
    pub diff: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollbackRecord {
    pub id: String,
    pub session_id: String,
    pub profile: String,
    pub turn_index: usize,
    pub workspace: PathBuf,
    pub tool_name: String,
    pub changed_files: Vec<ChangedFile>,
    pub before_snapshot: Vec<FileSnapshot>,
    pub after_snapshot: Vec<FileSnapshot>,
    pub diffs: Vec<FileDiff>,
    pub created_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RollbackRecordSummary {
    pub id: String,
    pub session_id: String,
    pub turn_index: usize,
    pub tool_name: String,
    pub changed_files: Vec<PathBuf>,
    pub created_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedToolOutput {
    pub output: ToolOutput,
    pub record: Option<RollbackRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RollbackPreview {
    pub record_id: String,
    pub files: Vec<FileRollbackPreview>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRollbackPreview {
    pub path: PathBuf,
    pub action: RestoreAction,
    pub diff: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RestoreAction {
    RestorePreviousContent,
    DeleteCreatedFile,
    NoChange,
    NothingToRestore,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreReport {
    pub record_id: String,
    pub files: Vec<RestoredFile>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoredFile {
    pub path: PathBuf,
    pub action: RestoreAction,
}

#[derive(Debug, Clone)]
pub struct RollbackManager {
    context: AgentContext,
    workspace: PathBuf,
    record_dir: PathBuf,
}

impl RollbackManager {
    pub fn new(context: AgentContext) -> Result<Self> {
        let workspace = workspace_root(&context)?;
        let record_dir = workspace
            .join(".rust-codingagent")
            .join("rollback")
            .join(sanitize_component(&context.profile))
            .join(sanitize_component(&context.session_id));

        Ok(Self {
            context,
            workspace,
            record_dir,
        })
    }

    pub fn run_tool(&self, request: ToolRequest) -> Result<RecordedToolOutput> {
        let Some(before_path) = mutating_path_from_request(&request) else {
            return Ok(RecordedToolOutput {
                output: run_tool(&self.context, request)?,
                record: None,
            });
        };

        let tool_name = tool_name(&request).to_string();
        let before_snapshot = vec![self.snapshot_user_path(&before_path)?];
        let output = run_tool(&self.context, request)?;

        let mut changed_paths = before_snapshot
            .iter()
            .map(|snapshot| snapshot.path.clone())
            .collect::<Vec<_>>();
        for path in changed_paths_from_output(&output) {
            let relative_path = self.user_path_to_relative(&path)?;
            push_unique_path(&mut changed_paths, relative_path);
        }

        let after_snapshot = self.snapshot_relative_paths(&changed_paths)?;
        let before_snapshot = self.ensure_snapshots_for_paths(before_snapshot, &changed_paths)?;
        let changed_files = changed_files(&before_snapshot, &after_snapshot);

        if changed_files.is_empty() {
            return Ok(RecordedToolOutput {
                output,
                record: None,
            });
        }

        let diffs = before_snapshot
            .iter()
            .zip(after_snapshot.iter())
            .map(|(before, after)| FileDiff {
                path: before.path.clone(),
                diff: render_diff(&before.path, before, after, "before", "after"),
            })
            .collect::<Vec<_>>();

        let created_at_ms = unix_millis();
        let id = self.next_record_id(created_at_ms)?;
        let record = RollbackRecord {
            id,
            session_id: self.context.session_id.clone(),
            profile: self.context.profile.clone(),
            turn_index: self.context.turn_index,
            workspace: self.workspace.clone(),
            tool_name,
            changed_files,
            before_snapshot,
            after_snapshot,
            diffs,
            created_at_ms,
        };

        self.save_record(&record)?;

        Ok(RecordedToolOutput {
            output,
            record: Some(record),
        })
    }

    pub fn list_records(&self) -> Result<Vec<RollbackRecordSummary>> {
        if !self.record_dir.exists() {
            return Ok(Vec::new());
        }

        let mut summaries = Vec::new();
        for entry in fs::read_dir(&self.record_dir)
            .with_context(|| format!("failed to list {}", self.record_dir.display()))?
        {
            let entry = entry?;
            if entry.path().extension().and_then(|ext| ext.to_str()) != Some("toml") {
                continue;
            }

            let record = self.read_record_file(&entry.path())?;
            summaries.push(record.summary());
        }

        summaries.sort_by_key(|summary| summary.created_at_ms);
        Ok(summaries)
    }

    pub fn load_record(&self, record_id: &str) -> Result<RollbackRecord> {
        self.read_record_file(&self.record_path(record_id)?)
    }

    pub fn preview(&self, record_id: &str) -> Result<RollbackPreview> {
        let record = self.load_record(record_id)?;
        let mut files = Vec::new();

        for before in &record.before_snapshot {
            let current = self.snapshot_relative_path(&before.path)?;
            let action = restore_action(&current, before);
            let diff = render_diff(&before.path, &current, before, "current", "rollback");
            files.push(FileRollbackPreview {
                path: before.path.clone(),
                action,
                diff,
            });
        }

        Ok(RollbackPreview {
            record_id: record.id,
            files,
        })
    }

    pub fn restore(&self, record_id: &str) -> Result<RestoreReport> {
        let record = self.load_record(record_id)?;
        self.restore_snapshots(record.id, &record.before_snapshot)
    }

    pub fn restore_file(&self, record_id: &str, path: impl AsRef<Path>) -> Result<RestoreReport> {
        let record = self.load_record(record_id)?;
        let relative_path = self.user_path_to_relative(path.as_ref())?;
        let snapshot = record
            .before_snapshot
            .iter()
            .find(|snapshot| snapshot.path == relative_path)
            .with_context(|| {
                format!(
                    "file {} is not part of rollback record {}",
                    relative_path.display(),
                    record.id
                )
            })?
            .clone();

        self.restore_snapshots(record.id, &[snapshot])
    }

    fn save_record(&self, record: &RollbackRecord) -> Result<()> {
        fs::create_dir_all(&self.record_dir)
            .with_context(|| format!("failed to create {}", self.record_dir.display()))?;
        let content =
            toml::to_string_pretty(record).context("failed to serialize rollback record")?;
        fs::write(self.record_path(&record.id)?, content)
            .with_context(|| format!("failed to write rollback record {}", record.id))?;
        Ok(())
    }

    fn read_record_file(&self, path: &Path) -> Result<RollbackRecord> {
        let content = fs::read_to_string(path)
            .with_context(|| format!("failed to read rollback record {}", path.display()))?;
        toml::from_str(&content)
            .with_context(|| format!("failed to parse rollback record {}", path.display()))
    }

    fn restore_snapshots(
        &self,
        record_id: String,
        snapshots: &[FileSnapshot],
    ) -> Result<RestoreReport> {
        let mut files = Vec::new();

        for snapshot in snapshots {
            let current = self.snapshot_relative_path(&snapshot.path)?;
            let action = restore_action(&current, snapshot);
            let path = self.absolute_for_relative(&snapshot.path)?;

            match action {
                RestoreAction::RestorePreviousContent => {
                    ensure_parent_dir(&path)?;
                    fs::write(&path, snapshot.content.as_deref().unwrap_or_default())
                        .with_context(|| format!("failed to restore {}", path.display()))?;
                }
                RestoreAction::DeleteCreatedFile => {
                    if path.exists() {
                        if !path.is_file() {
                            bail!("rollback target is not a file: {}", path.display());
                        }
                        fs::remove_file(&path)
                            .with_context(|| format!("failed to delete {}", path.display()))?;
                    }
                }
                RestoreAction::NoChange | RestoreAction::NothingToRestore => {}
            }

            files.push(RestoredFile {
                path: snapshot.path.clone(),
                action,
            });
        }

        Ok(RestoreReport { record_id, files })
    }

    fn snapshot_user_path(&self, path: &Path) -> Result<FileSnapshot> {
        let relative_path = self.user_path_to_relative(path)?;
        self.snapshot_relative_path(&relative_path)
    }

    fn snapshot_relative_paths(&self, paths: &[PathBuf]) -> Result<Vec<FileSnapshot>> {
        paths
            .iter()
            .map(|path| self.snapshot_relative_path(path))
            .collect()
    }

    fn snapshot_relative_path(&self, relative_path: &Path) -> Result<FileSnapshot> {
        let path = self.absolute_for_relative(relative_path)?;
        if !path.exists() {
            return Ok(FileSnapshot {
                path: normalize_path(relative_path),
                existed: false,
                content: None,
            });
        }

        if !path.is_file() {
            bail!("rollback target is not a file: {}", path.display());
        }

        let content = fs::read_to_string(&path)
            .with_context(|| format!("failed to read snapshot {}", path.display()))?;

        Ok(FileSnapshot {
            path: normalize_path(relative_path),
            existed: true,
            content: Some(content),
        })
    }

    fn ensure_snapshots_for_paths(
        &self,
        mut snapshots: Vec<FileSnapshot>,
        paths: &[PathBuf],
    ) -> Result<Vec<FileSnapshot>> {
        for path in paths {
            if snapshots.iter().any(|snapshot| snapshot.path == *path) {
                continue;
            }
            snapshots.push(self.snapshot_relative_path(path)?);
        }
        Ok(snapshots)
    }

    fn user_path_to_relative(&self, path: &Path) -> Result<PathBuf> {
        let candidate = if path.is_absolute() {
            normalize_path(path)
        } else {
            normalize_path(&self.workspace.join(path))
        };
        ensure_inside_workspace(&self.workspace, &candidate)?;

        let resolved = if candidate.exists() {
            dunce::canonicalize(&candidate)
                .with_context(|| format!("failed to resolve {}", candidate.display()))?
        } else {
            candidate
        };
        ensure_inside_workspace(&self.workspace, &resolved)?;

        let relative = resolved
            .strip_prefix(&self.workspace)
            .with_context(|| format!("failed to relativize {}", resolved.display()))?;
        if relative.as_os_str().is_empty() {
            bail!("workspace root cannot be used as a rollback file target");
        }
        Ok(normalize_path(relative))
    }

    fn absolute_for_relative(&self, relative_path: &Path) -> Result<PathBuf> {
        if relative_path.is_absolute() {
            bail!(
                "rollback record path must be relative: {}",
                relative_path.display()
            );
        }
        let path = normalize_path(&self.workspace.join(relative_path));
        ensure_inside_workspace(&self.workspace, &path)?;
        Ok(path)
    }

    fn next_record_id(&self, created_at_ms: u64) -> Result<String> {
        fs::create_dir_all(&self.record_dir)
            .with_context(|| format!("failed to create {}", self.record_dir.display()))?;

        for suffix in 0..1000 {
            let id = if suffix == 0 {
                format!("r-{}-{created_at_ms}", self.context.turn_index)
            } else {
                format!("r-{}-{created_at_ms}-{suffix}", self.context.turn_index)
            };
            if !self.record_path(&id)?.exists() {
                return Ok(id);
            }
        }

        bail!("failed to allocate rollback record id");
    }

    fn record_path(&self, record_id: &str) -> Result<PathBuf> {
        if !is_safe_record_id(record_id) {
            bail!("invalid rollback record id: {record_id}");
        }
        Ok(self.record_dir.join(format!("{record_id}.toml")))
    }
}

impl RollbackRecord {
    pub fn summary(&self) -> RollbackRecordSummary {
        RollbackRecordSummary {
            id: self.id.clone(),
            session_id: self.session_id.clone(),
            turn_index: self.turn_index,
            tool_name: self.tool_name.clone(),
            changed_files: self
                .changed_files
                .iter()
                .map(|file| file.path.clone())
                .collect(),
            created_at_ms: self.created_at_ms,
        }
    }
}

pub fn run_tool_with_rollback(
    context: &AgentContext,
    request: ToolRequest,
) -> Result<RecordedToolOutput> {
    RollbackManager::new(context.clone())?.run_tool(request)
}

fn mutating_path_from_request(request: &ToolRequest) -> Option<PathBuf> {
    match request {
        ToolRequest::Write { path, .. } | ToolRequest::Edit { path, .. } => Some(path.clone()),
        ToolRequest::Read { .. } | ToolRequest::Grep { .. } | ToolRequest::Shell { .. } => None,
    }
}

fn changed_paths_from_output(output: &ToolOutput) -> Vec<PathBuf> {
    match output {
        ToolOutput::Write { path, .. } | ToolOutput::Edit { path, .. } => vec![path.clone()],
        ToolOutput::Read { .. } | ToolOutput::Grep { .. } | ToolOutput::Shell { .. } => Vec::new(),
    }
}

fn tool_name(request: &ToolRequest) -> &'static str {
    match request {
        ToolRequest::Read { .. } => "read",
        ToolRequest::Write { .. } => "write",
        ToolRequest::Edit { .. } => "edit",
        ToolRequest::Grep { .. } => "grep",
        ToolRequest::Shell { .. } => "shell",
    }
}

fn changed_files(before: &[FileSnapshot], after: &[FileSnapshot]) -> Vec<ChangedFile> {
    before
        .iter()
        .zip(after.iter())
        .filter_map(|(before, after)| {
            let kind = change_kind(before, after);
            (kind != FileChangeKind::Unchanged).then(|| ChangedFile {
                path: before.path.clone(),
                kind,
                bytes_before: snapshot_bytes(before),
                bytes_after: snapshot_bytes(after),
            })
        })
        .collect()
}

fn change_kind(before: &FileSnapshot, after: &FileSnapshot) -> FileChangeKind {
    match (before.existed, after.existed) {
        (false, true) => FileChangeKind::Created,
        (true, false) => FileChangeKind::Deleted,
        (true, true) if before.content != after.content => FileChangeKind::Modified,
        _ => FileChangeKind::Unchanged,
    }
}

fn snapshot_bytes(snapshot: &FileSnapshot) -> Option<usize> {
    snapshot.content.as_ref().map(|content| content.len())
}

fn restore_action(current: &FileSnapshot, target: &FileSnapshot) -> RestoreAction {
    match (current.existed, target.existed) {
        (true, false) => RestoreAction::DeleteCreatedFile,
        (false, false) => RestoreAction::NothingToRestore,
        (_, true) if current.content == target.content => RestoreAction::NoChange,
        (_, true) => RestoreAction::RestorePreviousContent,
    }
}

fn render_diff(
    path: &Path,
    before: &FileSnapshot,
    after: &FileSnapshot,
    before_label: &str,
    after_label: &str,
) -> String {
    let mut diff = String::new();
    diff.push_str(&format!(
        "--- {} ({})\n+++ {} ({})\n",
        path.display(),
        snapshot_label(before, before_label),
        path.display(),
        snapshot_label(after, after_label)
    ));

    if before.content == after.content {
        diff.push_str("(no changes)\n");
        return diff;
    }

    let before_text = before.content.as_deref().unwrap_or_default();
    let after_text = after.content.as_deref().unwrap_or_default();
    let before_lines = before_text.lines().collect::<Vec<_>>();
    let after_lines = after_text.lines().collect::<Vec<_>>();
    let table = lcs_table(&before_lines, &after_lines);

    let mut before_index = 0;
    let mut after_index = 0;
    while before_index < before_lines.len() && after_index < after_lines.len() {
        if before_lines[before_index] == after_lines[after_index] {
            push_diff_line(&mut diff, ' ', before_lines[before_index]);
            before_index += 1;
            after_index += 1;
        } else if table[before_index + 1][after_index] >= table[before_index][after_index + 1] {
            push_diff_line(&mut diff, '-', before_lines[before_index]);
            before_index += 1;
        } else {
            push_diff_line(&mut diff, '+', after_lines[after_index]);
            after_index += 1;
        }
    }

    while before_index < before_lines.len() {
        push_diff_line(&mut diff, '-', before_lines[before_index]);
        before_index += 1;
    }
    while after_index < after_lines.len() {
        push_diff_line(&mut diff, '+', after_lines[after_index]);
        after_index += 1;
    }

    diff
}

fn snapshot_label(snapshot: &FileSnapshot, label: &str) -> String {
    if snapshot.existed {
        label.to_string()
    } else {
        format!("{label}, missing")
    }
}

fn lcs_table(before: &[&str], after: &[&str]) -> Vec<Vec<usize>> {
    let mut table = vec![vec![0; after.len() + 1]; before.len() + 1];
    for i in (0..before.len()).rev() {
        for j in (0..after.len()).rev() {
            table[i][j] = if before[i] == after[j] {
                table[i + 1][j + 1] + 1
            } else {
                table[i + 1][j].max(table[i][j + 1])
            };
        }
    }
    table
}

fn push_diff_line(diff: &mut String, prefix: char, line: &str) {
    diff.push(prefix);
    diff.push_str(line);
    diff.push('\n');
}

fn workspace_root(context: &AgentContext) -> Result<PathBuf> {
    dunce::canonicalize(&context.workspace).with_context(|| {
        format!(
            "failed to resolve workspace {}",
            context.workspace.display()
        )
    })
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();

    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
        }
    }

    normalized
}

fn ensure_inside_workspace(root: &Path, path: &Path) -> Result<()> {
    if path.starts_with(root) {
        Ok(())
    } else {
        bail!(
            "path {} is outside workspace {}",
            path.display(),
            root.display()
        );
    }
}

fn ensure_parent_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create directory {}", parent.display()))?;
    }
    Ok(())
}

fn push_unique_path(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.iter().any(|existing| existing == &path) {
        paths.push(path);
    }
}

fn sanitize_component(value: &str) -> String {
    let sanitized = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();

    if sanitized.is_empty() {
        "default".to_string()
    } else {
        sanitized
    }
}

fn is_safe_record_id(record_id: &str) -> bool {
    !record_id.is_empty()
        && record_id != "."
        && record_id != ".."
        && record_id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
}

fn unix_millis() -> u64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    u64::try_from(millis).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;

    #[test]
    fn write_record_can_delete_created_file() {
        let workspace = temp_workspace("rollback-create");
        let context = context_for(&workspace);
        let manager = RollbackManager::new(context).unwrap();

        let result = manager
            .run_tool(ToolRequest::Write {
                path: PathBuf::from("notes/new.txt"),
                content: "created\n".to_string(),
                overwrite: false,
            })
            .unwrap();

        let record = result.record.expect("write should create rollback record");
        assert_eq!(record.tool_name, "write");
        assert_eq!(record.changed_files[0].kind, FileChangeKind::Created);
        assert!(workspace.join("notes/new.txt").exists());

        let preview = manager.preview(&record.id).unwrap();
        assert_eq!(preview.files[0].action, RestoreAction::DeleteCreatedFile);

        let report = manager.restore(&record.id).unwrap();
        assert_eq!(report.files[0].action, RestoreAction::DeleteCreatedFile);
        assert!(!workspace.join("notes/new.txt").exists());

        remove_workspace(&workspace);
    }

    #[test]
    fn edit_record_can_restore_previous_content() {
        let workspace = temp_workspace("rollback-edit");
        fs::write(workspace.join("file.txt"), "alpha\nbeta\n").unwrap();
        let context = context_for(&workspace);
        let manager = RollbackManager::new(context).unwrap();

        let result = run_tool_with_rollback(
            &manager.context,
            ToolRequest::Edit {
                path: PathBuf::from("file.txt"),
                old: "beta".to_string(),
                new: "gamma".to_string(),
            },
        )
        .unwrap();

        let record = result.record.expect("edit should create rollback record");
        assert_eq!(record.changed_files[0].kind, FileChangeKind::Modified);
        assert!(record.diffs[0].diff.contains("-beta"));
        assert!(record.diffs[0].diff.contains("+gamma"));
        assert_eq!(
            fs::read_to_string(workspace.join("file.txt")).unwrap(),
            "alpha\ngamma\n"
        );

        let preview = manager.preview(&record.id).unwrap();
        assert_eq!(
            preview.files[0].action,
            RestoreAction::RestorePreviousContent
        );
        assert!(preview.files[0].diff.contains("+beta"));

        manager.restore(&record.id).unwrap();
        assert_eq!(
            fs::read_to_string(workspace.join("file.txt")).unwrap(),
            "alpha\nbeta\n"
        );

        remove_workspace(&workspace);
    }

    #[test]
    fn restore_file_only_restores_requested_file() {
        let workspace = temp_workspace("rollback-restore-one");
        fs::write(workspace.join("a.txt"), "old-a").unwrap();
        fs::write(workspace.join("b.txt"), "old-b").unwrap();
        let context = context_for(&workspace);
        let manager = RollbackManager::new(context.clone()).unwrap();

        let first = manager
            .run_tool(ToolRequest::Edit {
                path: PathBuf::from("a.txt"),
                old: "old".to_string(),
                new: "new".to_string(),
            })
            .unwrap()
            .record
            .unwrap();
        manager
            .run_tool(ToolRequest::Edit {
                path: PathBuf::from("b.txt"),
                old: "old".to_string(),
                new: "new".to_string(),
            })
            .unwrap();

        manager.restore_file(&first.id, "a.txt").unwrap();
        assert_eq!(
            fs::read_to_string(workspace.join("a.txt")).unwrap(),
            "old-a"
        );
        assert_eq!(
            fs::read_to_string(workspace.join("b.txt")).unwrap(),
            "new-b"
        );

        let records = RollbackManager::new(context)
            .unwrap()
            .list_records()
            .unwrap();
        assert_eq!(records.len(), 2);

        remove_workspace(&workspace);
    }

    #[test]
    fn read_tool_does_not_create_rollback_record() {
        let workspace = temp_workspace("rollback-read");
        fs::write(workspace.join("file.txt"), "hello").unwrap();
        let context = context_for(&workspace);

        let result = run_tool_with_rollback(
            &context,
            ToolRequest::Read {
                path: PathBuf::from("file.txt"),
                max_bytes: None,
            },
        )
        .unwrap();

        assert!(result.record.is_none());
        assert!(RollbackManager::new(context)
            .unwrap()
            .list_records()
            .unwrap()
            .is_empty());

        remove_workspace(&workspace);
    }

    fn context_for(workspace: &Path) -> AgentContext {
        AgentContext {
            session_id: "test-session".to_string(),
            profile: "test".to_string(),
            workspace: workspace.to_path_buf(),
            provider: "local".to_string(),
            model: "stub".to_string(),
            turn_index: 7,
        }
    }

    fn temp_workspace(prefix: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("{prefix}-{}", unix_millis()));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn remove_workspace(path: &Path) {
        if path.exists() {
            fs::remove_dir_all(path).unwrap();
        }
    }
}
