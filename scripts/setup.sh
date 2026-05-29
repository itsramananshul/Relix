#!/usr/bin/env bash
# scripts/setup.sh — RELIX-7.18 / GAP 17 PART 3
#
# Idempotent operator setup for the research-backed identity
# pipeline. Prompts for ONE of three web-search API keys and
# writes the chosen value to the project-root `.env` file.
#
# Re-running the script after a key is already present in `.env`
# leaves the existing value untouched unless the operator types
# a new value at the prompt.
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

PROVIDERS=("tavily" "brave" "perplexity")
ENV_VARS=("TAVILY_API_KEY" "BRAVE_SEARCH_API_KEY" "PERPLEXITY_API_KEY")
DESCRIPTIONS=(
  "Tavily (https://tavily.com — research-tuned, generous free tier)"
  "Brave Search (https://api.search.brave.com — privacy-first, pay-as-you-go)"
  "Perplexity (https://docs.perplexity.ai — citation-rich answers)"
)

cat <<'BANNER'
==============================================================
 Relix research-backed identity setup (RELIX-7.18 / GAP 17)
--------------------------------------------------------------
 Pick a search provider and paste its API key. The chosen key
 is written to .env at the project root. Re-running this
 script keeps any value you do not overwrite.
==============================================================
BANNER

echo
echo "Available providers:"
for i in "${!PROVIDERS[@]}"; do
  printf "  %d) %-11s — %s\n" "$((i + 1))" "${PROVIDERS[$i]}" "${DESCRIPTIONS[$i]}"
done

choice=""
while [[ -z "$choice" ]]; do
  read -r -p "Pick [1-3]: " choice
  if [[ ! "$choice" =~ ^[1-3]$ ]]; then
    echo "  please enter 1, 2, or 3"
    choice=""
  fi
done

idx=$((choice - 1))
provider="${PROVIDERS[$idx]}"
var="${ENV_VARS[$idx]}"

# If the env file already has a value for the chosen var,
# offer to keep it.
existing=""
if [[ -f "$ENV_FILE" ]]; then
  existing="$(grep -E "^${var}=" "$ENV_FILE" 2>/dev/null | tail -n 1 | cut -d= -f2- || true)"
fi

if [[ -n "$existing" ]]; then
  masked="${existing:0:4}…${existing: -4}"
  read -r -p "  $var already set ($masked). Replace? [y/N] " replace
  case "$replace" in
    y|Y|yes|YES)
      key=""
      ;;
    *)
      echo "  keeping existing value; nothing to do."
      exit 0
      ;;
  esac
else
  key=""
fi

while [[ -z "$key" ]]; do
  read -r -s -p "  Paste your $provider API key: " key
  echo
  if [[ -z "$key" ]]; then
    echo "  key cannot be empty"
  fi
done

# Touch the env file so the rest of the logic always operates
# on something writable.
touch "$ENV_FILE"
chmod 600 "$ENV_FILE" || true

tmp="$(mktemp)"
written=0
while IFS= read -r line || [[ -n "$line" ]]; do
  if [[ "$line" == "${var}="* ]]; then
    printf '%s=%s\n' "$var" "$key" >> "$tmp"
    written=1
  else
    printf '%s\n' "$line" >> "$tmp"
  fi
done < "$ENV_FILE"

if [[ "$written" -eq 0 ]]; then
  printf '%s=%s\n' "$var" "$key" >> "$tmp"
fi

mv "$tmp" "$ENV_FILE"
chmod 600 "$ENV_FILE" || true

echo
echo "Wrote $var to $ENV_FILE (mode 600)."
echo "Enable the pipeline by setting:"
echo
echo "  [session_identity.research]"
echo "  enabled = true"
echo
echo "  [session_identity.web_search]"
echo "  enabled  = true"
echo "  provider = \"auto\""
echo
echo "in your controller config TOML."
