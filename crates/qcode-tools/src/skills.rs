//! Agent Skills: on-demand instruction packs.
//!
//! A skill is a directory with a `SKILL.md` file (YAML frontmatter + Markdown).
//! The spec is public: <https://agentskills.io/specification>. Skills are
//! discovered from the user and project config trees, and only their `name` and
//! `description` enter the system prompt. The full instructions and bundled
//! files are loaded by the `skill` tool, so a large skill costs context only
//! when it is actually used.
//!
//! Skills are user-installed instructions, not permissions: `allowed-tools` is
//! deliberately ignored, and every tool a skill asks the model to run still
//! goes through the normal policy and approval path.
//!
//! Discovery roots, in increasing precedence:
//!
//! 1. `~/.agents/skills` and `~/.config/agents/skills` (global, e.g. `npx skills -g`)
//! 2. `~/.config/qcode/skills` (user)
//! 3. `[skills] roots` from the config file, in listed order (config)
//! 4. `<workspace>/.agents/skills` (project, agent ecosystem)
//! 5. `<workspace>/.qcode/skills` (project, qcode-specific)
//!
//! A later skill with the same name wins. Invalid skills are skipped with a
//! warning; one broken skill never prevents qc from starting.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use qcode_core::event::ToolMetadata;
use qcode_core::provider::ToolSpec;
use qcode_core::tool::{ApprovalClass, Tool, ToolContext, ToolError, ToolOutput};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::truncate;

pub const SKILL_FILE: &str = "SKILL.md";
/// SKILL.md is loaded whole when a skill is activated. 64KiB accommodates real
/// multi-language skills while still bounding a single tool result; detailed
/// material belongs in bundled files.
pub const MAX_SKILL_MD_BYTES: u64 = 64 * 1024;
pub const MAX_DESCRIPTION_CHARS: usize = 1024;
pub const MAX_SKILL_NAME_CHARS: usize = 64;
/// The index is always in the system prompt; capped so a large collection
/// (e.g. 45 globally installed skills) still fits without crowding the turn.
pub const MAX_PROMPT_BYTES: usize = 16 * 1024;
pub const MAX_LISTED_FILES: usize = 200;
/// Largest bundled file the skill tool will read before truncating output.
pub const MAX_REFERENCE_BYTES: u64 = 256 * 1024;
const OUTPUT_LIMIT: usize = 32 * 1024;
const FILES_IN_OUTPUT: usize = 50;
const MAX_SCAN_DEPTH: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SkillSource {
    Global,
    User,
    Config,
    Project,
}

