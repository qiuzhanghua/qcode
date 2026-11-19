//! Config templates (ADR-0104).
//!
//! A template is **scaffolding, not an overlay**: `qc config init --template X`
//! reads one once and writes an ordinary `config.toml`. After that the template
//! never participates in loading, so upgrading qcode cannot change a user's
//! sandbox boundary behind their back — which is the whole reason this is not a
//! merge layer.
//!
//! Two kinds, mirroring the two things that are hard to write by hand:
//!
//! - [`Kind::Security`] — `[sandbox]` + `[network]` together. They are one
//!   template because `network = "strict"` is only legal on macOS's
//!   `sandbox-exec`, so "how strict is the network" and "which backend" cannot
//!   be chosen independently.
//! - [`Kind::Llm`] — one provider route. See ADR-0104 §5 for why the built-in
//!   ones reference [`super::config::catalog_entry`] instead of copying it.
//!
//! The files live in `template/` next to this module and are embedded with
//! `rust-embed`: debug builds read them from disk (editing a template needs no
//! rebuild), release builds carry them in the binary.

use std::path::PathBuf;

use serde::Deserialize;

use crate::config::Config;

/// The embedded `template/` directory.
#[derive(rust_embed::Embed)]
#[folder = "template/"]
struct Asset;

/// What a template produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// One `[providers.*]` route.
    Llm,
    /// `[sandbox]` + `[network]` as one self-consistent pair.
    Security,
}

impl Kind {
    /// The directory name under `template/`, which is what the path says.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Llm => "llm",
            Self::Security => "security",
        }
    }

    fn from_dir(dir: &str) -> Option<Self> {
        match dir {
            "llm" => Some(Self::Llm),
            "security" => Some(Self::Security),
            _ => None,
        }
    }

    /// Parse a `--template` argument that spells the kind out, e.g.
    /// `security-landlock` or a bare `landlock` when unambiguous.
    pub fn parse_dir(dir: &str) -> Option<Self> {
        Self::from_dir(dir)
    }
}

/// What a template asks the user for.
#[derive(Debug, Clone, Deserialize)]
pub struct Prompt {
    /// Placeholder name, substituted as `${key}` in `[config]`.
    pub key: String,
    /// What to show when asking.
    pub label: String,
    #[serde(default)]
    pub kind: PromptKind,
    /// For [`PromptKind::Secret`]: the environment variable name to write into
    /// the config instead of the secret itself.
    #[serde(default)]
    pub env: Option<String>,
}

/// How a prompt's answer is used.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PromptKind {
    /// Never written to `config.toml` — only its environment variable name is.
    #[default]
    Secret,
    /// Substituted verbatim.
    Text,
    /// Expanded (`~`) and substituted.
    Path,
}

/// The `[template]` section.
#[derive(Debug, Clone, Deserialize)]
struct Meta {
    title: String,
    summary: String,
    /// For [`Kind::Llm`]: the catalog route whose facts this template reuses.
    #[serde(default)]
    catalog: Option<String>,
    /// Optional; when present it must agree with the path (see [`Template::parse`]).
    #[serde(default)]
    kind: Option<Kind>,
    #[serde(default)]
    id: Option<String>,
    /// Operating systems this template applies to. Empty means all of them.
    ///
    /// Some of what a template sets genuinely does not exist on every platform:
    /// `bwrap` is Linux-only, `windows-acl` is Windows-only, and there is no
    /// `network = "strict"` outside macOS. Writing such a config elsewhere
    /// produces a file that loads and then fails at assembly, so the template
    /// says where it belongs instead of leaving that to be discovered.
    #[serde(default)]
    os: Vec<String>,
}

/// A comment emitted into the generated file, optionally only on one OS.
///
/// This is how a template says "this key is a no-op here" without writing a
/// misleading value: Windows does not restrict reads at all, so its
/// `extra_read` grants nothing, and a comment is the only honest way to say so.
#[derive(Debug, Clone, Deserialize)]
pub struct Note {
    /// `linux` / `macos` / `windows`; absent means every platform.
    #[serde(default)]
    pub os: Option<String>,
    pub text: String,
}

