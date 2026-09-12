#!/bin/sh
# Luvus Antigravity CLI integration. Antigravity passes the hook JSON through
# stdin; the Luvus binary parses it with a bounded reader and returns JSON.

if [ "${LUVUS_ENV:-}" != "1" ] || [ -z "${LUVUS_SOCKET_PATH:-}" ] || [ -z "${LUVUS_PANE_ID:-}" ]; then
  printf '{}\n'
  exit 0
fi

luvus_bin="${LUVUS_BIN_PATH:-luvus}"
"$luvus_bin" integration hook antigravity 2>/dev/null || printf '{}\n'
