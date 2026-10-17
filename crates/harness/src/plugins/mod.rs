//! Plugins: the unit through which a hosted service's MCP tools reach the
//! model (SPEC §4.5, §11 #21–#24).
//!
//! A plugin is a manifest in `PLUGINS_DIR` (or a deprecated `MCP_SERVERS`
//! entry). The core only reads generic metadata from it — groups, attachment
//! params, permissions — and never names an app (AGENTS.md #1).

pub mod disclosure;
pub mod manifest;
pub mod router;
pub mod secrets;

use crate::config::BackendAddr;
use crate::mcp::{McpRegistry, RegistryError};
use crate::orchestrator::{NoBackends, ToolExecutor};
use arc_swap::ArcSwap;
use contract::ErrorCode;
use manifest::{Auth, Manifest, Provenance};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Cap on third-party tool descriptions shown to the model (SPEC §11 #22).
pub const THIRD_PARTY_DESCRIPTION_MAX: usize = 400;

#[derive(Debug, Clone)]
pub struct Plugin {
    pub id: String,
    /// `None` for a legacy `MCP_SERVERS` entry (always treated as trusted).
    pub manifest: Option<Manifest>,
    pub hash: Option<String>,
    pub status: PluginStatus,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PluginStatus {
    Ready,
    /// Not connected; the reason never contains secret values.
    Failed(String),
}

impl Plugin {
    pub fn is_legacy(&self) -> bool {
        self.manifest.is_none()
    }

    pub fn provenance(&self) -> Provenance {
        self.manifest
            .as_ref()
            .map_or(Provenance::FirstParty, |m| m.provenance)
    }
}

/// A routable set of tools (SPEC §11 #23). Ids are qualified:
/// `<plugin>.<group>`, or just `<plugin>` for a plugin's implicit group.
#[derive(Debug, Clone, PartialEq)]
pub struct GroupDef {
    pub id: String,
    pub plugin: String,
    pub when: String,
    pub how: String,
    /// Namespaced tool names, all discovered and enabled.
    pub tools: Vec<String>,
    /// Qualified ids of existing groups.
    pub related: Vec<String>,
}

/// One immutable view of every loaded plugin. Orders read a snapshot; a
/// reload swaps in a new one between orders (SPEC §11 #24).
pub struct PluginSet {
    executor: Arc<dyn ToolExecutor>,
    plugins: Vec<Plugin>,
    groups: Vec<GroupDef>,
}

impl PluginSet {
    pub fn assemble(executor: Arc<dyn ToolExecutor>, plugins: Vec<Plugin>) -> Self {
        let groups = build_groups(executor.tool_schemas(), &plugins);
        Self {
            executor,
            plugins,
            groups,
        }
    }

    /// Every tool namespace becomes a legacy plugin — the shape tests and
    /// `MCP_SERVERS`-only deployments get.
    pub fn from_executor(executor: Arc<dyn ToolExecutor>) -> Self {
        let mut ids: Vec<String> = Vec::new();
        for s in executor.tool_schemas() {
            if let Some(ns) = s["function"]["name"].as_str().and_then(namespace)
                && !ids.iter().any(|i| i == ns)
            {
                ids.push(ns.to_owned());
            }
        }
        let plugins = ids
            .into_iter()
            .map(|id| Plugin {
                id,
                manifest: None,
                hash: None,
                status: PluginStatus::Ready,
            })
            .collect();
        Self::assemble(executor, plugins)
    }

    pub fn plugins(&self) -> &[Plugin] {
        &self.plugins
    }

    pub fn groups(&self) -> &[GroupDef] {
        &self.groups
    }

    pub fn group(&self, id: &str) -> Option<&GroupDef> {
        self.groups.iter().find(|g| g.id == id)
    }

    /// Plugins that did not load (secrets or connection).
    pub fn failed(&self) -> Vec<&Plugin> {
        self.plugins
            .iter()
            .filter(|p| matches!(p.status, PluginStatus::Failed(_)))
            .collect()
    }

    pub fn plugin(&self, id: &str) -> Option<&Plugin> {
        self.plugins.iter().find(|p| p.id == id)
    }

    /// Whether `param` of `tool` takes an attachment handle (SPEC §4.4):
    /// declared in the manifest, or the legacy `*_ref` suffix convention.
    pub fn is_attachment_param(&self, tool: &str, param: &str) -> bool {
        let plugin = namespace(tool).and_then(|ns| self.plugin(ns));
        match plugin.and_then(|p| p.manifest.as_ref()) {
            Some(m) => m.tools.attachment_params.iter().any(|a| a == param),
            None => param.ends_with("_ref"),
        }
    }

    /// Distinct attachment parameter names across discovered tools, sorted.
    pub fn attachment_param_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .tool_schemas()
            .iter()
            .filter_map(|s| {
                let tool = s["function"]["name"].as_str()?;
                let props = s["function"]["parameters"]["properties"].as_object()?;
                Some(
                    props
                        .keys()
                        .filter(|k| self.is_attachment_param(tool, k))
                        .cloned()
                        .collect::<Vec<_>>(),
                )
            })
            .flatten()
            .collect();
        names.sort();
        names.dedup();
        names
    }

    /// Whether any parameter in this tool schema takes an attachment.
    pub fn takes_attachment(&self, schema: &serde_json::Value) -> bool {
        let Some(name) = schema["function"]["name"].as_str() else {
            return false;
        };
        schema["function"]["parameters"]["properties"]
            .as_object()
            .is_some_and(|props| props.keys().any(|k| self.is_attachment_param(name, k)))
    }
}

#[async_trait::async_trait]
impl ToolExecutor for PluginSet {
    async fn execute(
        &self,
        tool: &str,
        args: &serde_json::Value,
    ) -> Result<serde_json::Value, ErrorCode> {
        self.executor.execute(tool, args).await
    }

    async fn execute_frozen(
        &self,
        tool: &str,
        args: &serde_json::Value,
    ) -> Result<serde_json::Value, ErrorCode> {
        self.executor.execute_frozen(tool, args).await
    }

    fn tool_schemas(&self) -> &[serde_json::Value] {
        self.executor.tool_schemas()
    }
}

/// Swappable handle to the current [`PluginSet`].
pub struct PluginRegistry {
    current: ArcSwap<PluginSet>,
}

impl PluginRegistry {
    pub fn new(set: PluginSet) -> Self {
        Self {
            current: ArcSwap::from_pointee(set),
        }
    }

    pub fn from_executor(executor: Arc<dyn ToolExecutor>) -> Self {
        Self::new(PluginSet::from_executor(executor))
    }

    pub fn snapshot(&self) -> Arc<PluginSet> {
        self.current.load_full()
    }

    /// Only the FIFO worker calls this, so it always lands between orders.
    pub fn store(&self, set: PluginSet) {
        self.current.store(Arc::new(set));
    }
}

/// Namespace of a `<plugin>.<tool>` name.
pub fn namespace(tool: &str) -> Option<&str> {
    tool.split_once('.').map(|(ns, _)| ns)
}

/// Where plugins come from.
#[derive(Debug, Clone)]
pub struct LoadSpec {
    pub dir: PathBuf,
    pub legacy: Vec<(String, BackendAddr)>,
    pub timeout: Duration,
}

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error(transparent)]
    Manifest(#[from] manifest::ManifestError),
    #[error("{file}: unreadable: {source}")]
    Io {
        file: String,
        source: std::io::Error,
    },
    #[error("{file}: file name must be `<id>.toml` (id is {id:?})")]
    FileName { file: String, id: String },
    #[error("plugin id {0:?} is defined both by a manifest and by MCP_SERVERS")]
    LegacyConflict(String),
    #[error("mcp backend from MCP_SERVERS failed: {0}")]
    Legacy(RegistryError),
}

