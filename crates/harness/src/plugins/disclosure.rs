//! Per-plugin disclosure report (SPEC §11 #22): what each plugin declares it
//! needs and how its tools were classified, so the operator can review it
//! before promoting. Derived and overwritten at every load; never read back,
//! so it is not a persistent store (AGENTS.md #7).

use super::manifest::{Provenance, ServerKind};
use super::{PluginSet, PluginStatus};
use crate::orchestrator::ToolExecutor;
use crate::policy::{ToolPolicy, TrustLevel};
use serde::Serialize;
use std::path::Path;

#[derive(Debug, Serialize)]
pub struct PluginReport {
    pub id: String,
    pub name: String,
    pub version: String,
    pub provenance: Provenance,
    /// `remote`, `container`, or `legacy` (an `MCP_SERVERS` entry).
    pub kind: String,
    /// Host and port only.
    pub server: String,
    pub image: Option<String>,
    pub egress: Vec<String>,
    pub volumes: Vec<String>,
    /// Names only, never values.
    pub secrets: Vec<String>,
    pub status: String,
    pub trust: Option<TrustLevel>,
    pub tools: usize,
    pub safe: usize,
    pub conditional: usize,
    pub destructive: usize,
    pub pending: usize,
    pub groups: Vec<String>,
    /// For `container` plugins: a service the operator can add to compose.
    pub compose_snippet: Option<String>,
}

pub fn build(set: &PluginSet, policy: &ToolPolicy) -> Vec<PluginReport> {
    let names: Vec<&str> = set
        .tool_schemas()
        .iter()
        .filter_map(|s| s["function"]["name"].as_str())
        .collect();
    set.plugins()
        .iter()
        .map(|p| {
            let tools: Vec<&str> = names
                .iter()
                .copied()
                .filter(|n| super::namespace(n) == Some(p.id.as_str()))
                .collect();
            let (safe, destructive, conditional, pending) = policy.counts_for(&tools);
            let status = match &p.status {
                PluginStatus::Ready => "ready".to_owned(),
                PluginStatus::Failed(reason) => format!("failed: {reason}"),
            };
            let groups = set
                .groups()
                .iter()
                .filter(|g| g.plugin == p.id)
                .map(|g| g.id.clone())
                .collect();
            let base = PluginReport {
                id: p.id.clone(),
                name: p.id.clone(),
                version: String::new(),
                provenance: p.provenance(),
                kind: "legacy".into(),
                server: String::new(),
                image: None,
                egress: Vec::new(),
                volumes: Vec::new(),
                secrets: Vec::new(),
                status,
                trust: policy.trust(&p.id).map(|t| t.level),
                tools: tools.len(),
                safe,
                conditional,
                destructive,
                pending,
                groups,
                compose_snippet: None,
            };
            let Some(m) = &p.manifest else {
                return base;
            };
            let server = reqwest::Url::parse(&m.server.url)
                .ok()
                .map(|u| {
                    let host = u.host_str().unwrap_or_default().to_owned();
                    u.port()
                        .map_or(host.clone(), |port| format!("{host}:{port}"))
                })
                .unwrap_or_default();
            PluginReport {
                name: m.name.clone(),
                version: m.version.clone(),
                kind: match m.server.kind {
                    ServerKind::Remote => "remote".into(),
                    ServerKind::Container => "container".into(),
                },
                server,
                image: m.server.image.clone(),
                egress: m.permissions.egress.clone(),
                volumes: m.permissions.volumes.clone(),
                secrets: m.permissions.secrets.clone(),
                compose_snippet: (m.server.kind == ServerKind::Container)
                    .then(|| compose_snippet(m)),
                ..base
            }
        })
        .collect()
}

