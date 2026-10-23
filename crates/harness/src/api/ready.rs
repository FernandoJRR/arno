//! Readiness report (SPEC §11 #31): whether orders can be served now.
//! Liveness stays `/v1/health`, the Docker healthcheck.

use crate::plugins::PluginStatus;
use crate::state::AppState;
use contract::{ModelReadiness, PluginReadiness, Readiness, ReadyStatus, issue};

pub async fn readiness(st: &AppState) -> Readiness {
    let probe = st.provider.probe().await;
    let last = st.model_health.last();
    let plugins: Vec<PluginReadiness> = st
        .plugins
        .snapshot()
        .plugins()
        .iter()
        .map(|p| PluginReadiness {
            id: p.id.clone(),
            status: match p.status {
                PluginStatus::Ready => "ready".into(),
                PluginStatus::Failed(_) => "failed".into(),
            },
            trust: st
                .policy
                .trust(&p.id)
                .and_then(|t| serde_json::to_value(t.level).ok())
                .and_then(|v| v.as_str().map(str::to_owned)),
        })
        .collect();

    let mut issues = Vec::new();
    if !probe.reachable {
        issues.push(issue::MODEL_UNREACHABLE);
    }
    match probe.gpu_share {
        Some(s) if s <= 0.0 => issues.push(issue::MODEL_ON_CPU),
        Some(s) if s < 1.0 => issues.push(issue::MODEL_PARTLY_ON_CPU),
        _ => {}
    }
    if last.timed_out {
        issues.push(issue::MODEL_TIMED_OUT);
    }
    if plugins.iter().any(|p| p.status != "ready") {
        issues.push(issue::PLUGIN_FAILED);
    }
    let status = if !probe.reachable {
        ReadyStatus::Unavailable
    } else if issues.is_empty() {
        ReadyStatus::Ready
    } else {
        ReadyStatus::Degraded
    };
    Readiness {
        status,
        issues: issues.into_iter().map(str::to_owned).collect(),
        model: ModelReadiness {
            name: st.cfg.model.clone(),
            reachable: probe.reachable,
            gpu_share: probe.gpu_share,
            last_elapsed_ms: last.elapsed_ms,
            last_timed_out: last.timed_out,
        },
        plugins,
    }
}

/// Logs a report: warn with its issue codes unless ready.
pub fn log(report: &Readiness) {
    if report.status == ReadyStatus::Ready {
        tracing::info!("model readiness ready");
    } else {
        tracing::warn!(status = ?report.status, issues = ?report.issues, "model readiness degraded");
    }
}