/// Reads manifests, connects, discovers, applies allowlists. A plugin whose
/// secrets or connection fail is kept as `Failed`; manifest errors and
/// legacy backends that fail abort (SPEC §11 #21).
pub async fn load(spec: &LoadSpec) -> Result<PluginSet, LoadError> {
    let manifests = read_manifests(&spec.dir)?;
    for (name, _) in &spec.legacy {
        if manifests.iter().any(|(m, _)| &m.id == name) {
            return Err(LoadError::LegacyConflict(name.clone()));
        }
    }
    if !spec.legacy.is_empty() {
        tracing::warn!(
            servers = spec.legacy.len(),
            "MCP_SERVERS is deprecated — move each entry to a manifest in PLUGINS_DIR (SPEC §11 #21)"
        );
    }

    let mut plugins = Vec::new();
    let mut servers = Vec::new();
    for (m, hash) in manifests {
        let status = match backend_addr(&m, &spec.dir) {
            Ok(addr) => {
                servers.push((m.id.clone(), addr));
                PluginStatus::Ready
            }
            Err(reason) => {
                tracing::error!(plugin = %m.id, %reason, "plugin not loaded");
                PluginStatus::Failed(reason)
            }
        };
        plugins.push(Plugin {
            id: m.id.clone(),
            manifest: Some(m),
            hash: Some(hash),
            status,
        });
    }
    for (name, addr) in &spec.legacy {
        servers.push((name.clone(), addr.clone()));
        plugins.push(Plugin {
            id: name.clone(),
            manifest: None,
            hash: None,
            status: PluginStatus::Ready,
        });
    }

    if servers.is_empty() {
        warn_failed(&plugins);
        tracing::info!("no plugins connected — tool dispatch reports backend_unavailable");
        return Ok(PluginSet::assemble(Arc::new(NoBackends), plugins));
    }

    let (mut registry, failed) = McpRegistry::connect_lenient(&servers, spec.timeout).await;
    for (name, err) in failed {
        let plugin = plugins
            .iter_mut()
            .find(|p| p.id == name)
            .expect("every server comes from a plugin");
        if plugin.is_legacy() {
            // Unchanged legacy semantics: an unreachable backend is a config error.
            return Err(LoadError::Legacy(err));
        }
        tracing::error!(plugin = %name, error = %err, "plugin not loaded");
        plugin.status = PluginStatus::Failed(err.to_string());
    }
    warn_failed(&plugins);

    let by_id: HashMap<String, Plugin> =
        plugins.iter().map(|p| (p.id.clone(), p.clone())).collect();
    registry.retain(|tool| {
        let Some((ns, bare)) = tool.split_once('.') else {
            return false;
        };
        match by_id.get(ns).and_then(|p| p.manifest.as_ref()) {
            Some(m) => m.enables(bare),
            None => true,
        }
    });
    registry.map_schemas(|schema| {
        let third_party = schema["function"]["name"]
            .as_str()
            .and_then(namespace)
            .and_then(|ns| by_id.get(ns))
            .is_some_and(|p| p.provenance() == Provenance::ThirdParty);
        if third_party && let Some(d) = schema["function"]["description"].as_str() {
            schema["function"]["description"] =
                manifest::sanitize(d, THIRD_PARTY_DESCRIPTION_MAX).into();
        }
    });

    Ok(PluginSet::assemble(Arc::new(registry), plugins))
}