/// Logs one line per plugin and writes the JSON report. Write failures are
/// logged, never fatal.
pub fn publish(reports: &[PluginReport], path: &Path) {
    for r in reports {
        tracing::info!(
            plugin = %r.id,
            provenance = ?r.provenance,
            kind = %r.kind,
            server = %r.server,
            egress = ?r.egress,
            volumes = ?r.volumes,
            secrets = ?r.secrets,
            status = %r.status,
            trust = ?r.trust,
            tools = r.tools,
            safe = r.safe,
            conditional = r.conditional,
            destructive = r.destructive,
            pending = r.pending,
            "plugin disclosure"
        );
    }
    let body = match serde_json::to_string_pretty(reports) {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(error = %e, "plugin report serialize failed");
            return;
        }
    };
    if let Err(e) = std::fs::write(path, body) {
        tracing::error!(path = %path.display(), error = %e, "plugin report write failed");
    }
}

/// The harness never runs containers (SPEC §11 #21); the operator adds this
/// service to compose and starts it.
fn compose_snippet(m: &super::manifest::Manifest) -> String {
    let mut out = format!(
        "  arno-plugin-{id}:\n    image: {image}\n    user: \"10001:10001\"\n    read_only: true\n    restart: unless-stopped\n",
        id = m.id,
        image = m.server.image.as_deref().unwrap_or_default(),
    );
    if !m.permissions.secrets.is_empty() {
        out.push_str(&format!(
            "    env_file:\n      - ./config/plugins/{}.env\n",
            m.id
        ));
    }
    if !m.permissions.volumes.is_empty() {
        out.push_str("    volumes:\n");
        for v in &m.permissions.volumes {
            out.push_str(&format!("      - {v}\n"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::Plugin;
    use crate::plugins::manifest::Manifest;
    use std::sync::Arc;

    struct Schemas(Vec<serde_json::Value>);

    #[async_trait::async_trait]
    impl crate::orchestrator::ToolExecutor for Schemas {
        async fn execute(
            &self,
            _: &str,
            _: &serde_json::Value,
        ) -> Result<serde_json::Value, contract::ErrorCode> {
            Ok(serde_json::Value::Null)
        }
        fn tool_schemas(&self) -> &[serde_json::Value] {
            &self.0
        }
    }

    #[test]
    fn report_lists_declared_permissions_and_never_secret_values() {
        let text = "arno_manifest = 1\nid = \"notes\"\nname = \"Notes\"\nversion = \"2\"\n\
            provenance = \"third-party\"\n[server]\nkind = \"container\"\n\
            url = \"http://arno-plugin-notes:9000/mcp\"\nimage = \"notes@sha256:abc\"\n\
            auth = { kind = \"bearer\", secret = \"NOTES_TOKEN\" }\n\
            [permissions]\negress = [\"api.notes.example:443\"]\nvolumes = [\"attachments:/attachments:ro\"]\n\
            secrets = [\"NOTES_TOKEN\"]\n";
        let m = Manifest::parse(text, "notes.toml").unwrap();
        let exec = Arc::new(Schemas(vec![
            serde_json::json!({"function": {"name": "notes.list"}}),
        ]));
        let set = PluginSet::assemble(
            exec,
            vec![Plugin {
                id: "notes".into(),
                manifest: Some(m),
                hash: Some("h".into()),
                status: PluginStatus::Ready,
            }],
        );
        let path = std::env::temp_dir().join(format!("arno-report-{}.json", std::process::id()));
        let policy = ToolPolicy::load(path.with_extension("policy.json"), Default::default());
        let reports = build(&set, &policy);
        assert_eq!(reports.len(), 1);
        let r = &reports[0];
        assert_eq!(r.kind, "container");
        assert_eq!(r.server, "arno-plugin-notes:9000");
        assert_eq!(r.secrets, vec!["NOTES_TOKEN"]);
        assert_eq!(r.tools, 1);
        assert_eq!(r.pending, 0, "never reconciled, so not counted");
        let snippet = r.compose_snippet.as_deref().unwrap();
        assert!(snippet.contains("image: notes@sha256:abc"));
        assert!(snippet.contains("env_file"));
        publish(&reports, &path);
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("api.notes.example:443"));
        std::fs::remove_file(&path).ok();
    }
}