impl SkillSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::User => "user",
            Self::Config => "config",
            Self::Project => "project",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub source: SkillSource,
    /// Canonical skill directory; bundled-file reads are confined to it.
    pub dir: PathBuf,
    pub body: String,
    pub files: Vec<String>,
    pub files_truncated: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum SkillError {
    #[error("{path}: cannot read: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path}: a skill directory must not be a symlink")]
    Symlink { path: PathBuf },
    #[error("{path}: not a directory")]
    NotADirectory { path: PathBuf },
    #[error("{path}: missing {SKILL_FILE}")]
    MissingSkillMd { path: PathBuf },
    #[error("{path}: {SKILL_FILE} must be a regular file")]
    NotAFile { path: PathBuf },
    #[error("{path}: {SKILL_FILE} is {bytes} bytes; the limit is {max} bytes")]
    TooLarge { path: PathBuf, bytes: u64, max: u64 },
    #[error("{path}: {SKILL_FILE} must start with a `---` frontmatter block")]
    MissingFrontmatter { path: PathBuf },
    #[error("{path}: frontmatter is not closed with `---`")]
    UnterminatedFrontmatter { path: PathBuf },
    #[error("{path}: frontmatter field `{field}` is missing")]
    MissingField { path: PathBuf, field: &'static str },
    #[error("{path}: invalid skill name '{name}': {reason}")]
    InvalidName {
        path: PathBuf,
        name: String,
        reason: String,
    },
    #[error("{path}: description must be 1-{MAX_DESCRIPTION_CHARS} characters")]
    InvalidDescription { path: PathBuf },
    #[error("{path}: skill name '{name}' does not match the directory name")]
    NameMismatch { path: PathBuf, name: String },
}

impl Skill {
    /// Load one skill directory. `dir` is the directory that contains
    /// `SKILL.md`, not the file itself.
    pub fn load(dir: &Path, source: SkillSource) -> Result<Self, SkillError> {
        let metadata = std::fs::symlink_metadata(dir).map_err(|source| SkillError::Read {
            path: dir.to_path_buf(),
            source,
        })?;
        if metadata.file_type().is_symlink() {
            return Err(SkillError::Symlink {
                path: dir.to_path_buf(),
            });
        }
        if !metadata.is_dir() {
            return Err(SkillError::NotADirectory {
                path: dir.to_path_buf(),
            });
        }

        let skill_md = dir.join(SKILL_FILE);
        let file_metadata = match std::fs::symlink_metadata(&skill_md) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(SkillError::MissingSkillMd {
                    path: dir.to_path_buf(),
                });
            }
            Err(source) => {
                return Err(SkillError::Read {
                    path: skill_md,
                    source,
                });
            }
        };
        if file_metadata.file_type().is_symlink() {
            return Err(SkillError::Symlink { path: skill_md });
        }
        if !file_metadata.is_file() {
            return Err(SkillError::NotAFile { path: skill_md });
        }
        if file_metadata.len() > MAX_SKILL_MD_BYTES {
            return Err(SkillError::TooLarge {
                path: skill_md.clone(),
                bytes: file_metadata.len(),
                max: MAX_SKILL_MD_BYTES,
            });
        }

        let text = std::fs::read_to_string(&skill_md).map_err(|source| SkillError::Read {
            path: skill_md.clone(),
            source,
        })?;
        let (name, description, body) = parse_skill_md(&skill_md, &text)?;

        let directory_name = dir
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        if directory_name != name {
            return Err(SkillError::NameMismatch {
                path: dir.to_path_buf(),
                name,
            });
        }

        let canonical = dir.canonicalize().map_err(|source| SkillError::Read {
            path: dir.to_path_buf(),
            source,
        })?;
        let (files, files_truncated) = list_bundled_files(&canonical);

        Ok(Self {
            name,
            description,
            source,
            dir: canonical,
            body,
            files,
            files_truncated,
        })
    }
}

/// Parse `SKILL.md` into `(name, description, body)`.
///
/// Only a deliberately small YAML subset is understood: top-level `name` and
/// `description` scalars, plus `|` / `>` block scalars. Everything else in the
/// frontmatter (including `allowed-tools` and `metadata`) is ignored.
fn parse_skill_md(path: &Path, text: &str) -> Result<(String, String, String), SkillError> {
    let normalized = text.replace("\r\n", "\n");
    let mut lines = normalized.lines();

    if lines.next().map(str::trim_end) != Some("---") {
        return Err(SkillError::MissingFrontmatter {
            path: path.to_path_buf(),
        });
    }

    let mut frontmatter = Vec::new();
    let mut body = Vec::new();
    let mut closed = false;
    for line in lines {
        if !closed {
            if line.trim_end() == "---" {
                closed = true;
            } else {
                frontmatter.push(line);
            }
        } else {
            body.push(line);
        }
    }
    if !closed {
        return Err(SkillError::UnterminatedFrontmatter {
            path: path.to_path_buf(),
        });
    }

    let fields = parse_fields(&frontmatter)?;
    let name = fields
        .get("name")
        .cloned()
        .ok_or_else(|| SkillError::MissingField {
            path: path.to_path_buf(),
            field: "name",
        })?;
    validate_name(path, &name)?;

    let description =
        fields
            .get("description")
            .cloned()
            .ok_or_else(|| SkillError::MissingField {
                path: path.to_path_buf(),
                field: "description",
            })?;
    let description = description.trim().to_string();
    if description.is_empty() || description.chars().count() > MAX_DESCRIPTION_CHARS {
        return Err(SkillError::InvalidDescription {
            path: path.to_path_buf(),
        });
    }

    Ok((name, description, body.join("\n").trim().to_string()))
}