/// One summary line when manifest plugins failed (secrets or connection).
fn warn_failed(plugins: &[Plugin]) {
    let failed: Vec<&str> = plugins
        .iter()
        .filter(|p| !p.is_legacy() && matches!(p.status, PluginStatus::Failed(_)))
        .map(|p| p.id.as_str())
        .collect();
    if failed.is_empty() {
        return;
    }
    let enabled = plugins.iter().filter(|p| !p.is_legacy()).count();
    tracing::warn!(
        plugins = %failed.join(", "),
        "{} of {enabled} enabled plugins could not be reached — running without them",
        failed.len()
    );
}

impl LoadSpec {
    pub fn from_config(cfg: &crate::config::Config) -> Self {
        Self {
            dir: cfg.plugins_dir.clone(),
            legacy: cfg.mcp_servers.clone(),
            timeout: cfg.mcp_tool_timeout,
        }
    }
}

/// Brings policy in line with a freshly loaded set: plugin trust first, then
/// tool reconcile (which may demote), classification, disclosure. A down
/// model leaves tools `Pending` (fail-closed) for the retry task.
pub async fn activate(
    set: &PluginSet,
    policy: &crate::policy::ToolPolicy,
    provider: &dyn crate::model::ModelProvider,
    report_path: &Path,
) {
    let facts: Vec<crate::policy::PluginFacts<'_>> = set
        .plugins()
        .iter()
        .filter_map(|p| {
            Some(crate::policy::PluginFacts {
                id: &p.id,
                manifest_hash: p.hash.as_deref()?,
                first_party: p.provenance() == Provenance::FirstParty,
            })
        })
        .collect();
    policy.reconcile_plugins(&facts);
    let schemas = set.tool_schemas();
    let newly_pending = policy.reconcile(schemas);
    if !newly_pending.is_empty() {
        let (resolved, still_pending) = policy
            .classify_pending(provider, schemas, &newly_pending)
            .await;
        tracing::info!(resolved, still_pending, "tool classification");
    }
    let (safe, destructive, conditional, pending) = policy.counts();
    tracing::info!(safe, destructive, conditional, pending, "tool policy ready");
    disclosure::publish(&disclosure::build(set, policy), report_path);
}

