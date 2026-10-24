//! `shisho eval`: replay a user's library through the backlog rankers and
//! print hold-out metrics (see `shisho_services::eval`), so a ranking change
//! is judged by a number rather than by feel.
//!
//! Live mode reads the user's library from Codex with their stored minted key,
//! read-only. `--save-fixture` writes that library to JSON, and `--fixture`
//! replays a saved one offline.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};

use shisho_services::affinity::AffinityRanker;
use shisho_services::eval::{self, EvalReport, Ranker, SnapshotOrder};
use shisho_services::library_snapshot::{LibrarySnapshot, SnapshotSeries};

pub async fn run(
    config_path: PathBuf,
    user: Option<String>,
    fixture: Option<PathBuf>,
    save_fixture: Option<PathBuf>,
) -> anyhow::Result<()> {
    let snapshot = match (fixture, user) {
        (Some(path), _) => load_fixture(&path)?,
        (None, Some(user)) => live_snapshot(&config_path, &user).await?,
        (None, None) => bail!("pass --user <id|email|codex-sub> or --fixture <path>"),
    };
    if let Some(path) = save_fixture {
        let json = serde_json::to_string_pretty(&snapshot.series)?;
        std::fs::write(&path, json).with_context(|| format!("writing {}", path.display()))?;
        println!("library saved to {}", path.display());
    }

    let rankers: Vec<Box<dyn Ranker>> = vec![Box::new(SnapshotOrder), Box::new(AffinityRanker)];
    let reports: Vec<EvalReport> = rankers
        .iter()
        .filter_map(|ranker| eval::evaluate(&snapshot, ranker.as_ref()))
        .collect();
    if reports.is_empty() {
        bail!("the library has no ratings to hold out; rate some series in Codex first");
    }
    print_table(&snapshot, &reports);
    Ok(())
}

fn load_fixture(path: &Path) -> anyhow::Result<LibrarySnapshot> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let series: Vec<SnapshotSeries> =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    Ok(LibrarySnapshot::from_series(series))
}

async fn live_snapshot(config_path: &Path, user: &str) -> anyhow::Result<LibrarySnapshot> {
    let cfg = shisho_config::load(config_path)
        .with_context(|| format!("loading config from {}", config_path.display()))?;
    super::init_tracing(&cfg);
    cfg.validate()?;
    let dek = shisho_db::keys::bootstrap_dek(&cfg)?
        .context("stored Codex keys are sealed with the data key; enable [encryption]")?;
    let db = shisho_db::connect(&cfg, Some(&dek)).await?;
    shisho_db::run_migrations(&db).await?;
    let services = shisho_api::build_services(&cfg, db.clone())
        .await?
        .context("the recommendation service is not configured (set codex.base_url)")?;
    let identity = shisho_api::build_codex_identity(&cfg).context("codex.base_url is unset")?;

    let account = shisho_db::repos::users::list_all(&db)
        .await?
        .into_iter()
        .find(|u| {
            u.id == user
                || u.email.as_deref() == Some(user)
                || u.codex_user_sub.as_deref() == Some(user)
        })
        .with_context(|| format!("no user matches {user:?}"))?;
    let ctx = identity
        .resolve(&db, &dek, &account.id)
        .await
        .with_context(|| format!("resolving the stored Codex key for {user:?}"))?;
    let snapshot = services
        .snapshots
        .get(&ctx.user_sub, &ctx.credential)
        .await
        .context("reading the library from Codex")?;
    Ok((*snapshot.value).clone())
}

fn print_table(snapshot: &LibrarySnapshot, reports: &[EvalReport]) {
    let untouched = snapshot
        .series
        .iter()
        .filter(|s| s.read_state == shisho_services::read_state::ReadState::Untouched)
        .count();
    println!(
        "library: {} series, {} untouched; {} favourites held out (top-quartile ratings)",
        snapshot.series.len(),
        untouched,
        reports[0].held_out
    );
    println!(
        "{:<18} {:>10} {:>10} {:>8} {:>10}",
        "ranker", "recall@10", "recall@40", "MRR", "rank frac"
    );
    for r in reports {
        println!(
            "{:<18} {:>10.3} {:>10.3} {:>8.3} {:>10.3}",
            r.ranker, r.recall_at_10, r.recall_at_40, r.mrr, r.mean_rank_fraction
        );
    }
}