fn validate_name(path: &Path, name: &str) -> Result<(), SkillError> {
    let reason = if name.is_empty() {
        Some("must not be empty".to_string())
    } else if name.chars().count() > MAX_SKILL_NAME_CHARS {
        Some(format!("must be at most {MAX_SKILL_NAME_CHARS} characters"))
    } else if !name
        .chars()
        .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
    {
        Some("only lowercase letters, digits and hyphens are allowed".to_string())
    } else if name.starts_with('-') || name.ends_with('-') {
        Some("must not start or end with a hyphen".to_string())
    } else if name.contains("--") {
        Some("must not contain consecutive hyphens".to_string())
    } else {
        None
    };

    match reason {
        Some(reason) => Err(SkillError::InvalidName {
            path: path.to_path_buf(),
            name: name.to_string(),
            reason,
        }),
        None => Ok(()),
    }
}

fn parse_fields(lines: &[&str]) -> Result<BTreeMap<String, String>, SkillError> {
    let mut fields = BTreeMap::new();
    let mut index = 0;

    while index < lines.len() {
        let line = lines[index];
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            index += 1;
            continue;
        }
        // Indented lines belong to a nested map (e.g. `metadata:`) or to a
        // block scalar that was consumed above. Neither is used by qcode.
        if trimmed.len() != line.len() {
            index += 1;
            continue;
        }

        let Some((key, rest)) = line.split_once(':') else {
            index += 1;
            continue;
        };
        let key = key.trim().to_string();
        let rest = rest.trim();

        let mut block = None;
        let mut value;
        if rest.starts_with('|') || rest.starts_with('>') {
            block = rest.chars().next();
            value = String::new();
        } else if rest.is_empty() {
            value = String::new();
        } else {
            value = unquote(rest);
            index += 1;
            fields.insert(key, value);
            continue;
        }

        let mut block_lines: Vec<&str> = Vec::new();
        let mut next = index + 1;
        while next < lines.len() {
            let candidate = lines[next];
            if candidate.trim().is_empty() {
                block_lines.push("");
                next += 1;
                continue;
            }
            if candidate.len() != candidate.trim_start().len() {
                block_lines.push(candidate);
                next += 1;
            } else {
                break;
            }
        }

        let indent = block_lines
            .iter()
            .filter(|line| !line.trim().is_empty())
            .map(|line| line.len() - line.trim_start().len())
            .min()
            .unwrap_or(0);
        let dedented: Vec<&str> = block_lines
            .iter()
            .map(|line| {
                if line.len() >= indent {
                    &line[indent..]
                } else {
                    ""
                }
            })
            .collect();

        value = match block {
            Some('|') => dedented.join("\n"),
            Some('>') => fold(&dedented),
            _ => value,
        };
        fields.insert(key, value.trim_end().to_string());
        index = next;
    }

    Ok(fields)
}

fn fold(lines: &[&str]) -> String {
    let mut out = String::new();
    for line in lines {
        if line.is_empty() {
            out.push('\n');
            continue;
        }
        if !out.is_empty() && !out.ends_with('\n') {
            out.push(' ');
        }
        out.push_str(line);
    }
    out.trim().to_string()
}

fn unquote(value: &str) -> String {
    let value = value.trim();
    if let Some(inner) = value
        .strip_prefix('"')
        .and_then(|text| text.strip_suffix('"'))
    {
        return inner.replace("\\\"", "\"").replace("\\\\", "\\");
    }
    if let Some(inner) = value
        .strip_prefix('\'')
        .and_then(|text| text.strip_suffix('\''))
    {
        return inner.replace("''", "'");
    }
    // A YAML comment starts at ` #` outside quotes.
    value
        .split_once(" #")
        .map(|(before, _)| before.trim().to_string())
        .unwrap_or_else(|| value.to_string())
}

