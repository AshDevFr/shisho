# Weekly recommendation report

The proactive loop: a scheduled, non-interactive `claude -p` session
connects to Shisho's MCP endpoint, runs the recommendation workflow, and
writes a Markdown report. Shisho stays a plain MCP server — the schedule,
the credential, and the LLM run all live outside it (host cron, systemd
timer, or a sidecar container).

Everything ships in [`scripts/weekly-report/`](../scripts/weekly-report/):

| File                          | Purpose                                          |
| ----------------------------- | ------------------------------------------------ |
| `run-weekly-report.sh`        | The cron payload: preflight, `claude -p`, report |
| `shisho-weekly-report.service`| Example systemd oneshot unit                     |
| `shisho-weekly-report.timer`  | Example systemd timer (Mon 07:00, persistent)    |
| `crontab.example`             | Plain-cron alternative                           |

The session instructions and report format are **not** a file here: they
are an embedded Jinja template in the binary (`weekly_report.md.j2`),
rendered at runtime by `shisho report prompt` and piped into `claude -p`.
There is no prompt copy to drift out of sync — edit the template, rebuild.

## Requirements

- [Claude Code](https://code.claude.com/docs) installed and authenticated
  for the user the schedule runs as (`claude -p` is the harness; Shisho
  contains none).
- The `shisho` binary on `PATH` (or set `SHISHO_BIN`) so the runner can
  render the prompt.
- A Codex API key for the user the report is for, with `series:read`,
  `libraries:read`, `books:read` and `progress:read` (see the README's
  permission table). The report only reads, so leave `ratings:write` and
  `want-to-read:write` off this key. **The cron environment
  holds the key, never Shisho** — Shisho resolves it per-request against
  Codex `/me` exactly like an interactive session.
- Network reach to the Shisho MCP endpoint (`https://<shisho>/mcp`).

## Setup

1. Create the environment file (mode `0600`):

   ```sh
   # /etc/shisho/weekly-report.env
   SHISHO_MCP_URL=https://shisho.example.com/mcp
   # Preferred: point at a 0600 file holding only the key.
   SHISHO_BEARER_FILE=/etc/shisho/weekly-report.key
   # Optional: where reports land (default ~/.local/share/shisho-reports).
   REPORT_DIR=/var/lib/shisho/reports
   ```

2. Install the schedule — either the systemd pair
   (`shisho-weekly-report.service` + `.timer`, adjust paths/user, then
   `systemctl enable --now shisho-weekly-report.timer`) or the
   `crontab.example` entry.

3. **Dry run** before trusting the schedule:

   ```sh
   . /etc/shisho/weekly-report.env && scripts/weekly-report/run-weekly-report.sh
   ```

   A healthy run prints the report path and exits 0. Check the report:
   tiered recommendations, open reconciliation questions, status notes,
   and (in Shisho) verdicts recorded under a fresh `weekly-<date>`
   session id — visible on the web console's Verdicts page.

## Delivery

The report is a Markdown file in `REPORT_DIR`
(`shisho-report-<date>.md`). File delivery is the primary mechanism;
forward it however the deployment likes (a webhook `curl` after the
script, an MTA wrapper, a synced directory). Keep the script's exit code
intact so the scheduler still sees failures.

## Failure semantics (never a silent week)

The script fails loud, by contract:

- **Preflight 401**: before spending an LLM run, it POSTs an MCP
  `initialize` with the bearer. Shisho answers a revoked/expired key with
  its distinct 401; the script writes `FAILED-<date>.md` into
  `REPORT_DIR` and exits non-zero, so the scheduler's failure
  notification fires.
- **Mid-session auth failure**: if the key dies during the run, Shisho's
  distinct message ("your Codex authorization expired or was revoked")
  appears in tool output; the script detects it, keeps the raw output in
  the failure marker, and exits non-zero.
- **Anything else** (`claude -p` non-zero, unreachable endpoint): marker +
  non-zero exit, with stderr in `REPORT_DIR/last-run.log`.

On the Shisho side the same rejection is visible without the script: the
Status page and the `status()` tool report the caller's minted-key state,
and a per-user surface (a recommendation tool, the taste-profile page)
returns the distinct re-authenticate error when the stored key no longer
validates. A healthy run leaves no failure residue (the day's marker is
removed on success).

### Key rotation

Generate a new Codex API key, replace the contents of
`SHISHO_BEARER_FILE` (or the env var), and dry-run the script once.
Nothing in Shisho needs to change — it never stored the old key; its
`/me` validation cache forgets the hash within its TTL.
