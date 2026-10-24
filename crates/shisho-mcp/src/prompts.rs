//! Prompt and instruction text lives in embedded `.md.j2` templates, never
//! inline in Rust (PRD Part 7 / Principle 9): operator-facing copy stays
//! discoverable and editable without reading source.
//!
//! Templates are embedded at build time via `rust-embed` and rendered by a
//! shared, process-wide `minijinja` environment configured with
//! [`UndefinedBehavior::Strict`]. Strict rendering turns a missing context
//! variable into an error instead of a silent blank, so template/context
//! drift fails at test time rather than shipping a half-empty prompt.
//!
//! The rendering substrate (a strict `minijinja` env over the embedded tree)
//! plus the prompt router that serves the `recommend` and `consolidate`
//! prompts on the MCP server by rendering those templates per call.

use std::sync::LazyLock;

use minijinja::value::Serde;
use minijinja::{Environment, UndefinedBehavior};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{PromptMessage, Role};
use rmcp::{ErrorData, prompt, prompt_router};
use rust_embed::RustEmbed;
use serde::{Deserialize, Serialize};

use crate::McpService;

/// Template name for the recommendation-session prompt.
pub const RECOMMEND: &str = "recommend.md.j2";
/// Template name for the consolidation-session prompt.
pub const CONSOLIDATE: &str = "consolidate.md.j2";
/// Template name for the unattended weekly-report prompt (rendered for the
/// headless `claude -p` runner, not registered as an MCP prompt).
pub const WEEKLY_REPORT: &str = "weekly_report.md.j2";

/// The embedded `prompts/` tree (release-embedded; the files are the source
/// of truth for all prompt/instruction copy).
#[derive(RustEmbed)]
#[folder = "prompts/"]
struct PromptAssets;

/// A configured-but-empty strict environment. Factored out so the render
/// contract (undefined variables error) can be exercised in isolation.
fn strict_environment() -> Environment<'static> {
    let mut env = Environment::new();
    env.set_undefined_behavior(UndefinedBehavior::Strict);
    env
}

/// The process-wide environment, built once with every embedded template.
static ENV: LazyLock<Environment<'static>> = LazyLock::new(|| {
    let mut env = strict_environment();
    for name in PromptAssets::iter() {
        let file = PromptAssets::get(&name).expect("embedded template disappeared");
        let source =
            String::from_utf8(file.data.into_owned()).expect("prompt template must be UTF-8");
        env.add_template_owned(name.to_string(), source)
            .expect("embedded prompt template must parse");
    }
    env
});

#[derive(Debug, thiserror::Error)]
pub enum PromptError {
    /// No embedded template by that name.
    #[error("unknown prompt template {0:?}")]
    Unknown(String),
    /// The template exists but rendering failed (most often a missing
    /// variable under strict mode).
    #[error("rendering prompt template {template:?} failed")]
    Render {
        template: String,
        #[source]
        source: minijinja::Error,
    },
}

/// Render an embedded template by name with a serializable context. An
/// unknown template and a strict render failure are distinct typed errors.
pub fn render(template: &str, ctx: impl Serialize) -> Result<String, PromptError> {
    let tmpl = ENV
        .get_template(template)
        .map_err(|_| PromptError::Unknown(template.to_owned()))?;
    tmpl.render(Serde(&ctx))
        .map_err(|source| PromptError::Render {
            template: template.to_owned(),
            source,
        })
}

/// Optional session parameters for the `recommend` prompt. Both keys are
/// always present in the serialized context (as `null` when unset), so the
/// template's guards never touch an undefined variable under strict mode.
#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecommendContext {
    /// Pool to start from ("library" | "acquirable"); appended as a
    /// session directive when set.
    pub pool: Option<String>,
    /// Target number of final recommendations, appended when set.
    pub count: Option<u32>,
}

/// Empty context for the parameter-less `consolidate` prompt.
#[derive(Debug, Default, Serialize)]
pub struct ConsolidateContext {}

/// Render the recommendation-session prompt with the given session
/// parameters.
pub fn recommend(ctx: &RecommendContext) -> Result<String, PromptError> {
    render(RECOMMEND, ctx)
}