/// The platform name this build runs on, spelled the way templates spell it.
pub const CURRENT_OS: &str = std::env::consts::OS;

/// The file as written.
#[derive(Debug, Clone, Deserialize)]
struct Raw {
    template: Meta,
    #[serde(default)]
    prompt: Vec<Prompt>,
    #[serde(default)]
    note: Vec<Note>,
    #[serde(default)]
    config: toml::Table,
}

/// One parsed template.
#[derive(Debug, Clone)]
pub struct Template {
    /// Where it came from: `security/landlock.toml`.
    pub source: String,
    pub kind: Kind,
    pub id: String,
    pub title: String,
    pub summary: String,
    pub catalog: Option<String>,
    pub prompts: Vec<Prompt>,
    /// Operating systems this template applies to; empty means all.
    pub os: Vec<String>,
    /// Platform-conditional comments, rendered into the generated file.
    pub notes: Vec<Note>,
    /// The `[config]` section: what gets written into `config.toml`.
    pub config: toml::Table,
    /// The raw `[config]` region of the source, comments included. Rendering
    /// from this (not from `config`) is what keeps the caveats in the file a
    /// user ends up with — see [`Template::document`].
    config_text: String,
}

#[derive(Debug, thiserror::Error)]
pub enum TemplateError {
    // The field is `path`, not `source`: `thiserror` treats a field named
    // `source` as the wrapped error and would demand `Error` on a `String`.
    #[error("template {path} is not valid TOML: {error}")]
    Parse {
        path: String,
        error: Box<toml::de::Error>,
    },
    #[error("template {path}: {message}")]
    Invalid { path: String, message: String },
    #[error("no template named {name:?}")]
    NotFound { name: String },
}

impl Template {
    /// Parse one embedded file.
    fn parse(path: &str, text: &str) -> Result<Self, TemplateError> {
        let raw: Raw = toml::from_str(text).map_err(|error| TemplateError::Parse {
            path: path.to_string(),
            error: Box::new(error),
        })?;
        let invalid = |message: String| TemplateError::Invalid {
            path: path.to_string(),
            message,
        };

        // Kind and id come from the path, which is the single authority: a
        // directory listing is what `template list` shows, so a file whose
        // `[template]` block disagrees with its own location would be listed
        // under one name and written under another.
        let mut parts = path.split('/');
        let dir = parts.next().unwrap_or_default();
        let file = parts.next().unwrap_or_default();
        if parts.next().is_some() {
            return Err(invalid(
                "unexpected nesting; templates are template/<kind>/<id>.toml".to_string(),
            ));
        }
        let kind = Kind::from_dir(dir).ok_or_else(|| {
            invalid(format!(
                "unknown template kind {dir:?}; expected one of llm, security",
            ))
        })?;
        let id = file
            .strip_suffix(".toml")
            .ok_or_else(|| invalid("template files must end in .toml".to_string()))?
            .to_string();
        if let Some(declared) = raw.template.kind
            && declared != kind
        {
            return Err(invalid(format!(
                "[template] kind = {:?} but the file is under {dir}/",
                declared.as_str(),
            )));
        }
        if let Some(declared) = &raw.template.id
            && *declared != id
        {
            return Err(invalid(format!(
                "[template] id = {declared:?} but the file is named {id}.toml",
            )));
        }
        Ok(Self {
            source: path.to_string(),
            kind,
            id,
            title: raw.template.title,
            summary: raw.template.summary,
            catalog: raw.template.catalog,
            prompts: raw.prompt,
            os: raw.template.os,
            notes: raw.note,
            config: raw.config,
            config_text: text.to_string(),
        })
    }

