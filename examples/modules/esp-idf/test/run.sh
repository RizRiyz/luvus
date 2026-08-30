#!/bin/sh
# Exercise the module against a throwaway luvus server and a fake idf.py, so the
# whole flow is testable with no ESP32 attached.
#
#   sh test/run.sh /path/to/luvus
set -eu
BIN="${1:-luvus}"
case "$BIN" in
  */*) BIN="$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")" ;;
esac
TEST_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/luvus-esp-test.XXXXXX")
HOME_DIR="$TEST_ROOT/home"
LOG="$TEST_ROOT/idf-calls.log"
PROJECT="$TEST_ROOT/project"
STATE="$TEST_ROOT/state"
FAIL_FLAG="$TEST_ROOT/idf-fake-fail"
SERVER_LOG="$TEST_ROOT/server.log"
SERVER_PID=
FAKE="$(cd "$(dirname "$0")/fake-idf" && pwd)"
R() {
  env -u LUVUS_SOCKET_PATH -u LUVUS_PANE_ID -u LUVUS_SESSION \
    LUVUS_HOME="$HOME_DIR" IDF_FAKE_LOG="$LOG" IDF_FAKE_FAIL="$FAIL_FLAG" "$@"
}
ok() { printf '  \033[32mPASS\033[0m %s\n' "$1"; }
no() { printf '  \033[31mFAIL\033[0m %s\n' "$1"; FAILED=1; }
FAILED=0

cleanup() {
  status=$?
  trap - EXIT HUP INT TERM
  if [ -n "$SERVER_PID" ] && kill -0 "$SERVER_PID" 2>/dev/null; then
    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
  fi
  rm -rf "$TEST_ROOT"
  exit "$status"
}
trap cleanup EXIT HUP INT TERM

mkdir -p "$HOME_DIR" "$PROJECT" "$STATE"
: > "$LOG"
(
  cd "$PROJECT"
  exec env -u LUVUS_SOCKET_PATH -u LUVUS_PANE_ID -u LUVUS_SESSION \
    LUVUS_HOME="$HOME_DIR" IDF_FAKE_LOG="$LOG" IDF_FAKE_FAIL="$FAIL_FLAG" \
    "$BIN" server
) >"$SERVER_LOG" 2>&1 &
SERVER_PID=$!
sleep 2

R "$BIN" module link "$(cd "$(dirname "$0")/.." && pwd)" >/dev/null
R "$BIN" module settings example.esp-idf idf_path "$FAKE" >/dev/null
R "$BIN" module settings example.esp-idf port /dev/ttyFAKE0 >/dev/null

R "$BIN" module list | grep -q '"example.esp-idf"' && ok "module registers" || no "module registers"

# `[[startup]]` runs asynchronously after link, so give the dock push a moment
# to land before asserting on it.
sleep 2
R "$BIN" ui dock list | grep -q '"esp-idf"' && ok "the single ESP-IDF dock mounts" || no "ESP-IDF dock did not mount"


: > "$LOG"; R "$BIN" module run example.esp-idf build >/dev/null 2>&1; sleep 2
grep -q "build" "$LOG" && ok "build reaches idf.py with -p/-b" || no "build reaches idf.py"

: > "$LOG"; R "$BIN" module run example.esp-idf monitor >/dev/null 2>&1; sleep 4
a=$(R "$BIN" pane read 1 | grep -o 'boot log line [0-9]*' | tail -1); sleep 2
b=$(R "$BIN" pane read 1 | grep -o 'boot log line [0-9]*' | tail -1)
[ "$a" != "$b" ] && ok "monitor really holds the pane" || no "monitor does not stream"

R "$BIN" module run example.esp-idf flash >/dev/null 2>&1; sleep 5
grep -q MONITOR-INTERRUPTED "$LOG" && ok "flash stops the monitor (Ctrl+C)" || no "monitor not interrupted"
[ "$(grep -c 'IDFCALL.*monitor' "$LOG")" -ge 2 ] && ok "monitor reopens after a good flash" || no "monitor did not reopen"
R "$BIN" pane read 1 | grep -q 'boot log line' && ok "pre-flash log survives" || no "scrollback lost"

# A failing flash must NOT bring the monitor back over the error.
: > "$LOG"; R "$BIN" module run example.esp-idf monitor >/dev/null 2>&1; sleep 3
touch "$FAIL_FLAG"
R "$BIN" module run example.esp-idf flash >/dev/null 2>&1 || true
sleep 5
rm -f "$FAIL_FLAG"
# The log was cleared after the monitor started, so a correct run records the
# flash and no further `monitor` invocation.
[ "$(grep -c 'IDFCALL.*monitor' "$LOG")" -eq 0 ] && ok "failed flash leaves the error visible" \
  || no "monitor restarted over a failed flash"

# ── expanding command groups ─────────────────────────────────────────────────
# A child row carries its subcommand as the row value, so one `run` action backs
# every entry in a group. Simulate the env a child click sets and check the
# variant really reaches idf.py -- otherwise every child would run `build`.
: > "$LOG"
R env LUVUS_MODULE_ROW_VALUE=app LUVUS_MODULE_ACTION_ID=run \
  LUVUS_MODULE_STATE_DIR="$STATE" LUVUS_PANE_ID=1 \
  LUVUS_SETTING_IDF_PATH="$FAKE" LUVUS_SETTING_PORT=/dev/ttyFAKE0 \
  LUVUS_BIN_PATH="$BIN" sh idf.sh >/dev/null 2>&1 || true
sleep 2
grep -q "IDFCALL.* app" "$LOG" && ok "a child row's value picks the subcommand" \
  || no "child row value did not reach idf.py (got: $(tail -1 "$LOG"))"

# Clicking a board makes it the active port. The dock is the only way to pick a
# port without editing settings by hand, so this is the one board interaction.
#
# These invoke the scripts directly rather than via `module run`: the row value
# reaches a real click through the *server*, which spawns the action with its own
# environment -- so `env VAR=x luvus module run ...` would set the variable on
# the CLI process and never reach the script.
RUN() { R env LUVUS_MODULE_STATE_DIR="$STATE" LUVUS_BIN_PATH="$BIN" \
          LUVUS_SETTING_IDF_PATH="$FAKE" "$@"; }

RUN LUVUS_MODULE_ROW_VALUE=/dev/ttyOTHER sh select-device.sh >/dev/null 2>&1 || true
sleep 1
R "$BIN" module settings example.esp-idf port 2>/dev/null | grep -q ttyOTHER \
  && ok "clicking a board selects its port" \
  || no "board click did not change the port setting"
R "$BIN" module settings example.esp-idf port /dev/ttyFAKE0 >/dev/null 2>&1 || true

# Expanding a group is a toggle: the same row opens and closes it.
RUN LUVUS_MODULE_ROW_VALUE=flash sh toggle.sh >/dev/null 2>&1 || true
[ "$(cat "$STATE/expanded" 2>/dev/null)" = flash ] \
  && ok "clicking a group expands it" || no "group did not expand"

# The collapse check is only meaningful once the expand above really wrote the
# file -- assert it existed first, so a broken expand can't make this pass.
if [ -f "$STATE/expanded" ]; then
  RUN LUVUS_MODULE_ROW_VALUE=flash sh toggle.sh >/dev/null 2>&1 || true
  [ ! -f "$STATE/expanded" ] \
    && ok "clicking it again collapses it" || no "group did not collapse"
else
  no "collapse untestable: nothing was expanded"
fi

# The chip row must show what the *project* is configured for, not the module
# setting -- it used to print the setting's arbitrary `esp32s3` default even for
# an esp32 project, which is a guess presented as fact.
printf 'CONFIG_IDF_TARGET="esp32"\n' > "$PROJECT/sdkconfig"
CHIP=$(RUN LUVUS_BIN_PATH=/bin/echo LUVUS_SETTING_TARGET=esp32s3 \
        LUVUS_WORKSPACE_CWD="$PROJECT" sh dock.sh 2>/dev/null || true)
case "$CHIP" in
  *"chip · esp32 → esp32s3"*) ok "chip row reports the project's real target" ;;
  *) no "chip row did not reflect sdkconfig (got: $(printf '%s' "$CHIP" | head -c 120))" ;;
esac
rm -f "$PROJECT/sdkconfig"

# An open group emits its children. `dock.sh` pipes its rows straight into
# `luvus ui dock push`, so point LUVUS_BIN_PATH at `echo` to read the JSON it
# would have pushed (the later assignment wins over RUN's).
RUN LUVUS_MODULE_ROW_VALUE=build sh toggle.sh >/dev/null 2>&1 || true
OUT=$(RUN LUVUS_BIN_PATH=/bin/echo sh dock.sh 2>/dev/null || true)
case "$OUT" in
  *"full clean"*) ok "an open group lists its children" ;;
  *)              no "open group did not emit children" ;;
esac
# The partition editor must explain itself rather than dying, when the node is
# not an ESP-IDF project (the `exec` bug that blinked a tab out of existence).
R "$BIN" module run example.esp-idf edit-partitions >/dev/null 2>&1; sleep 3
P=$(R "$BIN" module log example.esp-idf 2>/dev/null | sed -n 's/.*"pane": "\([0-9]*\)".*/\1/p' | head -1)
R "$BIN" pane read "$P" 2>/dev/null | grep -q "Not an ESP-IDF project" \
  && ok "partition editor explains itself instead of vanishing" \
  || no "partition editor pane did not report why it stopped"

R "$BIN" server stop >/dev/null 2>&1 || true
wait "$SERVER_PID" 2>/dev/null || true
SERVER_PID=
[ "$FAILED" = 0 ] && echo "  all good" || { echo "  some checks failed"; exit 1; }
