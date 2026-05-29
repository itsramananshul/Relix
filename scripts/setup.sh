#!/usr/bin/env bash
# scripts/setup.sh — Relix idempotent operator setup.
#
# Walks the operator through every optional API key Relix uses
# and writes the chosen values to the project-root `.env` file.
# Re-running the script keeps any value the operator does NOT
# overwrite — every prompt accepts a blank line to skip and
# offers to keep an existing value when it is already set.
#
# Sections:
#
#   1. Web search (RELIX-7.18 / GAP 17):
#        TAVILY_API_KEY | BRAVE_SEARCH_API_KEY | PERPLEXITY_API_KEY
#   2. Document parsing + web reading (GAP 10 PART 1 / 2):
#        LLAMA_CLOUD_API_KEY | JINA_API_KEY | FIRECRAWL_API_KEY
#   3. Screen capture (GAP 10 PART 3): toggles `RELIX_SCREEN_ENABLED`.
#
# Usage:
#   ./scripts/setup.sh
#
# Environment:
#   RELIX_ENV_FILE — override the target `.env` path (default:
#                    `<project-root>/.env`).

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" &> /dev/null && pwd)"
PROJECT_ROOT="$(cd -- "$SCRIPT_DIR/.." &> /dev/null && pwd)"
ENV_FILE="${RELIX_ENV_FILE:-$PROJECT_ROOT/.env}"

touch "$ENV_FILE"
chmod 600 "$ENV_FILE" 2>/dev/null || true

env_get() {
  local var="$1"
  if [[ -f "$ENV_FILE" ]]; then
    grep -E "^${var}=" "$ENV_FILE" 2>/dev/null | tail -n 1 | cut -d= -f2- || true
  fi
}

env_set() {
  local var="$1" value="$2"
  local tmp written=0
  tmp="$(mktemp)"
  if [[ -f "$ENV_FILE" ]]; then
    while IFS= read -r line || [[ -n "$line" ]]; do
      if [[ "$line" == "${var}="* ]]; then
        printf '%s=%s\n' "$var" "$value" >> "$tmp"
        written=1
      else
        printf '%s\n' "$line" >> "$tmp"
      fi
    done < "$ENV_FILE"
  fi
  if [[ "$written" -eq 0 ]]; then
    printf '%s=%s\n' "$var" "$value" >> "$tmp"
  fi
  mv "$tmp" "$ENV_FILE"
  chmod 600 "$ENV_FILE" 2>/dev/null || true
}

masked() {
  local v="$1"
  local n=${#v}
  if [[ $n -lt 8 ]]; then
    printf '****'
  else
    printf '%s...%s' "${v:0:4}" "${v: -4}"
  fi
}

prompt_secret() {
  local var="$1" label="$2" url="$3"
  local existing
  existing="$(env_get "$var")"
  if [[ -n "$existing" ]]; then
    printf '  %s already set (%s).\n' "$var" "$(masked "$existing")"
    read -r -p "    Enter a new value (or press Enter to keep): " value
    if [[ -z "$value" ]]; then
      echo "    kept existing value."
      return
    fi
  else
    echo "  $label"
    echo "    See: $url"
    read -r -s -p "    Enter key (or press Enter to skip): " value
    echo
    if [[ -z "$value" ]]; then
      echo "    skipped."
      return
    fi
  fi
  env_set "$var" "$value"
  echo "    wrote $var."
}

prompt_yes_no() {
  local var="$1" label="$2"
  local current
  current="$(env_get "$var")"
  local default_hint="[y/N]"
  if [[ "$current" == "true" ]]; then
    default_hint="[Y/n]"
  fi
  read -r -p "  $label $default_hint " ans
  case "${ans:-}" in
    y|Y|yes|YES) env_set "$var" "true"; echo "    enabled." ;;
    n|N|no|NO) env_set "$var" "false"; echo "    disabled." ;;
    "")
      if [[ "$current" == "true" ]]; then
        echo "    kept enabled."
      else
        env_set "$var" "false"
        echo "    kept disabled."
      fi
      ;;
    *)
      env_set "$var" "false"
      echo "    treated as no."
      ;;
  esac
}

cat <<'BANNER'
==============================================================
 Relix operator setup
--------------------------------------------------------------
 Walks every optional API key + feature toggle Relix supports.
 Press Enter at any prompt to skip; existing values are kept
 unless you overwrite them. The script writes
   .env (mode 600) at the project root.
==============================================================
BANNER

echo
echo "=== Web search (research-backed identity, GAP 17) ==="
echo
echo "Pick ONE of the three providers below — the controller"
echo "auto-selects the first non-empty key (Tavily → Brave →"
echo "Perplexity). Skipping every prompt keeps the cap dormant."
echo
prompt_secret TAVILY_API_KEY \
  "Tavily — research-tuned, generous free tier" \
  "https://tavily.com"
prompt_secret BRAVE_SEARCH_API_KEY \
  "Brave Search — privacy-first, pay-as-you-go" \
  "https://api.search.brave.com"
prompt_secret PERPLEXITY_API_KEY \
  "Perplexity — citation-rich answers" \
  "https://docs.perplexity.ai"

echo
echo "=== Document parsing and web reading (GAP 10) ==="
echo
echo "Cloud document parsing dramatically improves quality on"
echo "complex PDFs and rich web pages. Skipping every prompt"
echo "keeps the local-only tier (already on)."
echo
prompt_secret LLAMA_CLOUD_API_KEY \
  "LlamaParse — best-in-class scanned-PDF parsing" \
  "https://cloud.llamaindex.ai"
prompt_secret JINA_API_KEY \
  "Jina Reader — clean markdown extraction for URLs" \
  "https://jina.ai/reader"
prompt_secret FIRECRAWL_API_KEY \
  "Firecrawl — JS-rendered URL scraping" \
  "https://firecrawl.dev"

echo
echo "=== Screen capture (GAP 10 PART 3) ==="
echo
echo "Screen capture lets agents see the host's screen via"
echo "scrot / screencapture / PowerShell. Opt-in by design"
echo "because the cap reads the screen."
echo
prompt_yes_no RELIX_SCREEN_ENABLED \
  "Enable [tool.screen]?"

cat <<EOF

==============================================================
Done. Wrote selections to $ENV_FILE.

To activate the features whose keys you provided, set the
corresponding TOML sections in your controller config:

  [session_identity.research]
  enabled = true
  [session_identity.web_search]
  enabled  = true
  provider = "auto"

  [tool.parse_document]
  enabled       = true
  prefer_cloud  = true

  [tool.web_read]
  enabled      = true
  prefer_cloud = true

  [tool.screen]
  enabled = true   # iff you answered yes above

  [metrics.cost_alerts]
  enabled               = true
  baseline_window_mins  = 60
  spike_multiplier      = 2.0
  drift_threshold       = 0.3

==============================================================
EOF
