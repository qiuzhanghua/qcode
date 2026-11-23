//! Task and root domain model.
//!
//! A Task is the user-visible top-level object; its Session is 1:1 and shares
//! the same id. Roots are directories mounted into the task with an access
//! mode. Existing workspace sessions migrate to a Task with one read-write
//! `workspace` root derived from `SessionStarted.cwd`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Maximum roots per task. Keeps sandbox profiles and the UI bounded.
pub const MAX_ROOTS: usize = 16;

/// 侧栏「在对话中创建定时任务」预填到输入框的引导文字。生成会话标题时要剥掉
/// 它，否则每条对话创建的会话 title 都以它开头（title 取第一条用户消息）。
/// 取当前语言下的引导文字。
///
/// 从 `const` 改成函数：译文来自运行期 locale。**预填与剥离必须取同一条 key**——
/// 见 [`strip_schedule_prefill`]。
pub fn schedule_draft_prefill() -> String {
    qcode_i18n::tr("core.prefill.schedule_draft").to_string()
}

/// 去掉第一条用户消息里的引导前缀，供会话标题使用（见 [`SCHEDULE_DRAFT_PREFILL`]）。
pub fn strip_schedule_prefill(text: &str) -> String {
    let text = text.trim();
    // 前缀用**当前语言**的引导文字，与预填同一个来源。写死字面量的话，界面语言
    // 一变剥离就静默失效，会话标题会被这句引导语顶满。
    let prefill = schedule_draft_prefill();
    text.strip_prefix(prefill.as_str())
        .map(|rest| rest.trim().to_string())
        .unwrap_or_else(|| text.to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RootAccess {
    ReadOnly,
    ReadWrite,
}

/// The label a migrated session gives its only root. Kept in one place so the
/// legacy path is easy to find and easy to delete later.
pub const LEGACY_ROOT_LABEL: &str = "workspace";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Root {
    /// Stable identity; never derive access or ownership from `path`.
    pub id: String,
    /// Task-local alias used as `@label/path`.
    pub label: String,
    pub path: PathBuf,
    pub access: RootAccess,
    /// At most one root is the shell cwd.
    pub primary: bool,
}

/// Whether a root's directory is usable right now.
///
/// This is derived from the filesystem instead of stored with the task: a
/// directory can disappear between two turns, and persisting the state would
/// create a second truth that goes stale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootState {
    Ready,
    /// The directory is gone (moved, deleted, or on an unmounted drive). The
    /// task needs a relink before file tools can use it again.
    Missing,
}

impl Root {
    /// Check the filesystem for this root's directory.
    pub fn state(&self) -> RootState {
        if self.path.is_dir() {
            RootState::Ready
        } else {
            RootState::Missing
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    /// Also the session id; Task:Session is 1:1.
    pub id: String,
    /// Explicit user title. `None` falls back to the first user message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub roots: Vec<Root>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TaskError {
    #[error("{}", qcode_i18n::tr("core.err.task.empty_id"))]
    EmptyId,
    #[error("{}", qcode_i18n::trp("core.err.task.invalid_label", &[.0.as_str()]))]
    InvalidLabel(String),
    #[error("{}", qcode_i18n::trp("core.err.task.duplicate_label", &[.0.as_str()]))]
    DuplicateLabel(String),
    #[error("{}", qcode_i18n::trp("core.err.task.duplicate_root_id", &[.0.as_str()]))]
    DuplicateRootId(String),
    #[error("{}", qcode_i18n::trp("core.err.task.too_many_roots", &[.0.to_string(), MAX_ROOTS.to_string()]))]
    TooManyRoots(usize),
    #[error("{}", qcode_i18n::tr("core.err.task.multiple_primary"))]
    MultiplePrimary,
    #[error("{}", qcode_i18n::tr("core.err.task.empty_root_path"))]
    EmptyRootPath,
    #[error("{}", qcode_i18n::tr("core.err.task.empty_title"))]
    EmptyTitle,
    #[error("{}", qcode_i18n::trp("core.err.task.title_too_long", &[.0.to_string()]))]
    TitleTooLong(usize),
}

impl Task {
    pub fn new(id: impl Into<String>, roots: Vec<Root>) -> Result<Self, TaskError> {
        let id = id.into();
        if id.trim().is_empty() {
            return Err(TaskError::EmptyId);
        }
        if roots.len() > MAX_ROOTS {
            return Err(TaskError::TooManyRoots(roots.len()));
        }
        let mut labels = std::collections::HashSet::new();
        let mut ids = std::collections::HashSet::new();
        let mut primary = 0;
        for root in &roots {
            if !is_slug(&root.label) {
                return Err(TaskError::InvalidLabel(root.label.clone()));
            }
            if !labels.insert(root.label.clone()) {
                return Err(TaskError::DuplicateLabel(root.label.clone()));
            }
            if root.id.trim().is_empty() || !ids.insert(root.id.clone()) {
                return Err(TaskError::DuplicateRootId(root.id.clone()));
            }
            if root.path.as_os_str().is_empty() {
                return Err(TaskError::EmptyRootPath);
            }
            primary += usize::from(root.primary);
        }
        if primary > 1 {
            return Err(TaskError::MultiplePrimary);
        }
        Ok(Self {
            id,
            title: None,
            roots,
        })
    }

    /// Migration only: a pre-Task session becomes a task whose single root is the
    /// directory it was started in.
    ///
    /// New tasks never get a label from this: they name their roots themselves.
    pub fn from_legacy_session(
        id: impl Into<String>,
        cwd: impl Into<PathBuf>,
    ) -> Result<Self, TaskError> {
        let id = id.into();
        let root = Root {
            id: format!("{id}-{LEGACY_ROOT_LABEL}"),
            label: LEGACY_ROOT_LABEL.to_string(),
            path: cwd.into(),
            access: RootAccess::ReadWrite,
            primary: true,
        };
        Self::new(id, vec![root])
    }

    pub fn session_id(&self) -> &str {
        &self.id
    }

    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// Validate and set an explicit title.
    pub fn rename(&mut self, title: impl Into<String>) -> Result<(), TaskError> {
        let title = title.into();
        let title = title.trim();
        if title.is_empty() {
            return Err(TaskError::EmptyTitle);
        }
        let chars = title.chars().count();
        if chars > 80 {
            return Err(TaskError::TitleTooLong(chars));
        }
        self.title = Some(title.to_string());
        Ok(())
    }

    pub fn primary_root(&self) -> Option<&Root> {
        self.roots.iter().find(|root| root.primary)
    }

    pub fn root(&self, label: &str) -> Option<&Root> {
        self.roots.iter().find(|root| root.label == label)
    }
}

/// A label is `[a-z0-9][a-z0-9-]*`, at most 32 bytes.
pub fn is_slug(label: &str) -> bool {
    let bytes = label.as_bytes();
    if bytes.is_empty() || bytes.len() > 32 {
        return false;
    }
    let first = bytes[0];
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return false;
    }
    bytes[1..]
        .iter()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(label: &str, path: &str, primary: bool) -> Root {
        Root {
            id: format!("{label}-id"),
            label: label.to_string(),
            path: PathBuf::from(path),
            access: RootAccess::ReadWrite,
            primary,
        }
    }

    #[test]
    fn legacy_workspace_becomes_one_primary_root() {
        let task = Task::from_legacy_session("01ABC", "/repo").unwrap();
        assert_eq!(task.id, "01ABC");
        assert_eq!(task.session_id(), "01ABC");
        assert_eq!(task.roots.len(), 1);
        assert_eq!(task.roots[0].label, "workspace");
        assert_eq!(task.roots[0].path, PathBuf::from("/repo"));
        assert_eq!(task.roots[0].access, RootAccess::ReadWrite);
        assert!(task.roots[0].primary);
    }

    #[test]
    fn no_root_task_is_valid() {
        let task = Task::new("01ABC", vec![]).unwrap();
        assert!(task.roots.is_empty());
        assert!(task.primary_root().is_none());
    }

    #[test]
    fn labels_are_slugs_and_unique() {
        assert!(is_slug("code-2"));
        assert!(is_slug("2docs"));
        assert!(!is_slug("Docs"));
        assert!(!is_slug("参考"));
        assert!(!is_slug("code_2"));
        assert!(!is_slug("-code"));
        assert!(!is_slug("a".repeat(33).as_str()));

        let err = Task::new(
            "id",
            vec![root("code", "/a", false), root("code", "/b", false)],
        )
        .unwrap_err();
        assert_eq!(err, TaskError::DuplicateLabel("code".to_string()));
    }

    #[test]
    fn at_most_one_primary_and_sixteen_roots() {
        let err = Task::new("id", vec![root("a", "/a", true), root("b", "/b", true)]).unwrap_err();
        assert_eq!(err, TaskError::MultiplePrimary);

        let roots = (0..17)
            .map(|index| root(&format!("r{index}"), &format!("/r{index}"), false))
            .collect();
        assert!(matches!(
            Task::new("id", roots).unwrap_err(),
            TaskError::TooManyRoots(17)
        ));
    }

    #[test]
    fn a_root_reports_missing_when_its_directory_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let root = Root {
            id: "r1".to_string(),
            label: "docs".to_string(),
            path: dir.path().to_path_buf(),
            access: RootAccess::ReadWrite,
            primary: true,
        };
        assert_eq!(root.state(), RootState::Ready);
        std::fs::remove_dir_all(dir.path()).unwrap();
        assert_eq!(root.state(), RootState::Missing);
    }
}
