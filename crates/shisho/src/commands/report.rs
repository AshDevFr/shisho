//! `shisho report prompt`: render the unattended weekly-report prompt from its
//! embedded Jinja template and print it to stdout, so the headless
//! `claude -p` runner can pipe it in. The prompt is data (a `.md.j2`), rendered
//! at runtime — never an inline string in the runner or in Rust.

use anyhow::Context;

/// Render the weekly-report prompt with the given session id (defaults to
/// `weekly-<today>` when none is passed) and this week's pool, and print it. This command does not
/// touch the database or the network — it only renders the template.
pub fn prompt(session_id: Option<String>) -> anyhow::Result<()> {
    let today = chrono::Utc::now().date_naive();
    let session_id = session_id.unwrap_or_else(|| format!("weekly-{}", today.format("%Y-%m-%d")));
    let pool = shisho_mcp::prompts::weekly_pool(today);
    let rendered = shisho_mcp::prompts::weekly_report(&session_id, pool)
        .context("rendering the weekly-report prompt")?;
    print!("{rendered}");
    Ok(())
}
