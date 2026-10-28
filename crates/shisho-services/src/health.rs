//! Backend reachability probe: db ping plus per-backend reachability (Codex
//! `/api/v1/info`, Tsundoku `/api/v1/health`) and the MangaBaka backbone state.
//! A down or unconfigured backend degrades the report, it never fails it; only
//! an unreachable database is fatal.
//!
//! One probe over `(db, &Services)`, shared by the MCP `status` tool and the
//! REST `/status` endpoint — no dependency on either shell's app state.

use chrono::{DateTime, Utc};
use sea_orm::DatabaseConnection;
use serde::Serialize;
use utoipa::ToSchema;

use crate::Services;
use shisho_codex::{CodexAppInfo, CodexError, MIN_CODEX_VERSION, version_supported};

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct HealthResponse {
    /// `ok` | `degraded` | `unavailable`.
    pub status: String,
    pub db: DbHealth,
    pub codex: BackendHealth,
    pub tsundoku: BackendHealth,
    /// The MangaBaka metadata backbone (dump + API fallback).
    pub mangabaka: MangabakaHealth,
    /// Last successful SWR cache loads.
    pub caches: CacheHealth,
}

/// Backbone state: the dump is the healthy mode; API-only (dump not loaded)
/// reports degraded so a first deploy knows to run the initial ingest.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct MangabakaHealth {
    pub dump_loaded: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub series_rows: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dump_modified_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_refresh_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_refresh_error: Option<String>,
    pub refresh_running: bool,
    pub negative_cache_size: usize,
    pub api_fallback: bool,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DbHealth {
    pub reachable: bool,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BackendHealth {
    pub configured: bool,
    pub reachable: bool,
    /// Codex is the required tier; Tsundoku is an enhancement.
    pub required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CacheHealth {
    pub library_snapshot_last_warmed_at: Option<String>,
    pub tsundoku_last_warmed_at: Option<String>,
}

/// Codex's half of the report. A reachable Codex older than
/// [`MIN_CODEX_VERSION`] carries an error (and degrades the report): it
/// predates `ratings:write`, so minted keys come out read-only.
fn codex_health(info: Result<CodexAppInfo, CodexError>, configured: bool) -> BackendHealth {
    match info {
        Ok(info) => BackendHealth {
            configured: true,
            reachable: true,
            required: true,
            error: (version_supported(&info.version) == Some(false)).then(|| {
                let (major, minor, patch) = MIN_CODEX_VERSION;
                format!(
                    "Codex {} is older than {major}.{minor}.{patch}, the minimum Shisho \
                     supports: rating writes are unavailable",
                    info.version
                )
            }),
        },
        Err(err) => BackendHealth {
            configured,
            reachable: false,
            required: true,
            error: Some(err.to_string()),
        },
    }
}

/// Probe every backend from the caller's domain handles. Never errors: an
/// unreachable backend degrades the report.
pub async fn probe(db: &DatabaseConnection, services: &Services) -> HealthResponse {
    let codex = services.snapshots.codex();
    let tsundoku = services.tsundoku.client();

    let (db_ping, codex_info, tsundoku_health, backbone) = tokio::join!(
        db.ping(),
        codex.info(),
        tsundoku.health(),
        services.metadata.provider().status(),
    );

    let db = DbHealth {
        reachable: db_ping.is_ok(),
    };

    let codex = codex_health(codex_info, services.snapshots.codex().is_configured());

    let tsundoku_configured = tsundoku.is_configured();
    let tsundoku = match tsundoku_health {
        Ok(_) => BackendHealth {
            configured: true,
            reachable: true,
            required: false,
            error: None,
        },
        Err(err) => BackendHealth {
            configured: tsundoku_configured,
            reachable: false,
            required: false,
            // An unconfigured Tsundoku is a deployment choice (Tier 0), not an
            // error worth reporting on every probe.
            error: tsundoku_configured.then(|| err.to_string()),
        },
    };

    let mangabaka = MangabakaHealth {
        dump_loaded: backbone.dump_loaded,
        series_rows: backbone.series_rows,
        dump_modified_at: backbone.dump_modified_at,
        last_refresh_at: backbone.last_refresh.as_ref().map(|a| a.at),
        last_refresh_error: backbone
            .last_refresh
            .as_ref()
            .and_then(|a| a.outcome.as_ref().err().cloned()),
        refresh_running: backbone.refresh_running,
        negative_cache_size: backbone.negative_cache_size,
        api_fallback: backbone.api_fallback,
    };

    let status = if !db.reachable {
        "unavailable"
    } else if codex.reachable
        && codex.error.is_none()
        && (tsundoku.reachable || !tsundoku.configured)
        && mangabaka.dump_loaded
    {
        "ok"
    } else {
        "degraded"
    };

    if status != "ok" {
        tracing::warn!(
            db = db.reachable,
            codex = codex.reachable,
            tsundoku = tsundoku.reachable,
            mangabaka_dump = mangabaka.dump_loaded,
            status,
            "status probe degraded"
        );
    }

    HealthResponse {
        status: status.to_owned(),
        db,
        codex,
        tsundoku,
        mangabaka,
        caches: CacheHealth {
            library_snapshot_last_warmed_at: services.snapshots.last_warm().map(|t| t.to_rfc3339()),
            tsundoku_last_warmed_at: services.tsundoku.last_warm().map(|t| t.to_rfc3339()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(version: &str) -> Result<CodexAppInfo, CodexError> {
        Ok(CodexAppInfo {
            name: "codex".into(),
            version: version.into(),
        })
    }

    #[test]
    fn a_supported_codex_is_healthy() {
        let health = codex_health(info("2.9.0"), true);
        assert!(health.reachable);
        assert_eq!(health.error, None);
    }

    #[test]
    fn an_old_codex_is_reachable_but_flagged() {
        let health = codex_health(info("2.8.1"), true);
        assert!(health.reachable);
        let error = health.error.expect("old version flagged");
        assert!(
            error.contains("2.8.1") && error.contains("2.9.0"),
            "{error}"
        );
    }

    #[test]
    fn an_unparseable_version_is_not_flagged() {
        assert_eq!(codex_health(info("dev"), true).error, None);
    }
}