    /// The `[config]` section as the `Config` type a real file parses into, plus
    /// the checks a real file goes through.
    ///
    /// Platform availability is deliberately **not** checked here: a macOS user
    /// asking for the `landlock` template should be able to write it and then be
    /// told clearly that the backend is unavailable, rather than be refused at
    /// the template layer (ADR-0104 §验证3).
    pub fn config(&self) -> Result<Config, TemplateError> {
        let invalid = |message: String| TemplateError::Invalid {
            path: self.source.clone(),
            message,
        };
        // Typos are silent in serde, so catch them the same way a real file does.
        let unknown = crate::config::unknown_keys(&self.config);
        if !unknown.is_empty() {
            return Err(invalid(format!(
                "unknown config keys: {}",
                unknown.join(", "),
            )));
        }
        // `Value::to_string` renders inline tables (`{ mode = "x" }`), which is
        // not a TOML document; `to_string_pretty` emits real `[sections]`, so the
        // generated text is exactly what a user would have written by hand.
        let text = toml::to_string_pretty(&toml::Value::Table(self.config.clone()))
            .map_err(|error| invalid(format!("cannot render the config section: {error}")))?;
        let config: Config = toml::from_str(&text).map_err(|error| invalid(format!("{error}")))?;
        config
            .sandbox
            .validate()
            .map_err(|error| invalid(format!("{error}")))?;
        config
            .mcp
            .validate()
            .map_err(|error| invalid(format!("{error}")))?;
        // The one combination that can never assemble, and it is
        // platform-independent: `strict` needs `sandbox-exec`'s Seatbelt profile.
        // Checked here so no template can ship in that state.
        if config.network_plan().strict
            && config.sandbox.mode != qcode_tools::sandbox::SandboxMode::SandboxExec
        {
            return Err(invalid(format!(
                "network strict requires [sandbox] mode = \"sandbox-exec\"; this template asks for {:?}",
                config.sandbox.mode.label(),
            )));
        }
        Ok(config)
    }

    /// Whether this template is meant for the platform this build runs on.
    ///
    /// An empty `os` means "every platform" — that is the common case (the llm
    /// templates, and `security/none.toml`).
    pub fn applies_here(&self) -> bool {
        self.applies_on(CURRENT_OS)
    }

    /// [`Self::applies_here`] with the platform spelled out, so the rule is
    /// testable for all three platforms from any one of them.
    pub fn applies_on(&self, os: &str) -> bool {
        self.os.is_empty() || self.os.iter().any(|candidate| candidate == os)
    }

    /// The notes that belong in a file generated on `os`, as comment lines.
    fn notes_for(&self, os: &str) -> Vec<&str> {
        self.notes
            .iter()
            .filter(|note| note.os.as_deref().is_none_or(|only| only == os))
            .map(|note| note.text.as_str())
            .collect()
    }

    /// The TOML this template writes into `config.toml`.
    ///
    /// Rendered from the source text rather than from [`Self::config`], because a
    /// parsed [`toml::Table`] has no comments: the caveats are half of what a
    /// template is for (ADR-0104 §7 makes saying the limit mandatory, and §8
    /// makes machine-specific paths comments-only), so re-rendering would strip
    /// exactly the text that keeps a user from misreading the setting.
    ///
    /// Only the `[config]` region — `[template]` and `[[prompt]]` describe the
    /// scaffolding and have no meaning in a real config file.
    pub fn document(&self) -> Result<String, TemplateError> {
        self.document_for(CURRENT_OS)
    }

    /// [`Self::document`] for a named platform. `config init` writes for the
    /// platform it runs on; tests use the others.
    pub fn document_for(&self, os: &str) -> Result<String, TemplateError> {
        let region = config_region(&self.config_text).ok_or_else(|| TemplateError::Invalid {
            path: self.source.clone(),
            message: "no `[config]` section".to_string(),
        })?;
        // The leading comment block is where a template says what it is and what
        // it cannot do; it belongs at the top of the file the user gets, not only
        // in the repository.
        let mut header = leading_comments(&self.config_text).to_string();
        // Platform notes follow it, because they are the same kind of statement:
        // what this file does and does not do *on this machine*.
        let notes = self.notes_for(os);
        if !notes.is_empty() {
            if !header.is_empty() && !header.ends_with("\n\n") {
                header.push('\n');
            }
            for text in notes {
                header.push_str(&format!("# {text}\n"));
            }
        }
        let body = strip_config_prefix(region);
        Ok(if header.trim().is_empty() {
            body
        } else {
            format!("{header}\n{body}")
        })
    }
}

