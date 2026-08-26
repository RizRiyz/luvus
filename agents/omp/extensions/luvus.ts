// luvus Oh My Pi (omp) integration extension.
//
// Auto-installed at ~/.omp/agent/extensions/luvus.ts by
// `luvus integration install omp`. omp's extension runner auto-discovers
// JS/TS factories under that directory, so this file loads on every omp
// launch with no agent config wiring.
//
// What it reports to luvus (same wire contract as the shipped claude /
// codex / copilot / kimi / grok hooks, so the same resume pipeline works):
//   • pane.report_session { agent: "omp", session_id } on session_start,
//     session_switch, and agent_start — enables `luvus agent resume` via
//     the `omp --resume <id>` flag.
//   • pane.report_event { agent: "omp", kind: "Stop" } on session_stop —
//     the main-session settle event. Child subagent sessions forward their
//     own agent_end/turn_end events for progress tracking; they never emit
//     session_stop, so a child completing cannot mark this pane done, and
//     the root emits Stop exactly once per settle.
//   • pane.report_event { kind: "Notification" } on tool approval requests
//     and ask-tool prompts while luvus shows the pane as blocked.
//
// Transport: every report spawns `<LUVUS_BIN_PATH> pane report …` (or
// `pane report-event …`). The CLI resolves the exact server socket from
// LUVUS_SOCKET_PATH — the same env luvus injects into this pane — so
// reports always reach the owning luvus session even when several luvus
// servers run side by side. No pipe enumeration, no wrong-server routing.
//
// The extension no-ops when LUVUS_ENV / LUVUS_PANE_ID are unset, so it is
// safe to leave installed on machines without luvus.

import type {
	ExtensionAPI,
	ExtensionContext,
} from "@oh-my-pi/pi-coding-agent";

const SOURCE = "luvus-hook";
const AGENT = "omp";
const PANE_ID = process.env.LUVUS_PANE_ID ?? "";
const ENABLED = process.env.LUVUS_ENV === "1" && PANE_ID !== "";

/** Ask-tool argument shape (the only tool input this extension inspects). */
interface AskToolArgs {
	questions?: Array<{ question?: string }>;
}

/** Shape of the `tool_approval_requested` event payload we read. */
interface ApprovalEvent {
	sessionId?: string;
	toolName?: string;
	reason?: string;
}

/** Shape of the `tool_execution_start` event payload we read. */
interface ToolExecutionStartEvent {
	toolCallId?: string;
	toolName?: string;
	args?: unknown;
}

/** Shape of the `session_stop` event payload we read. */
interface SessionStopEvent {
	session_id?: string;
	session_file?: string;
}

let lastSessionRef: string | undefined;

function binPath(): string {
	return process.env.LUVUS_BIN_PATH || "luvus";
}

/**
 * Spawn one report through the luvus CLI.
 *
 * The CLI targets this exact luvus session via LUVUS_SOCKET_PATH, so no
 * endpoint discovery happens here.
 *
 * Returns a Promise<boolean> indicating whether the CLI executed and exited
 * cleanly with code 0 (and was not killed by timeout). Failures never throw or reject.
 */
function makeSender(pi: ExtensionAPI): (args: string[]) => Promise<boolean> {
	return (args: string[]) => {
		if (!ENABLED) return Promise.resolve(false);
		let result: unknown;
		try {
			result = pi.exec(binPath(), args, { timeout: 1500 });
		} catch {
			return Promise.resolve(false);
		}
		return Promise.resolve(result).then(
			(res) => {
				const r = res as { code?: number; exitCode?: number; killed?: boolean } | undefined;
				const exit = r?.code ?? r?.exitCode;
				return exit === 0 && !r?.killed;
			},
			() => false,
		);
	};
}

function sessionRef(ctx: ExtensionContext): string | undefined {
	try {
		const id = ctx.sessionManager?.getSessionId?.();
		if (typeof id === "string" && id.length > 0) return id;
	} catch {
		// session manager unavailable in this context
	}
	// No session-file fallback: a file path is not a session id. If the
	// id is unavailable here, the next agent_start reports it — by then
	// getSessionId() is reliably populated.
	return undefined;
}

export function createLuvusExtension(pi: ExtensionAPI): void {
	if (!ENABLED) return;

	const sendReport = makeSender(pi);

	function reportSession(sessionRefValue: string | undefined): void {
		if (!sessionRefValue || sessionRefValue === lastSessionRef) return;
		lastSessionRef = sessionRefValue;
		void sendReport(["pane", "report", "--agent", AGENT, "--session", sessionRefValue]).then(
			(ok) => {
				// Compare-and-clear: only drop the marker if no newer session
				// report has committed since this one was sent.
				if (!ok && lastSessionRef === sessionRefValue) {
					lastSessionRef = undefined;
				}
			},
		);
	}

	function reportStop(): void {
		void sendReport(["pane", "report-event", "--agent", AGENT, "--kind", "Stop"]);
	}

	function reportNotification(message: string): void {
		void sendReport([
			"pane",
			"report-event",
			"--agent",
			AGENT,
			"--kind",
			"Notification",
			"--message",
			message.slice(0, 200),
		]);
	}

	pi.on("session_start", (_event, ctx) => {
		reportSession(sessionRef(ctx));
	});

	pi.on("session_switch", (_event, ctx) => {
		reportSession(sessionRef(ctx));
	});

	pi.on("agent_start", (_event, ctx) => {
		reportSession(sessionRef(ctx));
	});

	// Root completion ONLY. session_stop fires when the main agent settles;
	// child subagent sessions forward their own agent_end/turn_end but never
	// session_stop, so a child finishing cannot mark this pane done, and the
	// root cannot double-report Stop across turn boundaries.
	pi.on("session_stop", (_event: SessionStopEvent) => {
		reportStop();
	});

	pi.on("tool_approval_requested", (event: ApprovalEvent) => {
		const label = event.reason || `${event.toolName ?? "Tool"} approval`;
		reportNotification(label);
	});

	pi.on("tool_execution_start", (event: ToolExecutionStartEvent) => {
		if (event.toolName !== "ask") return;
		const args = event.args as AskToolArgs | undefined;
		const question = args?.questions?.find((q) => typeof q?.question === "string")
			?.question;
		reportNotification(question ?? "waiting for user input");
	});
}

// omp's extension loader accepts a module-as-function or `module.default`;
// a named-only export fails both checks and the file is skipped. This line
// is the load contract — do not remove.
export default createLuvusExtension;