/// Re-reads `PLUGINS_DIR` and swaps the set. Runs only on the FIFO worker,
/// between orders (SPEC §11 #24); on error the current set stays.
pub async fn reload(st: &crate::state::AppState) -> Result<(), LoadError> {
    // Hand edits (promotions) since boot take effect now.
    st.policy.reload_from_disk();
    let set = load(&LoadSpec::from_config(&st.cfg)).await?;
    activate(
        &set,
        &st.policy,
        st.provider.as_ref(),
        &st.cfg.plugins_report_path,
    )
    .await;
    st.plugins.store(set);
    Ok(())
}

/// `<id>.toml` files in `dir`, sorted; a missing dir means none.
fn read_manifests(dir: &Path) -> Result<Vec<(Manifest, String)>, LoadError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::info!(dir = %dir.display(), "PLUGINS_DIR does not exist — no manifests");
            return Ok(Vec::new());
        }
        Err(source) => {
            return Err(LoadError::Io {
                file: dir.display().to_string(),
                source,
            });
        }
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "toml"))
        .collect();
    paths.sort();

    let mut out: Vec<(Manifest, String)> = Vec::new();
    for path in paths {
        let file = path.display().to_string();
        let text = std::fs::read_to_string(&path).map_err(|source| LoadError::Io {
            file: file.clone(),
            source,
        })?;
        let m = Manifest::parse(&text, &file)?;
        // File name == id also makes ids unique within the dir.
        if path.file_stem().and_then(|s| s.to_str()) != Some(m.id.as_str()) {
            return Err(LoadError::FileName { file, id: m.id });
        }
        out.push((m, manifest::hash(&text)));
    }
    Ok(out)
}

/// Builds the connection for a manifest, attaching only its own secrets.
fn backend_addr(m: &Manifest, dir: &Path) -> Result<BackendAddr, String> {
    let secrets =
        secrets::Secrets::load(&dir.join(format!("{}.env", m.id))).map_err(|e| e.to_string())?;
    secrets
        .require(&m.permissions.secrets)
        .map_err(|e| e.to_string())?;
    let url = reqwest::Url::parse(&m.server.url).map_err(|_| "server.url invalid".to_owned())?;
    let headers = match &m.server.auth {
        Some(Auth::Bearer { secret }) => {
            let token = secrets.get(secret).expect("required above");
            vec![("Authorization".to_owned(), format!("Bearer {token}"))]
        }
        None => Vec::new(),
    };
    Ok(BackendAddr::Http { url, headers })
}

