// Luvus-managed Oh My Pi (OMP) integration.
// Reinstalling or updating the integration overwrites this file; place custom
// extensions beside it rather than editing it.

import type {
	ExtensionAPI,
	ExtensionContext,
} from "@oh-my-pi/pi-coding-agent";

const AGENT = "omp";
const SOURCE = "omp/extension";
const REPORT_TTL_SECONDS = "300";
const PANE_ID = process.env.LUVUS_PANE_ID ?? "";
const ENABLED =
	process.env.LUVUS_ENV === "1" &&
	PANE_ID !== "" &&
	(process.env.LUVUS_SOCKET_PATH ?? "") !== "";

type AgentState = "idle" | "working" | "blocked" | "done";

interface AskToolArgs {
	questions?: Array<{ question?: string }>;
}

interface ApprovalEvent {
	toolCallId?: string;
	toolName?: string;
	reason?: string;
}

interface ToolEvent {
	toolCallId?: string;
	toolName?: string;
	args?: unknown;
}

let commandQueue: Promise<boolean> = Promise.resolve(true);
let sequence = Date.now() * 1000;

function binPath(): string {
	return process.env.LUVUS_BIN_PATH || "luvus";
}

function execute(pi: ExtensionAPI, args: string[]): Promise<boolean> {
	if (!ENABLED) return Promise.resolve(false);
	try {
		return Promise.resolve(pi.exec(binPath(), args, { timeout: 1500 })).then(
			(result) => result.code === 0 && !result.killed,
			() => false,
		);
	} catch {
		return Promise.resolve(false);
	}
}

function enqueue(pi: ExtensionAPI, args: string[]): Promise<boolean> {
	commandQueue = commandQueue.then(
		() => execute(pi, args),
		() => execute(pi, args),
	);
	return commandQueue;
}

function sessionId(ctx: ExtensionContext): string | undefined {
	try {
		const id = ctx.sessionManager?.getSessionId?.();
		return typeof id === "string" && id.length > 0 && id.length <= 512
			? id
			: undefined;
	} catch {
		return undefined;
	}
}

function boundedText(value: string): string {
	return Array.from(value).slice(0, 200).join("");
}

function firstQuestion(args: unknown): string {
	const questions = (args as AskToolArgs | undefined)?.questions;
	return (
		questions?.find((question) => typeof question?.question === "string")
			?.question ?? "waiting for user input"
	);
}

export function createLuvusExtension(pi: ExtensionAPI): void {
	if (!ENABLED) return;

	let rootSession = false;
	let active = false;
	let settled = false;
	let currentSessionId: string | undefined;
	let lastReportKey: string | undefined;
	let blockedMessage: string | undefined;
	const approvals = new Set<string>();
	const asks = new Set<string>();

	function activateRoot(ctx: ExtensionContext): boolean {
		if (ctx.hasUI !== true) return false;
		rootSession = true;
		currentSessionId = sessionId(ctx) ?? currentSessionId;
		return true;
	}

	function desiredState(): AgentState {
		if (approvals.size > 0 || asks.size > 0) return "blocked";
		if (active) return "working";
		return settled ? "done" : "idle";
	}

	function publish(force = false): void {
		if (!rootSession) return;
		const state = desiredState();
		const message = state === "blocked" ? blockedMessage : undefined;
		const key = `${state}\u0000${message ?? ""}\u0000${currentSessionId ?? ""}`;
		if (!force && key === lastReportKey) return;
		lastReportKey = key;
		sequence += 1;
		const args = [
			"agent",
			"report",
			PANE_ID,
			"--source",
			SOURCE,
			"--kind",
			AGENT,
			"--status",
			state,
			"--sequence",
			String(sequence),
			"--ttl",
			REPORT_TTL_SECONDS,
		];
		if (message) args.push("--message", message);
		if (currentSessionId) args.push("--session", currentSessionId);
		void enqueue(pi, args).then((ok) => {
			if (!ok && lastReportKey === key) lastReportKey = undefined;
		});
	}

	function hook(kind: string, message?: string): void {
		const args = [
			"pane",
			"report-event",
			"--agent",
			AGENT,
			"--kind",
			kind,
		];
		if (message) args.push("--message", boundedText(message));
		void enqueue(pi, args);
	}

	function reset(ctx: ExtensionContext): boolean {
		if (!activateRoot(ctx)) return false;
		active = false;
		settled = false;
		blockedMessage = undefined;
		approvals.clear();
		asks.clear();
		lastReportKey = undefined;
		return true;
	}

	pi.on("session_start", (_event, ctx) => {
		if (reset(ctx)) publish(true);
	});

	pi.on("session_switch", (_event, ctx) => {
		if (reset(ctx)) publish(true);
	});

	pi.on("agent_start", (_event, ctx) => {
		if (!activateRoot(ctx)) return;
		currentSessionId = sessionId(ctx) ?? currentSessionId;
		active = true;
		settled = false;
		publish();
	});

	pi.on("tool_approval_requested", (event: ApprovalEvent, ctx) => {
		if (!activateRoot(ctx)) return;
		const id = event.toolCallId ?? `approval:${approvals.size}`;
		approvals.add(id);
		blockedMessage = boundedText(
			event.reason || `${event.toolName ?? "Tool"} approval`,
		);
		publish();
		hook("Notification", blockedMessage);
	});

	pi.on("tool_approval_resolved", (event: ApprovalEvent, ctx) => {
		if (!activateRoot(ctx)) return;
		if (event.toolCallId) approvals.delete(event.toolCallId);
		else approvals.clear();
		if (approvals.size === 0 && asks.size === 0) blockedMessage = undefined;
		publish();
	});

	pi.on("tool_execution_start", (event: ToolEvent, ctx) => {
		if (event.toolName !== "ask") return;
		if (!activateRoot(ctx)) return;
		const id = event.toolCallId ?? `ask:${asks.size}`;
		asks.add(id);
		blockedMessage = boundedText(firstQuestion(event.args));
		publish();
		hook("Notification", blockedMessage);
	});

	pi.on("tool_execution_end", (event: ToolEvent, ctx) => {
		if (event.toolName !== "ask") return;
		if (!activateRoot(ctx)) return;
		if (event.toolCallId) asks.delete(event.toolCallId);
		else asks.clear();
		if (approvals.size === 0 && asks.size === 0) blockedMessage = undefined;
		publish();
	});

	// OMP guarantees session_stop only for the root session after continuations
	// settle; child subagent completion therefore cannot mark this pane done.
	pi.on("session_stop", (_event, ctx) => {
		if (!activateRoot(ctx)) return;
		active = false;
		settled = true;
		blockedMessage = undefined;
		approvals.clear();
		asks.clear();
		publish();
		hook("Stop");
	});

	pi.on("session_shutdown", async (_event, ctx) => {
		if (!activateRoot(ctx)) return;
		await enqueue(pi, ["agent", "release", PANE_ID, "--source", SOURCE]);
	});
}

export default createLuvusExtension;
