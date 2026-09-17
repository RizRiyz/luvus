#!/bin/sh
# Luvus Letta Code integration. Letta passes one SessionStart JSON payload on
# stdin. The Luvus binary reads it with a strict bound and reports only the
# exact conversation identity to the inherited owner-local server.

if [ "${LUVUS_ENV:-}" != "1" ] || [ -z "${LUVUS_SOCKET_PATH:-}" ] || [ -z "${LUVUS_PANE_ID:-}" ]; then
  exit 0
fi

luvus_bin="${LUVUS_BIN_PATH:-luvus}"
"$luvus_bin" integration hook letta >/dev/null 2>&1 || true
exit 0