/// Recursively list regular files below the skill root. Symlinks are skipped:
/// the tool may only read files that really live inside the skill directory.
fn list_bundled_files(root: &Path) -> (Vec<String>, bool) {
    let mut files = Vec::new();
    let mut truncated = false;
    let mut stack = vec![root.to_path_buf()];

    while let Some(directory) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(|entry| entry.file_name());

        for entry in entries {
            let path = entry.path();
            if path == root.join(SKILL_FILE) {
                continue;
            }
            let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if metadata.file_type().is_symlink() {
                continue;
            }
            if metadata.is_dir() {
                let depth = path.components().count() - root.components().count();
                if depth < MAX_SCAN_DEPTH {
                    stack.push(path);
                }
            } else if metadata.is_file() {
                if files.len() >= MAX_LISTED_FILES {
                    truncated = true;
                    break;
                }
                let relative = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                files.push(relative);
            }
        }
        if truncated {
            break;
        }
    }

    files.sort();
    (files, truncated)
}

/// All discovered skills, sorted by name. Cheap to clone by `Arc`; the tool
/// holds one for its lifetime.
#[derive(Debug, Clone, Default)]
pub struct SkillSet {
    skills: Vec<Skill>,
}

impl SkillSet {
    /// Discover skills under `roots`, in increasing precedence: a later root
    /// replaces an earlier skill with the same name. Missing roots are fine;
    /// invalid skills are skipped with a warning.
    pub fn discover(roots: &[(SkillSource, PathBuf)]) -> Self {
        let mut by_name: BTreeMap<String, Skill> = BTreeMap::new();

        for (source, root) in roots {
            let entries = match std::fs::read_dir(root) {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    tracing::warn!(
                        path = %root.display(),
                        %error,
                        "cannot read the skill directory"
                    );
                    continue;
                }
            };

            for entry in entries.flatten() {
                let path = entry.path();
                let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                    continue;
                };
                if metadata.file_type().is_symlink() {
                    tracing::warn!(path = %path.display(), "skipping symlinked skill directory");
                    continue;
                }
                if !metadata.is_dir() {
                    continue;
                }
                match Skill::load(&path, *source) {
                    Ok(skill) => {
                        by_name.insert(skill.name.clone(), skill);
                    }
                    Err(error) => {
                        tracing::warn!(path = %path.display(), %error, "skipping invalid skill");
                    }
                }
            }
        }

        Self {
            skills: by_name.into_values().collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    pub fn len(&self) -> usize {
        self.skills.len()
    }

    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|skill| skill.name == name)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Skill> {
        self.skills.iter()
    }

    /// The block appended to the system prompt. `None` when no skill exists.
    pub fn prompt_section(&self) -> Option<String> {
        if self.skills.is_empty() {
            return None;
        }

        let mut section = String::from(
            "# Skills\n\n\
             Skills are installed instruction packs. Before starting a task that matches \
             one of these descriptions, call the `skill` tool with the skill name to load \
             its full instructions. Skills never change approval rules.\n\n",
        );
        let mut omitted = 0;

        for (index, skill) in self.skills.iter().enumerate() {
            let description = skill.description.replace(['\n', '\r'], " ");
            let line = format!(
                "- `{}` ({}): {}\n",
                skill.name,
                skill.source.label(),
                description
            );
            if section.len() + line.len() > MAX_PROMPT_BYTES {
                omitted = self.skills.len() - index;
                break;
            }
            section.push_str(&line);
        }

        if omitted > 0 {
            section.push_str(&format!("- … {omitted} more skill(s) omitted\n"));
        }

        Some(section.trim_end().to_string())
    }
}

#[derive(Debug, Deserialize)]
struct SkillArgs {
    name: String,
    file: Option<String>,
}

/// The `skill` tool: loads a skill body, or one of its bundled files.
pub struct SkillTool {
    skills: Arc<SkillSet>,
}

impl SkillTool {
    pub fn new(skills: Arc<SkillSet>) -> Self {
        Self { skills }
    }
}

