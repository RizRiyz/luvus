# Luvus Codex integration for Windows. Codex passes one SessionStart or
# UserPromptSubmit JSON payload on stdin, and the Luvus binary reports the
# session to the pane running it (`luvus integration hook codex`).
#
# The binary path is fixed when the integration is installed. Codex can run
# hooks from its shared background server, which keeps the environment of
# whichever pane first started it, so an inherited LUVUS_BIN_PATH may name an
# unrelated build.
$luvus = __LUVUS_BIN__
if (-not (Test-Path -LiteralPath $luvus -PathType Leaf)) {
    $luvus = "luvus"
}
try {
    & $luvus integration hook codex *> $null
} catch {
    # Hooks are optional telemetry into the local Luvus server. Never interrupt
    # Codex when the server or installed binary is unavailable.
}
exit 0
