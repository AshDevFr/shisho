#!/usr/bin/env bash
# Scheduled, unattended Shisho recommendation run: connects `claude -p` to
# the Shisho MCP endpoint with a static bearer, runs the weekly prompt, and
# writes the report to the report directory.
#
# The credential lives in the cron environment, never in Shisho. Failure is
# loud by design: any auth failure exits non-zero (so the host scheduler's
# failure notification fires) and leaves a FAILED-<date>.md marker in the
# report directory. A healthy run leaves no failure residue.
#
# The prompt is not stored here: it is rendered at runtime from Shisho's
# embedded Jinja template via `shisho report prompt`, so there is no prompt
# text to drift out of sync in this script.
#
# Environment:
#   SHISHO_MCP_URL       MCP endpoint, e.g. https://shisho.example.com/mcp (required)
#   SHISHO_BEARER        Codex API key of the report user (required, or:)
#   SHISHO_BEARER_FILE   path to a file containing the key (0600, preferred)
#   SHISHO_BIN           shisho binary that renders the prompt (default: shisho)
#   REPORT_DIR           report sink (default: ~/.local/share/shisho-reports)
#   CLAUDE_BIN           claude binary (default: claude)
#   CLAUDE_MODEL         optional --model override
#   MAX_TURNS            agentic turn budget (default: 50)

set -euo pipefail

: "${SHISHO_MCP_URL:?SHISHO_MCP_URL is required (e.g. https://shisho.example.com/mcp)}"
if [[ -z "${SHISHO_BEARER:-}" && -n "${SHISHO_BEARER_FILE:-}" ]]; then
  SHISHO_BEARER="$(<"${SHISHO_BEARER_FILE}")"
fi
: "${SHISHO_BEARER:?SHISHO_BEARER (or SHISHO_BEARER_FILE) is required}"

REPORT_DIR="${REPORT_DIR:-${HOME}/.local/share/shisho-reports}"
mkdir -p "${REPORT_DIR}"
date_tag="$(date +%F)"
report="${REPORT_DIR}/shisho-report-${date_tag}.md"
marker="${REPORT_DIR}/FAILED-${date_tag}.md"
run_log="${REPORT_DIR}/last-run.log"

fail() {
  local reason="$1"
  {
    echo "# Shisho weekly report FAILED (${date_tag})"
    echo
    echo "${reason}"
  } >"${marker}"
  echo "shisho weekly report failed: ${reason}" >&2
  exit 1
}

# Render the prompt from the embedded template (session id = weekly-<date>).
prompt="$("${SHISHO_BIN:-shisho}" report prompt --session "weekly-${date_tag}")" \
  || fail "could not render the weekly-report prompt via '${SHISHO_BIN:-shisho} report prompt'"

# Preflight: verify the credential against the MCP endpoint before spending
# an LLM run. Shisho answers a bad key with a distinct 401.
http_status="$(
  curl -sS -o /dev/null -w '%{http_code}' -X POST "${SHISHO_MCP_URL}" \
    -H "Authorization: Bearer ${SHISHO_BEARER}" \
    -H 'Content-Type: application/json' \
    -H 'Accept: application/json, text/event-stream' \
    --data '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"shisho-weekly-report-preflight","version":"0"}}}'
)" || fail "could not reach the Shisho MCP endpoint at ${SHISHO_MCP_URL}"

if [[ "${http_status}" == "401" ]]; then
  fail "Shisho rejected the cron credential (HTTP 401): the Codex API key has expired or was revoked. Rotate the key in this cron environment (SHISHO_BEARER / SHISHO_BEARER_FILE)."
fi
case "${http_status}" in
  2*) ;;
  *) fail "unexpected HTTP ${http_status} from the MCP endpoint during preflight" ;;
esac

mcp_config="$(printf '{"mcpServers":{"shisho":{"type":"http","url":"%s","headers":{"Authorization":"Bearer %s"}}}}' \
  "${SHISHO_MCP_URL}" "${SHISHO_BEARER}")"

tmp="$(mktemp)"
trap 'rm -f "${tmp}"' EXIT

if ! "${CLAUDE_BIN:-claude}" -p \
  --mcp-config "${mcp_config}" \
  --strict-mcp-config \
  --allowedTools "mcp__shisho__*" "WebSearch" "WebFetch" \
  --max-turns "${MAX_TURNS:-50}" \
  ${CLAUDE_MODEL:+--model "${CLAUDE_MODEL}"} \
  <<<"${prompt}" >"${tmp}" 2>"${run_log}"; then
  fail "claude -p exited non-zero; see ${run_log}"
fi

# In-band auth failure: a mid-session Codex 401 reaches tool output as the
# distinct revoked-key message. A report built on it is not a report.
if grep -qi "expired or was revoked" "${tmp}"; then
  cp "${tmp}" "${marker}"
  fail "the run hit Codex auth failures mid-session; raw output kept in ${marker}"
fi

mv "${tmp}" "${report}"
trap - EXIT
# A retried healthy run clears the day's failure marker.
rm -f "${marker}"
echo "shisho weekly report written to ${report}"
