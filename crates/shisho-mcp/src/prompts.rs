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
//! This module is only the rendering substrate; wiring the `recommend` and
//! `consolidate` prompts onto the MCP server is the prompt-router's job.

use std::sync::LazyLock;

use minijinja::{Environment, UndefinedBehavior, Value};
use rust_embed::RustEmbed;
use serde::Serialize;

/// Template name for the recommendation-session prompt.
pub const RECOMMEND: &str = "recommend.md.j2";
/// Template name for the consolidation-session prompt.
pub const CONSOLIDATE: &str = "consolidate.md.j2";

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
    tmpl.render(Value::from_serialize(&ctx))
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
    pub count: Option<String>,
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
            count: Some("5".into()),
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
