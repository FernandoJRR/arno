//! Per-plugin secrets from `<id>.env` (SPEC §11 #21).
//!
//! Values stay in this map: never in the process env, never logged, attached
//! only to the owning plugin's connection.

use std::collections::HashMap;
use std::path::Path;

pub struct Secrets(HashMap<String, String>);

impl std::fmt::Debug for Secrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.0.keys()).finish()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SecretsError {
    #[error("{file}: unreadable: {source}")]
    Read {
        file: String,
        source: std::io::Error,
    },
    // Line number only: the line itself may hold a value.
    #[error("{file}:{line}: expected KEY=VALUE")]
    Syntax { file: String, line: usize },
    #[error("declared secret {0} missing from the plugin env file")]
    Missing(String),
}

impl Secrets {
    /// Reads `path` if it exists; an absent file means no secrets.
    pub fn load(path: &Path) -> Result<Self, SecretsError> {
        let file = path.display().to_string();
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self(HashMap::new())),
            Err(source) => return Err(SecretsError::Read { file, source }),
        };
        Self::parse(&text, &file)
    }

    /// `KEY=VALUE` lines; `#` comments and blanks skipped; one pair of
    /// surrounding quotes stripped. No expansion.
    pub fn parse(text: &str, file: &str) -> Result<Self, SecretsError> {
        let mut map = HashMap::new();
        for (i, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let line = line.strip_prefix("export ").unwrap_or(line);
            let (k, v) = line.split_once('=').ok_or(SecretsError::Syntax {
                file: file.into(),
                line: i + 1,
            })?;
            let k = k.trim();
            if k.is_empty() {
                return Err(SecretsError::Syntax {
                    file: file.into(),
                    line: i + 1,
                });
            }
            let v = v.trim();
            let v = ['"', '\'']
                .iter()
                .find_map(|q| v.strip_prefix(*q).and_then(|s| s.strip_suffix(*q)))
                .unwrap_or(v);
            map.insert(k.to_owned(), v.to_owned());
        }
        Ok(Self(map))
    }

    /// Every declared name must be present and non-empty.
    pub fn require(&self, declared: &[String]) -> Result<(), SecretsError> {
        match declared
            .iter()
            .find(|k| self.0.get(*k).is_none_or(|v| v.is_empty()))
        {
            Some(missing) => Err(SecretsError::Missing(missing.clone())),
            None => Ok(()),
        }
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pairs_comments_and_quotes() {
        let s = Secrets::parse(
            "# token\nA_TOKEN=abc=def\nexport B=\"quoted value\"\n\nC='x'\n",
            "t.env",
        )
        .unwrap();
        assert_eq!(s.get("A_TOKEN"), Some("abc=def"));
        assert_eq!(s.get("B"), Some("quoted value"));
        assert_eq!(s.get("C"), Some("x"));
    }

    #[test]
    fn syntax_error_never_echoes_the_line() {
        let err = Secrets::parse("supersecretvalue\n", "t.env").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("t.env:1"));
        assert!(!msg.contains("supersecretvalue"));
    }

    #[test]
    fn missing_or_empty_declared_secret_fails() {
        let s = Secrets::parse("A=1\nB=\n", "t.env").unwrap();
        assert!(s.require(&["A".into()]).is_ok());
        assert!(matches!(s.require(&["B".into()]), Err(SecretsError::Missing(k)) if k == "B"));
        assert!(matches!(s.require(&["C".into()]), Err(SecretsError::Missing(k)) if k == "C"));
    }

    #[test]
    fn debug_shows_names_only() {
        let s = Secrets::parse("TOKEN=hunter2\n", "t.env").unwrap();
        let shown = format!("{s:?}");
        assert!(shown.contains("TOKEN"));
        assert!(!shown.contains("hunter2"));
    }
}