/// Render the consolidation-session prompt.
pub fn consolidate() -> Result<String, PromptError> {
    render(CONSOLIDATE, ConsolidateContext::default())
}

/// Context for the weekly-report prompt: the session id every verdict of the
/// run is grouped under (typically `weekly-<date>`). Field name matches the
/// template's `{{ session_id }}` (snake_case), so no serde rename.
#[derive(Debug, Serialize)]
pub struct WeeklyReportContext {
    pub session_id: String,
    /// The one pool this run covers (see [`weekly_pool`]).
    pub pool: String,
}

/// The weekly report covers one pool per run, alternating by ISO week (even:
/// the library backlog, odd: acquirable), so each run can verify its
/// finalists properly within the headless runner's turn budget.
pub fn weekly_pool(date: chrono::NaiveDate) -> &'static str {
    use chrono::Datelike;
    if date.iso_week().week().is_multiple_of(2) {
        "library"
    } else {
        "acquirable"
    }
}

/// Parse the `count` prompt argument. MCP prompt arguments are strings on the
/// wire, so the number is validated here rather than typed in the schema.
pub fn parse_count(raw: Option<String>) -> Result<Option<u32>, String> {
    match raw.as_deref().map(str::trim) {
        None | Some("") => Ok(None),
        Some(text) => match text.parse::<u32>() {
            Ok(n) if n > 0 => Ok(Some(n)),
            _ => Err(format!(
                "count must be a positive whole number, got {text:?}"
            )),
        },
    }
}

/// Render the unattended weekly-report prompt. Not an MCP prompt — the headless
/// `claude -p` runner renders it (via the `shisho report prompt` CLI) and pipes
/// it in, so the prompt stays data (a `.md.j2`), never an inline string in the
/// runner or in Rust.
pub fn weekly_report(session_id: &str, pool: &str) -> Result<String, PromptError> {
    render(
        WEEKLY_REPORT,
        WeeklyReportContext {
            session_id: session_id.to_owned(),
            pool: pool.to_owned(),
        },
    )
}

/// Optional session parameters accepted by the `recommend` prompt (mirrors
/// [`RecommendContext`] on the wire).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RecommendArgs {
    /// Pool to start from: "library" (default) or "acquirable".
    #[serde(default)]
    pub pool: Option<String>,
    /// How many final recommendations to aim for (a positive whole number).
    #[serde(default)]
    pub count: Option<String>,
}

/// The prompt router: renders each server-provided prompt from its embedded
/// template per call. No per-user data and no scope gating — prompts are static
/// workflow copy (parameterized only by the session args above).
#[prompt_router(vis = "pub(crate)")]
impl McpService {
    #[prompt(
        name = "recommend",
        description = "Run a recommendation session: one pool of pre-ranked candidates, verification of the finalists against the taste profile, tiered output, and a verdict logged for everything surfaced."
    )]
    async fn recommend_prompt(
        &self,
        Parameters(args): Parameters<RecommendArgs>,
    ) -> Result<Vec<PromptMessage>, ErrorData> {
        let count = parse_count(args.count).map_err(|msg| ErrorData::invalid_params(msg, None))?;
        let text = recommend(&RecommendContext {
            pool: args.pool,
            count,
        })
        .map_err(render_error)?;
        Ok(vec![PromptMessage::new_text(Role::User, text)])
    }

    #[prompt(
        name = "consolidate",
        description = "Consolidate recent reading feedback into a proposed taste-profile update via propose_profile_update."
    )]
    async fn consolidate_prompt(&self) -> Result<Vec<PromptMessage>, ErrorData> {
        let text = consolidate().map_err(render_error)?;
        Ok(vec![PromptMessage::new_text(Role::User, text)])
    }
}