fn build_groups(schemas: &[serde_json::Value], plugins: &[Plugin]) -> Vec<GroupDef> {
    let discovered: Vec<&str> = schemas
        .iter()
        .filter_map(|s| s["function"]["name"].as_str())
        .collect();
    let mut groups: Vec<GroupDef> = Vec::new();
    // Declared `related` entries, resolved once every group exists.
    let mut pending_related: Vec<(usize, String, Vec<String>)> = Vec::new();

    for p in plugins {
        let tools: Vec<&str> = discovered
            .iter()
            .copied()
            .filter(|t| namespace(t) == Some(p.id.as_str()))
            .collect();
        let mut grouped: HashSet<&str> = HashSet::new();
        let declared = p.manifest.as_ref().map_or(&[][..], |m| &m.groups[..]);
        for g in declared {
            let mut members = Vec::new();
            for bare in &g.tools {
                let full = format!("{}.{bare}", p.id);
                match tools.iter().find(|t| **t == full) {
                    Some(t) => {
                        grouped.insert(t);
                        members.push(full);
                    }
                    // Servers drift; a stale group entry is not fatal.
                    None if p.status == PluginStatus::Ready => {
                        tracing::warn!(plugin = %p.id, group = %g.id, tool = %bare,
                            "group names a tool that was not discovered or is disabled");
                    }
                    None => {}
                }
            }
            pending_related.push((groups.len(), p.id.clone(), g.related.clone()));
            groups.push(GroupDef {
                id: format!("{}.{}", p.id, g.id),
                plugin: p.id.clone(),
                when: g.when.clone(),
                how: g.how.clone(),
                tools: members,
                related: Vec::new(),
            });
        }
        let rest: Vec<String> = tools
            .iter()
            .filter(|t| !grouped.contains(*t))
            .map(|t| (*t).to_owned())
            .collect();
        if rest.is_empty() {
            continue;
        }
        let name = p
            .manifest
            .as_ref()
            .map_or(p.id.as_str(), |m| m.name.as_str());
        if declared.is_empty() {
            groups.push(GroupDef {
                id: p.id.clone(),
                plugin: p.id.clone(),
                when: manifest::sanitize(name, manifest::WHEN_MAX),
                how: String::new(),
                tools: rest,
                related: Vec::new(),
            });
        } else {
            let other = format!("{}.other", p.id);
            match groups.iter_mut().find(|g| g.id == other) {
                Some(g) => g.tools.extend(rest),
                None => groups.push(GroupDef {
                    id: other,
                    plugin: p.id.clone(),
                    when: manifest::sanitize(&format!("Other {name} tools"), manifest::WHEN_MAX),
                    how: String::new(),
                    tools: rest,
                    related: Vec::new(),
                }),
            }
        }
    }

    let ids: HashSet<String> = groups.iter().map(|g| g.id.clone()).collect();
    for (idx, plugin, related) in pending_related {
        for r in related {
            let local = format!("{plugin}.{r}");
            let resolved = if ids.contains(&r) {
                Some(r.clone())
            } else if !r.contains('.') && ids.contains(&local) {
                Some(local)
            } else {
                None
            };
            match resolved {
                Some(id) if !groups[idx].related.contains(&id) && id != groups[idx].id => {
                    groups[idx].related.push(id)
                }
                Some(_) => {}
                None => tracing::warn!(group = %groups[idx].id, related = %r,
                    "related group not found"),
            }
        }
    }
    // Groups with no tools left are not routable.
    groups.retain(|g| !g.tools.is_empty());
    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Schemas(Vec<serde_json::Value>);

    #[async_trait::async_trait]
    impl ToolExecutor for Schemas {
        async fn execute(
            &self,
            _: &str,
            _: &serde_json::Value,
        ) -> Result<serde_json::Value, ErrorCode> {
            Ok(serde_json::Value::Null)
        }
        fn tool_schemas(&self) -> &[serde_json::Value] {
            &self.0
        }
    }

    fn schema(name: &str, params: &[&str]) -> serde_json::Value {
        let props: serde_json::Map<String, serde_json::Value> = params
            .iter()
            .map(|p| ((*p).to_owned(), serde_json::json!({"type": "string"})))
            .collect();
        serde_json::json!({"type": "function", "function": {
            "name": name, "description": "", "parameters": {"type": "object", "properties": props}}})
    }

    fn manifest(id: &str, extra: &str) -> Manifest {
        let text = format!(
            "arno_manifest = 1\nid = \"{id}\"\nname = \"{id} app\"\nversion = \"1\"\n\
             provenance = \"first-party\"\n[server]\nkind = \"remote\"\nurl = \"http://x/mcp\"\n{extra}"
        );
        Manifest::parse(&text, "t.toml").unwrap()
    }

    fn plugin(m: Manifest) -> Plugin {
        Plugin {
            id: m.id.clone(),
            manifest: Some(m),
            hash: Some("h".into()),
            status: PluginStatus::Ready,
        }
    }

    #[test]
    fn plugin_without_groups_gets_one_implicit_group() {
        let exec = Arc::new(Schemas(vec![schema("ocr.extract_text", &["image_ref"])]));
        let set = PluginSet::assemble(exec, vec![plugin(manifest("ocr", ""))]);
        assert_eq!(set.groups().len(), 1);
        let g = &set.groups()[0];
        assert_eq!(g.id, "ocr");
        assert_eq!(g.when, "ocr app");
        assert_eq!(g.tools, vec!["ocr.extract_text"]);
    }

    #[test]
    fn declared_groups_leftovers_and_related_resolve() {
        let exec = Arc::new(Schemas(vec![
            schema("fin.list_tx", &[]),
            schema("fin.add_tx", &[]),
            schema("fin.list_accounts", &[]),
            schema("fin.misc", &[]),
            schema("ocr.extract_text", &["image_ref"]),
        ]));
        let fin = manifest(
            "fin",
            "[[groups]]\nid = \"tx\"\nwhen = \"Transactions\"\ntools = [\"list_tx\", \"add_tx\", \"gone\"]\n\
             related = [\"accounts\", \"ocr\", \"nope\"]\n\
             [[groups]]\nid = \"accounts\"\nwhen = \"Accounts\"\ntools = [\"list_accounts\"]\n",
        );
        let ocr = manifest(
            "ocr",
            "[[groups]]\nid = \"read\"\nwhen = \"Read images\"\ntools = [\"extract_text\"]\nrelated = [\"fin.tx\"]\n",
        );
        let set = PluginSet::assemble(exec, vec![plugin(fin), plugin(ocr)]);
        let tx = set.group("fin.tx").unwrap();
        assert_eq!(tx.tools, vec!["fin.list_tx", "fin.add_tx"]);
        // `ocr` has no implicit group (it declares one), so only `accounts` resolves.
        assert_eq!(tx.related, vec!["fin.accounts"]);
        assert_eq!(set.group("fin.other").unwrap().tools, vec!["fin.misc"]);
        assert_eq!(set.group("ocr.read").unwrap().related, vec!["fin.tx"]);
    }

    #[test]
    fn same_tool_name_in_two_plugins_stays_separate() {
        let exec = Arc::new(Schemas(vec![
            schema("a.search", &[]),
            schema("b.search", &[]),
        ]));
        let set = PluginSet::assemble(
            exec,
            vec![plugin(manifest("a", "")), plugin(manifest("b", ""))],
        );
        assert_eq!(set.group("a").unwrap().tools, vec!["a.search"]);
        assert_eq!(set.group("b").unwrap().tools, vec!["b.search"]);
    }

    #[test]
    fn attachment_params_come_from_the_manifest_or_legacy_suffix() {
        let exec = Arc::new(Schemas(vec![
            schema("ocr.extract_text", &["image_ref"]),
            schema("docs.ingest", &["file_ref", "upload"]),
            schema("old.read", &["image_ref"]),
        ]));
        let docs = manifest("docs", "[tools]\nattachment_params = [\"upload\"]\n");
        let ocr = manifest("ocr", "");
        let legacy = Plugin {
            id: "old".into(),
            manifest: None,
            hash: None,
            status: PluginStatus::Ready,
        };
        let set = PluginSet::assemble(exec, vec![plugin(docs), plugin(ocr), legacy]);
        assert!(set.is_attachment_param("docs.ingest", "upload"));
        assert!(!set.is_attachment_param("docs.ingest", "file_ref"));
        // A manifest that declares nothing takes no attachments.
        assert!(!set.is_attachment_param("ocr.extract_text", "image_ref"));
        assert!(set.is_attachment_param("old.read", "image_ref"));
    }

    #[test]
    fn from_executor_makes_one_legacy_plugin_per_namespace() {
        let exec = Arc::new(Schemas(vec![
            schema("mock.a", &[]),
            schema("mock.b", &[]),
            schema("other.c", &[]),
        ]));
        let set = PluginSet::from_executor(exec);
        let ids: Vec<&str> = set.plugins().iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, vec!["mock", "other"]);
        assert!(set.plugins().iter().all(Plugin::is_legacy));
    }

    #[tokio::test]
    async fn manifest_file_name_must_match_id() {
        let dir = std::env::temp_dir().join(format!("arno-plugins-name-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("wrong.toml"),
            "arno_manifest = 1\nid = \"right\"\nname = \"R\"\nversion = \"1\"\nprovenance = \"third-party\"\n\
             [server]\nkind = \"remote\"\nurl = \"http://127.0.0.1:1/mcp\"\n",
        )
        .unwrap();
        let spec = LoadSpec {
            dir: dir.clone(),
            legacy: Vec::new(),
            timeout: Duration::from_secs(1),
        };
        assert!(matches!(load(&spec).await, Err(LoadError::FileName { .. })));
        std::fs::remove_dir_all(&dir).ok();
    }
}