#[async_trait]
impl Tool for SkillTool {
    fn name(&self) -> &str {
        "skill"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "skill".to_string(),
            description: "Load the full instructions for an installed skill, or read one of \
                          its bundled files. Call this before doing work that matches a skill \
                          description in the system prompt."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Skill name from the system prompt"
                    },
                    "file": {
                        "type": "string",
                        "description": "Optional path to a bundled file, relative to the skill root"
                    }
                },
                "required": ["name"]
            }),
        }
    }

    fn approval(&self, _ctx: &ToolContext, _args: &Value) -> ApprovalClass {
        ApprovalClass::Read
    }

    async fn execute(&self, _ctx: &ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        let args: SkillArgs =
            serde_json::from_value(args).map_err(|error| ToolError::Invalid(error.to_string()))?;

        let Some(skill) = self.skills.get(&args.name) else {
            let available: Vec<&str> = self
                .skills
                .skills
                .iter()
                .take(20)
                .map(|skill| skill.name.as_str())
                .collect();
            return Err(ToolError::Invalid(format!(
                "unknown skill '{}'; available: {}",
                args.name,
                available.join(", ")
            )));
        };

        match args.file {
            Some(file) => read_bundled_file(skill, &file),
            None => Ok(render_skill(skill)),
        }
    }
}

fn render_skill(skill: &Skill) -> ToolOutput {
    let mut output = format!(
        "# Skill: {}\n\nSource: {} ({})\n\n{}\n",
        skill.name,
        skill.source.label(),
        skill.dir.display(),
        skill.body.trim()
    );

    if !skill.files.is_empty() {
        output.push_str("\n## Bundled files\n\n");
        for file in skill.files.iter().take(FILES_IN_OUTPUT) {
            output.push_str("- ");
            output.push_str(file);
            output.push('\n');
        }
        if skill.files.len() > FILES_IN_OUTPUT || skill.files_truncated {
            output.push_str("- … more files omitted\n");
        }
        output.push_str(&format!(
            "\nRead one with skill(name=\"{}\", file=\"<path>\").\n",
            skill.name
        ));
    }

    let bytes = output.len() as u64;
    let (output, truncated) = truncate(&output, OUTPUT_LIMIT);
    ToolOutput::ok(output).with_metadata(ToolMetadata {
        truncated,
        bytes,
        ..ToolMetadata::default()
    })
}

