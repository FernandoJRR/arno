//! Plugin manifest v1 (SPEC §4.5, §11 #21).
//!
//! A manifest only *describes* a plugin: where its MCP server lives, what it
//! declares it needs, and how its tools group. Nothing in it can loosen tool
//! policy (AGENTS.md #6 exception). Unknown keys and versions fail loudly,
//! like unknown env vars.

use serde::Deserialize;
use sha2::{Digest, Sha256};

pub const MANIFEST_VERSION: u32 = 1;
pub const WHEN_MAX: usize = 80;
pub const HOW_MAX: usize = 400;
pub const NAME_MAX: usize = 80;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub arno_manifest: u32,
    pub id: String,
    pub name: String,
    pub version: String,
    pub provenance: Provenance,
    pub server: Server,
    #[serde(default)]
    pub permissions: Permissions,
    #[serde(default)]
    pub tools: Tools,
    #[serde(default)]
    pub groups: Vec<Group>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Provenance {
    FirstParty,
    ThirdParty,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServerKind {
    Remote,
    /// Run by the operator from the generated compose snippet; the harness
    /// never manages containers itself (SPEC §11 #21).
    Container,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Server {
    pub kind: ServerKind,
    pub url: String,
    /// Pinned image (`...@sha256:...`) for `container` plugins.
    pub image: Option<String>,
    pub auth: Option<Auth>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub enum Auth {
    /// `Authorization: Bearer <secret>`, value read from the plugin env file.
    Bearer { secret: String },
}

/// Declared, not enforced: shown in the disclosure report (SPEC §11 #22).
#[derive(Debug, Clone, Default, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct Permissions {
    #[serde(default)]
    pub egress: Vec<String>,
    #[serde(default)]
    pub volumes: Vec<String>,
    #[serde(default)]
    pub secrets: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tools {
    /// Operator allowlist of bare tool names; `*` and `?` globs.
    #[serde(default = "all_tools")]
    pub enabled: Vec<String>,
    /// Parameters that take an attachment handle (SPEC §4.4).
    #[serde(default)]
    pub attachment_params: Vec<String>,
}

impl Default for Tools {
    fn default() -> Self {
        Self {
            enabled: all_tools(),
            attachment_params: Vec::new(),
        }
    }
}

fn all_tools() -> Vec<String> {
    vec!["*".into()]
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Group {
    pub id: String,
    pub when: String,
    #[serde(default)]
    pub how: String,
    pub tools: Vec<String>,
    #[serde(default)]
    pub related: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("{file}: {source}")]
    Parse {
        file: String,
        source: Box<toml::de::Error>,
    },
    #[error("{file}: {problem}")]
    Invalid { file: String, problem: String },
}

impl Manifest {
    /// Parses and validates; `file` names the source in errors only.
    pub fn parse(text: &str, file: &str) -> Result<Self, ManifestError> {
        let mut m: Manifest = toml::from_str(text).map_err(|e| ManifestError::Parse {
            file: file.into(),
            source: Box::new(e),
        })?;
        let invalid = |problem: String| ManifestError::Invalid {
            file: file.into(),
            problem,
        };
        if m.arno_manifest != MANIFEST_VERSION {
            return Err(invalid(format!(
                "unsupported arno_manifest = {} (this harness reads {MANIFEST_VERSION})",
                m.arno_manifest
            )));
        }
        if !is_ident(&m.id) {
            return Err(invalid(format!(
                "id {:?} must be [a-z0-9-], starting with a letter",
                m.id
            )));
        }
        let url = reqwest::Url::parse(&m.server.url)
            .map_err(|_| invalid("server.url is not a valid URL".into()))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(invalid("server.url must be http(s)".into()));
        }
        if m.server.kind == ServerKind::Container && m.server.image.is_none() {
            return Err(invalid("container plugins must pin server.image".into()));
        }
        if let Some(Auth::Bearer { secret }) = &m.server.auth
            && !m.permissions.secrets.contains(secret)
        {
            return Err(invalid(format!(
                "server.auth uses secret {secret:?} not declared in permissions.secrets"
            )));
        }
        for secret in &m.permissions.secrets {
            if !is_env_key(secret) {
                return Err(invalid(format!(
                    "secret name {secret:?} must be UPPER_SNAKE"
                )));
            }
        }
        let mut seen = std::collections::HashSet::new();
        for g in &m.groups {
            if !is_ident(&g.id) {
                return Err(invalid(format!("group id {:?} must be [a-z0-9-]", g.id)));
            }
            if !seen.insert(g.id.clone()) {
                return Err(invalid(format!("duplicate group id {:?}", g.id)));
            }
        }
        // Author text reaches the model: strip and cap it (SPEC §11 #22).
        m.name = sanitize(&m.name, NAME_MAX);
        for g in &mut m.groups {
            g.when = sanitize(&g.when, WHEN_MAX);
            g.how = sanitize(&g.how, HOW_MAX);
        }
        Ok(m)
    }

    /// Whether the operator allowlist admits this bare tool name.
    pub fn enables(&self, tool: &str) -> bool {
        self.tools.enabled.iter().any(|p| glob_match(p, tool))
    }
}

/// sha256 hex of the manifest bytes; any edit changes it and resets trust.
pub fn hash(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Lowercase letter first, then `[a-z0-9-]`. No `.`: the id is a namespace.
pub fn is_ident(s: &str) -> bool {
    s.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn is_env_key(s: &str) -> bool {
    s.chars()
        .next()
        .is_some_and(|c| c.is_ascii_uppercase() || c == '_')
        && s.chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// Drops control characters, collapses whitespace runs, caps at `max` chars.
pub fn sanitize(text: &str, max: usize) -> String {
    let cleaned: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.chars().take(max).collect()
}

/// `*` matches any run, `?` one char; everything else is literal.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"
arno_manifest = 1
id = "notes"
name = "Notes"
version = "0.1.0"
provenance = "third-party"

[server]
kind = "remote"
url = "http://notes:9000/mcp"
auth = { kind = "bearer", secret = "NOTES_TOKEN" }

[permissions]
egress = ["notes:9000"]
secrets = ["NOTES_TOKEN"]

[tools]
enabled = ["list_*", "get_note"]
attachment_params = ["file_ref"]

[[groups]]
id = "read"
when = "Find or read notes"
how = "Search before reading."
tools = ["list_notes", "get_note"]
related = ["files.upload"]
"#;

    #[test]
    fn valid_manifest_parses() {
        let m = Manifest::parse(VALID, "notes.toml").unwrap();
        assert_eq!(m.id, "notes");
        assert_eq!(m.provenance, Provenance::ThirdParty);
        assert!(
            matches!(m.server.auth, Some(Auth::Bearer { ref secret }) if secret == "NOTES_TOKEN")
        );
        assert_eq!(m.groups[0].related, vec!["files.upload"]);
        assert!(m.enables("list_notes"));
        assert!(m.enables("get_note"));
        assert!(!m.enables("delete_note"));
    }

    #[test]
    fn unknown_version_is_rejected() {
        let text = VALID.replace("arno_manifest = 1", "arno_manifest = 2");
        assert!(matches!(
            Manifest::parse(&text, "x.toml"),
            Err(ManifestError::Invalid { .. })
        ));
    }

    #[test]
    fn unknown_key_is_rejected() {
        let text = VALID.replace("[tools]", "[tools]\nsafe = [\"delete_note\"]");
        assert!(matches!(
            Manifest::parse(&text, "x.toml"),
            Err(ManifestError::Parse { .. })
        ));
        let text = format!("hooks = []\n{VALID}");
        assert!(Manifest::parse(&text, "x.toml").is_err());
    }

    #[test]
    fn id_cannot_contain_a_dot_or_uppercase() {
        for bad in ["no.tes", "Notes", "1notes", ""] {
            let text = VALID.replace("id = \"notes\"", &format!("id = {bad:?}"));
            assert!(Manifest::parse(&text, "x.toml").is_err(), "{bad}");
        }
    }

    #[test]
    fn auth_secret_must_be_declared() {
        let text = VALID.replace("secrets = [\"NOTES_TOKEN\"]", "secrets = []");
        assert!(Manifest::parse(&text, "x.toml").is_err());
    }

    #[test]
    fn container_requires_pinned_image() {
        let text = VALID.replace("kind = \"remote\"", "kind = \"container\"");
        assert!(Manifest::parse(&text, "x.toml").is_err());
    }

    #[test]
    fn author_text_is_stripped_and_capped() {
        let long = "x".repeat(500);
        let text = VALID
            .replace("Find or read notes", &format!("Find\\u0007 or\\n{long}"))
            .replace("Search before reading.", &long);
        let m = Manifest::parse(&text, "x.toml").unwrap();
        assert!(m.groups[0].when.chars().count() <= WHEN_MAX);
        assert!(m.groups[0].when.starts_with("Find or x"));
        assert_eq!(m.groups[0].how.chars().count(), HOW_MAX);
    }

    #[test]
    fn missing_tools_section_enables_everything() {
        let text = VALID.split("[tools]").next().unwrap().to_owned();
        let m = Manifest::parse(&text, "x.toml").unwrap();
        assert!(m.enables("anything"));
        assert!(m.groups.is_empty());
    }

    #[test]
    fn glob_handles_stars_and_question_marks() {
        assert!(glob_match("*", ""));
        assert!(glob_match("list_*", "list_accounts"));
        assert!(glob_match("*_rule", "propose_create_rule"));
        assert!(glob_match("get_?", "get_a"));
        assert!(!glob_match("get_?", "get_ab"));
        assert!(!glob_match("list_*", "propose_list"));
    }
}