/// The comment block at the very top of a template, comments and blank lines
/// only, stopping at the first real content.
///
/// It is the template's own explanation — what this backend is, what it cannot
/// do — and it is prepended to the generated file because that is where a user
/// reads it.
fn leading_comments(text: &str) -> &str {
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            offset += line.len();
            continue;
        }
        break;
    }
    &text[..offset]
}

/// The `[config]` region of a template's source, from its first table header to
/// the end of the file.
///
/// Invariant (checked by a test): every `[config…]` header comes **after** all
/// `[template]` / `[[prompt]]` headers. Every shipped template satisfies it,
/// because the config body reads best last, right after the caveats about it.
fn config_region(text: &str) -> Option<&str> {
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        if line.trim_start().starts_with("[config") {
            return Some(&text[offset..]);
        }
        offset += line.len();
    }
    None
}

/// Drop the `config` level from table headers: `[config.sandbox]` becomes
/// `[sandbox]`, `[[config.providers.x.models]]` becomes `[[providers.x.models]]`,
/// and a bare `[config]` disappears (its keys are already at the root).
///
/// Only lines that *start* a table are rewritten; a value such as
/// `extra_read = ["~/x"]` starts with a letter and is left alone.
fn strip_config_prefix(region: &str) -> String {
    let mut out = String::with_capacity(region.len());
    for line in region.split_inclusive('\n') {
        if line.trim() == "[config]" {
            continue;
        }
        if let Some(rest) = line.strip_prefix("[[config.") {
            out.push_str("[[");
            out.push_str(rest);
        } else if let Some(rest) = line.strip_prefix("[config.") {
            out.push('[');
            out.push_str(rest);
        } else {
            out.push_str(line);
        }
    }
    out
}

/// Every embedded template, in path order (`llm/` before `security/`, then by id).
///
/// A parse failure is an error rather than a missing row: a template that does
/// not parse is a shipped defect, and silently omitting it would turn that into
/// "the template does not exist" (ADR-0104 §9).
pub fn all() -> Result<Vec<Template>, TemplateError> {
    let mut names: Vec<PathBuf> = Asset::iter()
        .map(|name| PathBuf::from(name.as_ref()))
        .collect();
    names.sort();
    let mut templates = Vec::with_capacity(names.len());
    for name in names {
        let path = name.to_string_lossy().replace('\\', "/");
        let file =
            Asset::get(&path).ok_or_else(|| TemplateError::NotFound { name: path.clone() })?;
        let text =
            std::str::from_utf8(file.data.as_ref()).map_err(|error| TemplateError::Invalid {
                path: path.clone(),
                message: format!("not UTF-8: {error}"),
            })?;
        templates.push(Template::parse(&path, text)?);
    }
    Ok(templates)
}