fn read_bundled_file(skill: &Skill, requested: &str) -> Result<ToolOutput, ToolError> {
    if requested.trim().is_empty() {
        return Err(ToolError::Invalid("file is empty".to_string()));
    }
    let raw = Path::new(requested);
    if raw.is_absolute() {
        return Err(ToolError::Invalid(
            "file must be relative to the skill root".to_string(),
        ));
    }

    let joined = skill.dir.join(raw);
    let canonical = joined.canonicalize().map_err(|_| {
        ToolError::Invalid(format!(
            "unknown file '{requested}' in skill '{}'",
            skill.name
        ))
    })?;
    if !canonical.starts_with(&skill.dir) {
        return Err(ToolError::Invalid(
            "file escapes the skill directory".to_string(),
        ));
    }

    let metadata = std::fs::symlink_metadata(&joined)?;
    if metadata.file_type().is_symlink() {
        return Err(ToolError::Invalid(
            "bundled files must not be symlinks".to_string(),
        ));
    }
    if !metadata.is_file() {
        return Err(ToolError::Invalid(format!(
            "'{requested}' is not a regular file"
        )));
    }
    if metadata.len() > MAX_REFERENCE_BYTES {
        return Err(ToolError::Invalid(format!(
            "'{requested}' is {} bytes; the limit is {MAX_REFERENCE_BYTES}",
            metadata.len()
        )));
    }

    let bytes = std::fs::read(&canonical)?;
    if bytes.iter().take(8192).any(|byte| *byte == 0) {
        return Err(ToolError::Invalid(format!(
            "'{requested}' looks binary; refusing to load it as text"
        )));
    }
    let text = String::from_utf8(bytes)
        .map_err(|_| ToolError::Invalid(format!("'{requested}' is not valid UTF-8")))?;

    let body = format!("[skill:{}] {requested}\n\n{text}", skill.name);
    let bytes = body.len() as u64;
    let (output, truncated) = truncate(&body, OUTPUT_LIMIT);
    Ok(ToolOutput::ok(output).with_metadata(ToolMetadata {
        truncated,
        bytes,
        ..ToolMetadata::default()
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(root: &Path, directory: &str, frontmatter: &str, body: &str) -> PathBuf {
        let dir = root.join(directory);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(SKILL_FILE),
            format!("---\n{frontmatter}\n---\n\n{body}\n"),
        )
        .unwrap();
        dir
    }

    fn set(root: &Path) -> SkillSet {
        SkillSet::discover(&[(SkillSource::User, root.to_path_buf())])
    }

    fn tool(set: SkillSet) -> SkillTool {
        SkillTool::new(Arc::new(set))
    }

    #[tokio::test]
    async fn loads_a_minimal_skill() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = write_skill(
            tmp.path(),
            "pdf-processing",
            "name: pdf-processing\ndescription: Extract PDF text and fill forms. Use for PDFs.",
            "Do the thing.",
        );

        let skill = Skill::load(&dir, SkillSource::Project).unwrap();
        assert_eq!(skill.name, "pdf-processing");
        assert!(skill.description.starts_with("Extract PDF"));
        assert_eq!(skill.body, "Do the thing.");
        assert_eq!(skill.source, SkillSource::Project);
        assert!(skill.files.is_empty());
        assert!(!skill.files_truncated);
    }

    #[test]
    fn parses_quoted_and_block_descriptions() {
        let quoted = tempfile::tempdir().unwrap();
        let quoted_dir = write_skill(
            quoted.path(),
            "quoted",
            "name: quoted\ndescription: \"Use when: docs have colons\"",
            "body",
        );
        let skill = Skill::load(&quoted_dir, SkillSource::User).unwrap();
        assert_eq!(skill.description, "Use when: docs have colons");

        let folded = tempfile::tempdir().unwrap();
        let folded_dir = write_skill(
            folded.path(),
            "folded",
            "name: folded\ndescription: >-\n  Line one\n  line two",
            "body",
        );
        let skill = Skill::load(&folded_dir, SkillSource::User).unwrap();
        assert_eq!(skill.description, "Line one line two");

        let literal = tempfile::tempdir().unwrap();
        let literal_dir = write_skill(
            literal.path(),
            "literal",
            "name: literal\ndescription: |\n  one\n  two",
            "body",
        );
        let skill = Skill::load(&literal_dir, SkillSource::User).unwrap();
        assert_eq!(skill.description, "one\ntwo");
    }

    #[test]
    fn ignores_unknown_and_nested_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = write_skill(
            tmp.path(),
            "with-meta",
            "name: with-meta\ndescription: uses metadata\nmetadata:\n  author: someone\nallowed-tools: Bash(git:*)",
            "body",
        );
        let skill = Skill::load(&dir, SkillSource::User).unwrap();
        assert_eq!(skill.name, "with-meta");
        assert_eq!(skill.description, "uses metadata");
    }

    #[test]
    fn rejects_bad_names_and_mismatches() {
        let upper = tempfile::tempdir().unwrap();
        let upper_dir = write_skill(
            upper.path(),
            "Upper",
            "name: Upper\ndescription: bad name",
            "body",
        );
        assert!(matches!(
            Skill::load(&upper_dir, SkillSource::User),
            Err(SkillError::InvalidName { .. })
        ));

        let double = tempfile::tempdir().unwrap();
        let double_dir = write_skill(
            double.path(),
            "a--b",
            "name: a--b\ndescription: bad name",
            "body",
        );
        assert!(matches!(
            Skill::load(&double_dir, SkillSource::User),
            Err(SkillError::InvalidName { .. })
        ));

        let mismatch = tempfile::tempdir().unwrap();
        let mismatch_dir = write_skill(
            mismatch.path(),
            "directory-name",
            "name: other-name\ndescription: mismatch",
            "body",
        );
        assert!(matches!(
            Skill::load(&mismatch_dir, SkillSource::User),
            Err(SkillError::NameMismatch { .. })
        ));
    }

    #[test]
    fn rejects_missing_fields_and_frontmatter() {
        let no_description = tempfile::tempdir().unwrap();
        let dir = no_description.path().join("no-description");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(SKILL_FILE), "---\nname: no-description\n---\nbody").unwrap();
        assert!(matches!(
            Skill::load(&dir, SkillSource::User),
            Err(SkillError::MissingField {
                field: "description",
                ..
            })
        ));

        let no_frontmatter = tempfile::tempdir().unwrap();
        let dir = no_frontmatter.path().join("bare");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(SKILL_FILE), "just markdown").unwrap();
        assert!(matches!(
            Skill::load(&dir, SkillSource::User),
            Err(SkillError::MissingFrontmatter { .. })
        ));

        let unterminated = tempfile::tempdir().unwrap();
        let dir = unterminated.path().join("open");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(SKILL_FILE), "---\nname: open\n").unwrap();
        assert!(matches!(
            Skill::load(&dir, SkillSource::User),
            Err(SkillError::UnterminatedFrontmatter { .. })
        ));
    }

    #[test]
    fn rejects_oversized_skill_md() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("big");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(SKILL_FILE),
            format!(
                "---\nname: big\ndescription: big\n---\n{}",
                "x".repeat(MAX_SKILL_MD_BYTES as usize)
            ),
        )
        .unwrap();
        assert!(matches!(
            Skill::load(&dir, SkillSource::User),
            Err(SkillError::TooLarge { .. })
        ));
    }

    #[test]
    fn discovers_project_over_user() {
        let user = tempfile::tempdir().unwrap();
        write_skill(
            user.path(),
            "shared",
            "name: shared\ndescription: user copy",
            "user body",
        );
        let project = tempfile::tempdir().unwrap();
        write_skill(
            project.path(),
            "shared",
            "name: shared\ndescription: project copy",
            "project body",
        );
        write_skill(
            project.path(),
            "only-project",
            "name: only-project\ndescription: project only",
            "body",
        );

        let skills = SkillSet::discover(&[
            (SkillSource::User, user.path().to_path_buf()),
            (SkillSource::Project, project.path().to_path_buf()),
        ]);
        assert_eq!(skills.len(), 2);
        let shared = skills.get("shared").unwrap();
        assert_eq!(shared.source, SkillSource::Project);
        assert_eq!(shared.body, "project body");
    }

    #[test]
    fn invalid_skills_are_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "good", "name: good\ndescription: works", "body");
        let bad = tmp.path().join("bad");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(bad.join(SKILL_FILE), "no frontmatter").unwrap();

        let skills = set(tmp.path());
        assert_eq!(skills.len(), 1);
        assert!(skills.get("good").is_some());
    }

    #[test]
    fn prompt_section_lists_skills_and_caps_itself() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(
            tmp.path(),
            "alpha",
            "name: alpha\ndescription: first skill",
            "body",
        );
        let section = set(tmp.path()).prompt_section().unwrap();
        assert!(section.contains("`alpha` (user): first skill"));
        assert!(section.contains("`skill` tool"));

        let many = tempfile::tempdir().unwrap();
        for index in 0..20 {
            let name = format!("skill-{index}");
            let description = "d".repeat(1000);
            write_skill(
                many.path(),
                &name,
                &format!("name: {name}\ndescription: {description}"),
                "body",
            );
        }
        let section = set(many.path()).prompt_section().unwrap();
        assert!(
            section.len() <= MAX_PROMPT_BYTES + 64,
            "section is {}",
            section.len()
        );
        assert!(section.contains("more skill(s) omitted"));
    }

    #[tokio::test]
    async fn skill_tool_renders_body_and_bundled_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = write_skill(
            tmp.path(),
            "docs",
            "name: docs\ndescription: write docs",
            "Read the reference first.",
        );
        std::fs::create_dir_all(dir.join("references")).unwrap();
        std::fs::write(dir.join("references/REFERENCE.md"), "reference body").unwrap();
        std::fs::create_dir_all(dir.join("scripts")).unwrap();
        std::fs::write(dir.join("scripts/run.sh"), "echo hi").unwrap();

        let tool = tool(set(tmp.path()));
        let output = tool
            .execute(&ToolContext::new(tmp.path()), json!({"name": "docs"}))
            .await
            .unwrap();
        assert_eq!(output.status, qcode_core::event::ToolStatus::Ok);
        assert!(output.output.starts_with("# Skill: docs"));
        assert!(output.output.contains("Read the reference first."));
        assert!(output.output.contains("references/REFERENCE.md"));
        assert!(output.output.contains("scripts/run.sh"));
    }

    #[tokio::test]
    async fn skill_tool_reads_bundled_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = write_skill(
            tmp.path(),
            "docs",
            "name: docs\ndescription: write docs",
            "body",
        );
        std::fs::create_dir_all(dir.join("references")).unwrap();
        std::fs::write(dir.join("references/REFERENCE.md"), "reference body").unwrap();

        let tool = tool(set(tmp.path()));
        let output = tool
            .execute(
                &ToolContext::new(tmp.path()),
                json!({"name": "docs", "file": "references/REFERENCE.md"}),
            )
            .await
            .unwrap();
        assert!(output.output.contains("reference body"));
    }

    #[tokio::test]
    async fn skill_tool_rejects_escapes_and_binary_files() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = write_skill(
            tmp.path(),
            "docs",
            "name: docs\ndescription: write docs",
            "body",
        );
        std::fs::write(tmp.path().join("secret.txt"), "secret").unwrap();
        std::fs::write(skill_dir.join("bin.dat"), [0u8, 1, 2]).unwrap();

        let tool = tool(set(tmp.path()));
        let context = ToolContext::new(tmp.path());

        let escaped = tool
            .execute(&context, json!({"name": "docs", "file": "../secret.txt"}))
            .await;
        assert!(escaped.is_err());

        let absolute = tool
            .execute(
                &context,
                json!({"name": "docs", "file": tmp.path().join("secret.txt")}),
            )
            .await;
        assert!(absolute.is_err());

        let binary = tool
            .execute(&context, json!({"name": "docs", "file": "bin.dat"}))
            .await;
        assert!(binary.is_err());
    }

    #[tokio::test]
    async fn skill_tool_reports_unknown_skills() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(
            tmp.path(),
            "known",
            "name: known\ndescription: known skill",
            "body",
        );
        let tool = tool(set(tmp.path()));
        let error = tool
            .execute(&ToolContext::new(tmp.path()), json!({"name": "nope"}))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("unknown skill 'nope'"));
        assert!(error.to_string().contains("known"));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_skill_directories_and_skill_files() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let real = write_skill(
            outside.path(),
            "outside",
            "name: outside\ndescription: outside",
            "body",
        );
        let link = tmp.path().join("linked");
        symlink(&real, &link).unwrap();
        assert!(matches!(
            Skill::load(&link, SkillSource::User),
            Err(SkillError::Symlink { .. })
        ));
        assert!(set(tmp.path()).is_empty());

        let dir = write_skill(tmp.path(), "docs", "name: docs\ndescription: docs", "body");
        let skill_md = dir.join(SKILL_FILE);
        let real_md = dir.join("real.md");
        std::fs::rename(&skill_md, &real_md).unwrap();
        symlink(&real_md, &skill_md).unwrap();
        assert!(matches!(
            Skill::load(&dir, SkillSource::User),
            Err(SkillError::Symlink { .. })
        ));
    }

    #[test]
    fn bundled_file_list_is_capped() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = write_skill(tmp.path(), "many", "name: many\ndescription: many", "body");
        for index in 0..MAX_LISTED_FILES + 5 {
            std::fs::write(dir.join(format!("f{index:03}.txt")), "x").unwrap();
        }

        let skill = Skill::load(&dir, SkillSource::User).unwrap();
        assert_eq!(skill.files.len(), MAX_LISTED_FILES);
        assert!(skill.files_truncated);
    }

    #[test]
    fn empty_set_has_no_prompt_section() {
        assert!(SkillSet::default().prompt_section().is_none());
    }
}
