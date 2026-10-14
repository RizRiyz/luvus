#!/bin/sh
# Luvus Codex integration. Codex passes one SessionStart or UserPromptSubmit
# JSON payload on stdin, and the Luvus binary reports the session to the pane
# running it (`luvus integration hook codex`).
#
# The binary path is fixed when the integration is installed. Codex can run
# hooks from its shared background server, which keeps the environment of
# whichever pane first started it, so an inherited LUVUS_BIN_PATH may name an
# unrelated build.
luvus_bin=__LUVUS_BIN__
if [ ! -x "$luvus_bin" ]; then
  luvus_bin="$(command -v luvus 2>/dev/null)" || exit 0
fi
"$luvus_bin" integration hook codex >/dev/null 2>&1 || true
exit 0