/// A prompt-render failure is a server bug (a template/context contract broke),
/// not the caller's to fix: surface it as an internal error.
fn render_error(err: PromptError) -> ErrorData {
    tracing::error!(error = %err, "rendering an MCP prompt failed");
    ErrorData::internal_error(format!("rendering the prompt failed: {err}"), None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recommend_renders_the_body_without_session_parameters() {
        let out = recommend(&RecommendContext::default()).expect("render");
        assert!(out.contains("# Recommendation session"));
        assert!(out.contains("## Workflow"));
        assert!(out.contains("## Hard rules"));
        assert!(
            !out.contains("## Session parameters"),
            "no parameters => no session-parameters block: {out}"
        );
    }

    #[test]
    fn recommend_appends_session_parameters_when_set() {
        let out = recommend(&RecommendContext {
            pool: Some("acquirable".into()),
            count: Some(5),
        })
        .expect("render");
        assert!(out.contains("## Session parameters"));
        assert!(out.contains("- Start from the \"acquirable\" pool."));
        assert!(out.contains("- Aim for 5 final recommendations."));
    }

    #[test]
    fn recommend_includes_only_the_set_parameters() {
        let out = recommend(&RecommendContext {
            pool: Some("library".into()),
            count: None,
        })
        .expect("render");
        assert!(out.contains("## Session parameters"));
        assert!(out.contains("- Start from the \"library\" pool."));
        assert!(!out.contains("final recommendations"));
    }

    #[test]
    fn consolidate_renders_the_body() {
        let out = consolidate().expect("render");
        assert!(out.contains("# Consolidation session"));
        assert!(out.contains("## Workflow"));
        assert!(out.contains("propose_profile_update"));
    }

    #[test]
    fn prompts_weekly_pool_alternates_by_iso_week() {
        // 2026-10-12 is ISO week 42, 2026-10-19 is week 43.
        let week42 = chrono::NaiveDate::from_ymd_opt(2026, 10, 12).unwrap();
        let week43 = chrono::NaiveDate::from_ymd_opt(2026, 10, 19).unwrap();
        assert_eq!(weekly_pool(week42), "library");
        assert_eq!(weekly_pool(week43), "acquirable");
        let out = weekly_report("weekly-2026-10-19", weekly_pool(week43)).expect("render");
        assert!(out.contains("This week's pool: `acquirable`"), "{out}");
    }

    #[test]
    fn prompts_recommend_verifies_finalists_not_every_candidate() {
        let out = recommend(&RecommendContext::default()).expect("render");
        assert!(!out.to_lowercase().contains("verify every candidate"));
        assert!(out.contains("because"));
        assert!(out.contains("finalists"));
    }

    #[test]
    fn prompts_count_must_be_a_positive_integer() {
        assert_eq!(parse_count(Some("5".into())).unwrap(), Some(5));
        assert_eq!(parse_count(None).unwrap(), None);
        assert!(parse_count(Some("five".into())).is_err());
        assert!(parse_count(Some("0".into())).is_err());
    }

    #[test]
    fn weekly_report_renders_with_the_session_id_injected() {
        let out = weekly_report("weekly-2026-06-15", "library").expect("render");
        assert!(out.contains("# Shisho weekly recommendation report"));
        assert!(out.contains("## Report format"));
        // The session id is interpolated everywhere the template references it.
        assert!(out.contains("weekly-2026-06-15"));
        // And there's no unresolved template variable left behind.
        assert!(!out.contains("{{"), "unrendered variable remains: {out}");
    }

    #[test]
    fn an_unknown_template_is_a_typed_error() {
        let err = render("nope.md.j2", RecommendContext::default()).expect_err("unknown");
        assert!(matches!(err, PromptError::Unknown(name) if name == "nope.md.j2"));
    }

    #[test]
    fn the_strict_env_makes_a_missing_variable_an_error() {
        // `render` shares this exact configuration, so a template that
        // prints an unbound variable surfaces as PromptError::Render rather
        // than a silent blank — context drift fails at test time.
        let mut env = strict_environment();
        env.add_template("probe", "value: {{ required }}")
            .expect("probe parses");
        let err = env
            .get_template("probe")
            .unwrap()
            .render(minijinja::context! {})
            .expect_err("undefined variable must error");
        assert_eq!(err.kind(), minijinja::ErrorKind::UndefinedError);
    }
}
