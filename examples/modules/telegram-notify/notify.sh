#!/bin/sh
# Telegram Notify -- sends a Telegram message when a pane's agent status
# changes, with a right-click "Send a test message" action.
#
# luvus runs this as a plain subprocess; everything arrives in the environment:
#   LUVUS_SETTING_BOT_TOKEN / LUVUS_SETTING_CHAT_ID / LUVUS_SETTING_NOTIFY_ON
#   LUVUS_MODULE_EVENT_JSON   event payload (agent/status/pane/project/cwd)
#   LUVUS_PANE_AGENT / LUVUS_PANE_STATUS / LUVUS_PANE_ID   flat fallbacks
#   LUVUS_WORKSPACE_CWD       the FOCUSED workspace -- fallback only, because
#                             the event payload describes the pane that changed
#
# stderr lands in `luvus module log`, which is the module's only failure
# channel (no toasts, no retries). The bot token never appears in argv (the
# request URL is handed to curl through its stdin config) and never in a log
# line: curl's stderr is discarded wholesale because --show-error would print
# the token-bearing URL on transport errors.
#
# POSIX sh + curl only; no other interpreter or tool is needed.

# Escape the two characters that matter inside a JSON string (house pattern,
# file-tree/lib.sh). Command substitution eats the trailing newline, which is
# exactly what we want here.
json_esc() { printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g'; }

# First "key": "value" string field of a luvus-produced JSON payload, via
# bounded sed. Empty output means the key is absent or its value is an empty
# string; callers treat both as "missing" and fall back.
json_field() {
  printf '%s' "$2" |
    sed -n 's/.*"'"$1"'"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' |
    sed -n '1p'
}

# Basename of a non-empty path; empty otherwise.
path_tail() {
  [ -n "$1" ] || return 0
  basename "$1"
}

# Send one message ($1) to the configured chat. One attempt, no retry.
# Failure reporting uses only the curl exit code or the HTTP status -- never
# curl's own output, which embeds the token.
send() {
  body="{\"chat_id\":\"$(json_esc "$chat_id")\",\"text\":\"$(json_esc "$1")\"}"
  http_code=$(printf 'url = "%s"\n' "$api_url" |
    curl -sS --max-time 10 -o /dev/null -w '%{http_code}' \
      -H 'Content-Type: application/json' \
      --data-binary "$body" -K - 2>/dev/null)
  curl_rc=$?
  if [ "$curl_rc" -ne 0 ]; then
    printf 'telegram-notify: send failed (curl exit %s)\n' "$curl_rc" >&2
    return 1
  fi
  case "$http_code" in
    2*) return 0 ;;
    *)
      printf 'telegram-notify: send failed (HTTP %s)\n' "$http_code" >&2
      return 1
      ;;
  esac
}

bot_token="${LUVUS_SETTING_BOT_TOKEN:-}"
chat_id="${LUVUS_SETTING_CHAT_ID:-}"
notify_on="${LUVUS_SETTING_NOTIFY_ON:-blocked}"
api_url="https://api.telegram.org/bot${bot_token}/sendMessage"
test_only=false
[ "${1:-}" = "--test" ] && test_only=true

# --- event identity ------------------------------------------------------
# Prefer the event payload -- it describes the pane that actually changed --
# over the flat vars, which can describe the pane in focus. Absent payload,
# unparseable JSON, or empty fields fall back to the flat vars / focused
# workspace (luvus builds payload fields with unwrap_or_default(), so a
# present-but-empty value is possible).
payload="${LUVUS_MODULE_EVENT_JSON:-}"
agent=""
status=""
pane=""
project=""
cwd=""
if [ -n "$payload" ]; then
  agent=$(json_field agent "$payload")
  status=$(json_field status "$payload")
  pane=$(json_field pane "$payload")
  project=$(json_field project "$payload")
  cwd=$(json_field cwd "$payload")
fi
[ -n "$agent" ] || agent="${LUVUS_PANE_AGENT:-agent}"
[ -n "$status" ] || status="${LUVUS_PANE_STATUS:-unknown}"
[ -n "$pane" ] || pane="${LUVUS_PANE_ID:-?}"

# Workspace token: the changed pane's project name, then its cwd, and only
# then the focused workspace's cwd. Never empty -- the message must be able
# to name where it happened even with everything unset.
workspace="$project"
[ -n "$workspace" ] || workspace=$(path_tail "$cwd")
[ -n "$workspace" ] || workspace=$(path_tail "${LUVUS_WORKSPACE_CWD:-}")
[ -n "$workspace" ] || workspace="luvus"

# --- what to send --------------------------------------------------------
if [ "$test_only" = true ]; then
  message="luvus telegram-notify: test message (pane ${pane} in ${workspace})"
else
  # Fail-closed gating: only the selected statuses notify. An unexpected
  # notify_on value stays silent rather than notifying on everything.
  case "$notify_on" in
    both)
      [ "$status" = blocked ] || [ "$status" = done ] || exit 0
      ;;
    blocked) [ "$status" = blocked ] || exit 0 ;;
    done)    [ "$status" = done ]    || exit 0 ;;
    *) exit 0 ;;
  esac
  message="${agent} is ${status} in ${workspace}"
fi

# --- settings guard (checked only once a send is actually attempted) ------
if [ -z "$bot_token" ]; then
  printf 'telegram-notify: bot_token is not set -- add it in Settings > Modules\n' >&2
  exit 1
fi
if [ -z "$chat_id" ]; then
  printf 'telegram-notify: chat_id is not set -- add it in Settings > Modules\n' >&2
  exit 1
fi

send "$message"