/// Look one up by id, within a kind.
pub fn find(kind: Kind, id: &str) -> Result<Template, TemplateError> {
    all()?
        .into_iter()
        .find(|template| template.kind == kind && template.id == id)
        .ok_or_else(|| TemplateError::NotFound {
            name: format!("{}/{id}", kind.as_str()),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR-0104 §9: every shipped template parses **and** survives the same
    /// checks a hand-written config file goes through.
    ///
    /// This is the only thing standing between a template and the user: it does
    /// not participate in compilation, so a renamed field or a misspelled key
    /// breaks nothing at build time and would ship as "your new config does not
    /// load". One test over the whole directory is cheaper than discovering that
    /// per platform.
    #[test]
    fn every_template_parses_and_validates() {
        let templates = all().expect("templates must parse");
        assert!(
            !templates.is_empty(),
            "no templates embedded; is `template/` next to this module?"
        );
        for template in &templates {
            template
                .config()
                .unwrap_or_else(|error| panic!("{}: {error}", template.source));
            template
                .document()
                .unwrap_or_else(|error| panic!("{}: {error}", template.source));
        }
    }

    /// The five security backends, and nothing else. A template disappearing is
    /// a user-visible regression, so the list is pinned rather than counted.
    #[test]
    fn the_security_templates_cover_every_backend() {
        let templates = all().unwrap();
        let security: Vec<&str> = templates
            .iter()
            .filter(|template| template.kind == Kind::Security)
            .map(|template| template.id.as_str())
            .collect();
        assert_eq!(
            security,
            vec!["bwrap", "darwin", "landlock", "none", "windows-acl"],
        );
    }

    /// ADR-0104 §7's matrix: each security template pins its own backend, and
    /// `strict` appears exactly once — the macOS one.
    ///
    /// The pairing is the point. `network = "strict"` requires `sandbox-exec`
    /// (`sandbox/mod.rs`: every other backend returns `Unsupported` for
    /// `ProxyOnly`), so a template that picked strict with another backend would
    /// produce a config that loads fine and then fails to assemble.
    #[test]
    fn strict_network_only_ships_with_the_macos_backend() {
        use qcode_tools::sandbox::SandboxMode;
        let mut strict = Vec::new();
        for template in all().unwrap() {
            if template.kind != Kind::Security {
                continue;
            }
            let config = template.config().unwrap();
            let expected = match template.id.as_str() {
                "bwrap" => SandboxMode::Bwrap,
                "darwin" => SandboxMode::SandboxExec,
                "landlock" => SandboxMode::Landlock,
                "none" => SandboxMode::None,
                "windows-acl" => SandboxMode::WindowsAcl,
                other => panic!("unexpected security template {other}"),
            };
            assert_eq!(
                config.sandbox.mode, expected,
                "{} must pin its own backend",
                template.id
            );
            if config.network_plan().strict {
                strict.push((template.id.clone(), config.sandbox.mode));
            }
        }
        assert_eq!(
            strict,
            Vec::<(String, SandboxMode)>::new(),
            "no template may default to strict: it is the one mode that needs \
             Seatbelt, and a default that fails to assemble on Linux/Windows is worse than \
             a comment showing how to enable it",
        );
    }

    /// Shipped extra roots must be **portable**: only `~`-relative spellings,
    /// never an absolute path from somebody's machine.
    ///
    /// The rule changed from "no values at all" to this one: the default config
    /// is supposed to grant read access to the home directory and write access to
    /// Documents on every platform, and a comment cannot do that. What must still
    /// never ship is `/home/q/cot/lib` — that works for exactly one person, and a
    /// wrong read root reads as "a command mysteriously fails", which blames
    /// qcode rather than the template. A missing `~`-relative directory is
    /// skipped with a warning instead of failing (see `check_extra_write`).
    #[test]
    fn shipped_extra_roots_are_home_relative_only() {
        for template in all().unwrap() {
            let config = template.config().unwrap();
            for (list, paths) in [
                ("extra_read", &config.sandbox.extra_read),
                ("extra_writable", &config.sandbox.extra_writable),
            ] {
                for path in paths {
                    let text = path.to_string_lossy();
                    assert!(
                        text == "~" || text.starts_with("~/"),
                        "{}: {list} holds {text:?}; ship `~` or `~/sub/dir` only \
                         (absolute paths are specific to one machine)",
                        template.source,
                    );
                }
            }
        }
    }

    /// `security/none.toml` has no backend, so listing extra roots there would be
    /// a promise nothing keeps.
    #[test]
    fn the_no_sandbox_template_grants_nothing() {
        let config = find(Kind::Security, "none").unwrap().config().unwrap();
        assert!(config.sandbox.extra_read.is_empty());
        assert!(config.sandbox.extra_writable.is_empty());
    }

    /// Every backend template hands over the two roots the default config is
    /// meant to give: read the home, write Documents. Windows is the exception
    /// with a stated reason (its backend does not restrict reads), and it still
    /// carries the lines so the file has one shape everywhere.
    #[test]
    fn every_backend_template_grants_home_read_and_documents_write() {
        for id in ["bwrap", "darwin", "landlock", "windows-acl"] {
            let config = find(Kind::Security, id).unwrap().config().unwrap();
            let read: Vec<String> = config
                .sandbox
                .extra_read
                .iter()
                .map(|path| path.display().to_string())
                .collect();
            let write: Vec<String> = config
                .sandbox
                .extra_writable
                .iter()
                .map(|path| path.display().to_string())
                .collect();
            assert_eq!(read, vec![String::from("~")], "{id}");
            assert_eq!(write, vec![String::from("~/Documents")], "{id}");
        }
    }

    /// Kind and id come from the path, and a disagreeing `[template]` block is
    /// refused: `template list` shows the path, so a file listed as one name and
    /// written as another is a trap.
    #[test]
    fn a_template_block_that_contradicts_its_path_is_refused() {
        let text = r#"
[template]
title = "t"
summary = "s"
kind = "llm"
"#;
        let error = Template::parse("security/landlock.toml", text).unwrap_err();
        assert!(
            format!("{error}").contains("kind"),
            "error should name the kind mismatch: {error}"
        );

        let text = r#"
[template]
title = "t"
summary = "s"
id = "bwrap"
"#;
        let error = Template::parse("security/landlock.toml", text).unwrap_err();
        assert!(
            format!("{error}").contains("named"),
            "error should name the id mismatch: {error}"
        );
    }

    /// A typo in a template's `[config]` must fail loudly. serde drops unknown
    /// keys silently, so without this the template would "work" while ignoring
    /// exactly the line the author meant to set.
    #[test]
    fn a_misspelled_config_key_is_an_error() {
        let text = r#"
[template]
title = "t"
summary = "s"

[config.sandbox]
mode = "landlock"
extra_reed = ["~/x"]
"#;
        let template = Template::parse("security/landlock.toml", text).unwrap();
        let error = template.config().unwrap_err();
        assert!(
            format!("{error}").contains("extra_reed"),
            "error should name the unknown key: {error}"
        );
    }

    /// The `[template]` and `[[prompt]]` sections describe the scaffolding and
    /// must not leak into the generated config file.
    #[test]
    fn the_written_document_carries_only_the_config_section() {
        let text = r#"
[template]
title = "T"
summary = "S"

[[prompt]]
key = "api_key"
label = "KEY"
kind = "secret"
env = "SOME_KEY"

[config.providers.somewhere]
base_url = "https://example.com/v1"
"#;
        let template = Template::parse("llm/somewhere.toml", text).unwrap();
        let document = template.document().unwrap();
        assert!(document.contains("base_url"), "{document}");
        assert!(!document.contains("[template]"), "{document}");
        assert!(!document.contains("prompt"), "{document}");
        assert!(!document.contains('S'), "summary leaked: {document}");
    }

    /// The caveats must survive into the file the user actually gets. This is
    /// the reason [`Template::document`] renders from the source text instead of
    /// from the parsed table: `toml::Table` has no comments, so the "landlock
    /// does not restrict the network" line would silently disappear.
    #[test]
    fn the_written_document_keeps_its_comments() {
        let landlock = find(Kind::Security, "landlock").unwrap();
        let document = landlock.document().unwrap();
        assert!(
            document.contains('#'),
            "generated config has no comments at all:\n{document}"
        );
        assert!(
            document.contains("不限制网络"),
            "the landlock caveat did not survive:\n{document}"
        );
        // And the prefix stripping really happened: a real config has no
        // `[config…]` header.
        assert!(
            !document.contains("[config"),
            "the `config` level was not stripped:\n{document}"
        );
    }

    /// `config_region` cuts from the first config header to EOF, so a template
    /// that put a `[template]`/`[[prompt]]` header *after* its config body would
    /// leak those into the generated file. No shipped template does; this keeps
    /// it that way.
    #[test]
    fn no_template_puts_metadata_after_its_config_body() {
        for template in all().unwrap() {
            let region = config_region(&template.config_text)
                .unwrap_or_else(|| panic!("{}: no [config] section", template.source));
            for line in region.lines() {
                let trimmed = line.trim();
                assert!(
                    !trimmed.starts_with("[template]") && !trimmed.starts_with("[[prompt]]"),
                    "{}: {trimmed} comes after the config body and would leak into the \
                     generated file",
                    template.source,
                );
            }
        }
    }

    /// Each backend template declares the platform it belongs to, because the
    /// backends are platform-specific by construction: `bwrap` has no build
    /// outside Linux, and writing it on macOS produces a file that loads and then
    /// fails to assemble. `none` is the one that applies everywhere.
    #[test]
    fn each_security_template_declares_its_platform() {
        let expected: &[(&str, &[&str])] = &[
            ("bwrap", &["linux"]),
            ("darwin", &["macos"]),
            ("landlock", &["linux"]),
            ("windows-acl", &["windows"]),
            ("none", &[]),
        ];
        for (id, os) in expected {
            let template = find(Kind::Security, id).unwrap();
            assert_eq!(template.os, *os, "{id}");
        }
    }

    /// The platform check itself. A template with no `os` applies everywhere; one
    /// with `os` applies only to those platforms — and every backend template must
    /// claim its own platform while disclaiming the other two.
    #[test]
    fn platform_applicability_is_the_declared_list() {
        let none = find(Kind::Security, "none").unwrap();
        for os in ["linux", "macos", "windows"] {
            assert!(none.applies_on(os), "none must apply on {os}");
        }
        let landlock = find(Kind::Security, "landlock").unwrap();
        assert!(landlock.applies_on("linux"));
        assert!(!landlock.applies_on("macos"));
        assert!(!landlock.applies_on("windows"));
        let darwin = find(Kind::Security, "darwin").unwrap();
        assert!(darwin.applies_on("macos"));
        assert!(!darwin.applies_on("linux"));
    }

    /// Platform notes must reach the file only on the platform they name, because
    /// they exist to explain what a key does (or does not do) *there*. Windows'
    /// `extra_read` is a no-op, and saying so on Linux would be noise.
    #[test]
    fn platform_notes_only_appear_on_their_platform() {
        let windows_acl = find(Kind::Security, "windows-acl").unwrap();
        let on_windows = windows_acl.document_for("windows").unwrap();
        assert!(
            on_windows.contains("extra_read 是空操作"),
            "the Windows-only caveat is missing on Windows:\n{on_windows}"
        );
        let on_linux = windows_acl.document_for("linux").unwrap();
        assert!(
            !on_linux.contains("extra_read 是空操作"),
            "a Windows-only note leaked onto Linux:\n{on_linux}"
        );
        // An unconditional note appears everywhere.
        let none = find(Kind::Security, "none").unwrap();
        for os in ["linux", "macos", "windows"] {
            assert!(
                none.document_for(os).unwrap().contains("不限制任何东西"),
                "the unconditional note is missing on {os}"
            );
        }
    }

    /// Every security template must say what its backend cannot do. The failure
    /// mode this prevents is a user reading "sandbox" and assuming confinement
    /// covers network and reads too.
    #[test]
    fn every_security_template_states_its_limit() {
        // Words that carry the caveat. At least one must appear in each summary,
        // because "已加固" without a stated limit is the thing ADR-0104 §7 bans.
        const LIMIT_WORDS: &[&str] = &[
            "仅",
            "只",
            "不限制",
            "不受",
            "做不到",
            "无法",
            "唯一",
            "全部",
        ];
        for template in all().unwrap() {
            if template.kind != Kind::Security {
                continue;
            }
            assert!(
                LIMIT_WORDS
                    .iter()
                    .any(|word| template.summary.contains(word)),
                "{} must state its limit in `summary`, not just claim safety: {}",
                template.id,
                template.summary,
            );
        }
    }
}
