// pi-luvus v0.1.0 — Luvus control plane client for Pi — Apache-2.0
// Self-contained extension bundle. Load with `pi -e` or install to
// ~/.pi/agent/extensions/. Pi-owned packages stay external (peer deps).
// @bun
// src/config.ts
var AGENT = "pi";
var SOURCE = "pi/extension";
var DEFAULT_TTL_SECONDS = 300;
var DEFAULT_HEARTBEAT_SECONDS = 90;
var MAX_TASK_ID_LENGTH = 256;
var MAX_PANE_ID_U64 = 18446744073709551615n;
var MAX_PATH_LENGTH = 4096;
function boundedToken(value, maxLength) {
  if (value === undefined || value.length === 0 || value.length > maxLength) {
    return;
  }
  if (value.trim().length === 0)
    return;
  for (const ch of value) {
    const code = ch.codePointAt(0) ?? 0;
    if (code < 32 || code === 127)
      return;
  }
  return value;
}
function boundedPath(value) {
  return boundedToken(value, MAX_PATH_LENGTH);
}
var U64_PATTERN = /^[1-9][0-9]*$/;
function validPaneId(value) {
  if (value === undefined || !U64_PATTERN.test(value))
    return;
  return BigInt(value) <= MAX_PANE_ID_U64 ? value : undefined;
}
function parseLuvusConfig(env) {
  const paneId = validPaneId(env.LUVUS_PANE_ID);
  const socketPath = boundedPath(env.LUVUS_SOCKET_PATH);
  const apiAddress = boundedPath(env.LUVUS_API_ADDRESS);
  let endpointKind = "none";
  if (socketPath !== undefined)
    endpointKind = "socket";
  else if (apiAddress !== undefined)
    endpointKind = "api-address";
  let disabledReason;
  if (env.LUVUS_ENV !== "1") {
    disabledReason = 'luvus bridge disabled: LUVUS_ENV is not "1"';
  } else if (paneId === undefined) {
    disabledReason = "luvus bridge disabled: missing or invalid LUVUS_PANE_ID";
  } else if (socketPath === undefined) {
    disabledReason = "luvus bridge disabled: no local endpoint (LUVUS_SOCKET_PATH is required; LUVUS_API_ADDRESS alone does not enable the bridge)";
  }
  return {
    enabled: disabledReason === undefined,
    paneId: paneId ?? "",
    endpointKind,
    socketPath,
    apiAddress,
    binPath: boundedPath(env.LUVUS_BIN_PATH),
    taskId: boundedToken(env.LUVUS_TASK_ID, MAX_TASK_ID_LENGTH),
    agent: AGENT,
    source: SOURCE,
    ttlSeconds: DEFAULT_TTL_SECONDS,
    heartbeatSeconds: DEFAULT_HEARTBEAT_SECONDS,
    usageReporting: false,
    widget: env.LUVUS_WIDGET === "1",
    metricsEnabled: true,
    disabledReason
  };
}
function loadLuvusConfig() {
  return parseLuvusConfig(process.env);
}

// src/lifecycle.ts
var ASK_TOOL_NAME = "ask";
function mapSessionStart(_event) {
  return { type: "session_start" };
}
function mapSessionShutdown(_event) {
  return { type: "shutdown" };
}
function mapAgentStart(_event) {
  return { type: "agent_start" };
}
function mapAgentSettled(_event) {
  return { type: "agent_settled" };
}
function mapUiPromptStart(_event) {
  return { type: "ui_prompt_start" };
}
function mapUiPromptEnd(_event) {
  return { type: "ui_prompt_end" };
}
function mapToolExecutionStart(event) {
  if (event.toolName !== ASK_TOOL_NAME)
    return;
  return { type: "ask_start", toolCallId: event.toolCallId };
}
function mapToolExecutionEnd(event) {
  if (event.toolName !== ASK_TOOL_NAME)
    return;
  return { type: "ask_end", toolCallId: event.toolCallId };
}

// src/orch/context-heartbeat.ts
var MIN_ORCH_RATIO_DELTA = 0.05;
function contextRatio(usage) {
  if (usage === undefined || usage === null)
    return;
  const tokens = usage.tokens;
  const window = usage.contextWindow;
  if (tokens === null || tokens === undefined)
    return;
  if (!Number.isFinite(tokens) || !Number.isFinite(window))
    return;
  if (window <= 0 || tokens < 0)
    return;
  return Math.min(1, Math.max(0, tokens / window));
}

class OrchContextTracker {
  minDelta;
  lastSent;
  pending;
  constructor(minDelta = MIN_ORCH_RATIO_DELTA) {
    this.minDelta = minDelta;
  }
  attempt(usage) {
    const ratio = contextRatio(usage);
    if (ratio === undefined)
      return;
    const baseline = this.pending ?? this.lastSent;
    if (baseline !== undefined && Math.abs(ratio - baseline) < this.minDelta) {
      return;
    }
    this.pending = ratio;
    return ratio;
  }
  ack(ratio) {
    if (this.pending === undefined || ratio !== this.pending)
      return;
    this.lastSent = ratio;
    this.pending = undefined;
  }
  fail(ratio) {
    if (this.pending === undefined || ratio !== this.pending)
      return;
    this.pending = undefined;
  }
  reset() {
    this.lastSent = undefined;
    this.pending = undefined;
  }
}

// src/session/identity.ts
var UUID_PATTERN = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
var MAX_SESSION_ID_LENGTH = 128;
function isValidSessionId(value) {
  if (value === undefined || value.length > MAX_SESSION_ID_LENGTH) {
    return false;
  }
  return UUID_PATTERN.test(value);
}
function safeRead(read) {
  try {
    return read();
  } catch {
    return;
  }
}
function readSessionIdentity(manager) {
  const rawId = safeRead(() => manager.getSessionId());
  const rawFile = safeRead(() => manager.getSessionFile());
  const sessionId = typeof rawId === "string" ? rawId : undefined;
  const sessionFile = typeof rawFile === "string" ? rawFile : undefined;
  const persisted = sessionFile !== undefined;
  const bindableSessionId = persisted && isValidSessionId(sessionId) ? sessionId : undefined;
  return { persisted, sessionId, sessionFile, bindableSessionId };
}
function reportSessionId(identity) {
  return identity.bindableSessionId;
}

// src/state/authority.ts
var MAX_MESSAGE_CODE_POINTS = 200;
var MAX_CLASSIFY_CHARS = 4096;
function boundMessage(message) {
  if (message === undefined)
    return;
  const points = Array.from(message);
  return points.length <= MAX_MESSAGE_CODE_POINTS ? message : points.slice(0, MAX_MESSAGE_CODE_POINTS).join("");
}
function errorText(error) {
  if (typeof error === "string")
    return error.slice(0, MAX_CLASSIFY_CHARS);
  if (typeof error === "object" && error !== null) {
    const record = error;
    const parts = [];
    for (const part of [record.message, record.stderr, record.stdout]) {
      if (typeof part === "string" && part.length > 0)
        parts.push(part);
    }
    return parts.join(`
`).slice(0, MAX_CLASSIFY_CHARS);
  }
  return "";
}
function classifyReportError(error) {
  if (typeof error === "object" && error !== null) {
    const code = error.code;
    if (code === "stale_report")
      return "stale_report";
    if (code === "authority_conflict")
      return "authority_conflict";
  }
  const text = errorText(error);
  if (/stale[_\s-]?report/i.test(text))
    return "stale_report";
  if (/authority[_\s-]?conflict/i.test(text))
    return "authority_conflict";
  return "other";
}

class StateAuthority {
  pane;
  source;
  agent;
  now;
  lastSequence;
  lastKey;
  constructor(options) {
    this.pane = options.pane;
    this.source = options.source;
    this.agent = options.agent;
    this.now = options.now ?? (() => Date.now());
  }
  buildReport(input) {
    const message = boundMessage(input.message);
    const key = [input.state, message ?? "", input.sessionId ?? ""].join("\x00");
    if (input.force !== true && key === this.lastKey) {
      return;
    }
    const sequence = this.allocateSequence();
    this.lastKey = key;
    return {
      pane: this.pane,
      source: this.source,
      agent: this.agent,
      status: input.state,
      message,
      sessionId: input.sessionId,
      sequence,
      ttlSeconds: DEFAULT_TTL_SECONDS
    };
  }
  resetSession() {
    this.lastKey = undefined;
  }
  invalidateForRetry(sequence) {
    if (sequence === this.lastSequence) {
      this.lastKey = undefined;
      return true;
    }
    return false;
  }
  allocateSequence() {
    if (this.lastSequence === undefined) {
      const seeded = Math.floor(this.now() * 1000);
      this.lastSequence = Number.isSafeInteger(seeded) && seeded > 0 ? seeded : 1;
    } else {
      const candidate = this.lastSequence + 1;
      if (!Number.isSafeInteger(candidate) || candidate <= this.lastSequence) {
        throw new RangeError("sequence overflow");
      }
      this.lastSequence = candidate;
    }
    return this.lastSequence;
  }
}

// src/state/heartbeat.ts
var DEFAULT_HEARTBEAT_INTERVAL_MS = DEFAULT_HEARTBEAT_SECONDS * 1000;
function systemHeartbeatScheduler() {
  return {
    schedule(callback, intervalMs) {
      const timer = setInterval(callback, intervalMs);
      const maybeUnref = timer;
      if (typeof maybeUnref.unref === "function") {
        maybeUnref.unref();
      }
      return {
        cancel: () => clearInterval(timer)
      };
    }
  };
}

class HeartbeatController {
  intervalMs;
  scheduler;
  onBeat;
  handle;
  current;
  stopped = false;
  epoch = 0;
  constructor(options) {
    this.intervalMs = options.intervalMs ?? DEFAULT_HEARTBEAT_INTERVAL_MS;
    this.scheduler = options.scheduler ?? systemHeartbeatScheduler();
    this.onBeat = options.onBeat;
  }
  start() {
    if (this.handle !== undefined)
      return;
    this.stopped = false;
    this.epoch += 1;
    const epoch = this.epoch;
    const scheduler = this.scheduler;
    const intervalMs = this.intervalMs;
    this.handle = scheduler.schedule(() => {
      if (epoch === this.epoch)
        this.onTick();
    }, intervalMs);
  }
  stop() {
    this.stopped = true;
    this.epoch += 1;
    this.handle?.cancel();
    this.handle = undefined;
  }
  beat() {
    if (this.stopped)
      return Promise.resolve();
    if (this.current !== undefined)
      return this.current;
    const task = this.executeBeat();
    this.current = task;
    return task;
  }
  async executeBeat() {
    try {
      await this.onBeat();
    } finally {
      this.current = undefined;
    }
  }
  onTick() {
    if (this.stopped || this.current !== undefined)
      return;
    this.beat().then(() => {}, () => {});
  }
}

// src/state/reducer.ts
function createInitialState() {
  return {
    publicState: "idle",
    active: false,
    settled: false,
    uiDepth: 0,
    askIds: new Set,
    shutdown: false
  };
}
function derivePublicState(draft) {
  if (draft.uiDepth > 0 || draft.askIds.size > 0)
    return "blocked";
  if (draft.active)
    return "working";
  if (draft.settled)
    return "done";
  return "idle";
}
function finish(draft) {
  return { ...draft, publicState: derivePublicState(draft) };
}
function draftOf(state) {
  return {
    active: state.active,
    settled: state.settled,
    uiDepth: state.uiDepth,
    askIds: state.askIds,
    shutdown: state.shutdown
  };
}
function reduce(state, event) {
  if (event.type === "shutdown") {
    if (state.shutdown)
      return state;
    return { ...state, shutdown: true };
  }
  if (event.type === "session_start") {
    return createInitialState();
  }
  if (state.shutdown)
    return state;
  switch (event.type) {
    case "agent_start": {
      if (state.active && !state.settled)
        return state;
      return finish({ ...draftOf(state), active: true, settled: false });
    }
    case "agent_settled": {
      if (!state.active && state.settled)
        return state;
      return finish({ ...draftOf(state), active: false, settled: true });
    }
    case "ui_prompt_start": {
      return finish({ ...draftOf(state), uiDepth: state.uiDepth + 1 });
    }
    case "ui_prompt_end": {
      if (state.uiDepth === 0)
        return state;
      return finish({ ...draftOf(state), uiDepth: state.uiDepth - 1 });
    }
    case "ask_start": {
      if (state.askIds.has(event.toolCallId))
        return state;
      const askIds = new Set(state.askIds);
      askIds.add(event.toolCallId);
      return finish({ ...draftOf(state), askIds });
    }
    case "ask_end": {
      if (!state.askIds.has(event.toolCallId))
        return state;
      const askIds = new Set(state.askIds);
      askIds.delete(event.toolCallId);
      return finish({ ...draftOf(state), askIds });
    }
  }
}

// src/inspection/fleet.ts
var FLEET_STATUSES = [
  "idle",
  "working",
  "blocked",
  "done"
];
function isRecord(value) {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
function asText(value) {
  return typeof value === "string" && value.length > 0 ? value : undefined;
}
function paneToEntry(pane, now) {
  if (typeof pane.paneId !== "string" || typeof pane.agent !== "string" || typeof pane.status !== "string" || typeof pane.authority !== "string" || typeof pane.cwd !== "string" || typeof pane.workspaceIndex !== "number" || typeof pane.tabIndex !== "number" || typeof pane.focused !== "boolean" || !FLEET_STATUSES.includes(pane.status)) {
    return;
  }
  const entry = {
    paneId: pane.paneId,
    agent: pane.agent,
    status: pane.status,
    authority: pane.authority,
    cwd: pane.cwd,
    workspaceIndex: pane.workspaceIndex,
    tabIndex: pane.tabIndex,
    focused: pane.focused,
    updatedAt: now
  };
  if (pane.agentSession !== undefined && pane.agentSession.length > 0) {
    entry.session = pane.agentSession;
  }
  if (pane.workspaceName !== undefined && pane.workspaceName.length > 0) {
    entry.workspaceName = pane.workspaceName;
  }
  return entry;
}

class FleetCache {
  entries = new Map;
  generation;
  eventSequence;
  now;
  constructor(options = {}) {
    this.now = options.now ?? Date.now;
  }
  bootstrapFromSnapshot(snapshot) {
    try {
      const panes = snapshot?.panes;
      const header = snapshot?.header;
      if (!Array.isArray(panes))
        return;
      const now = this.now();
      const next = new Map;
      for (const pane of panes) {
        const entry = paneToEntry(pane, now);
        if (entry !== undefined)
          next.set(entry.paneId, entry);
      }
      this.entries.clear();
      for (const [paneId, entry] of next)
        this.entries.set(paneId, entry);
      if (isRecord(header)) {
        if (typeof header.serverGeneration === "string") {
          this.generation = header.serverGeneration;
        }
        if (typeof header.eventSequence === "number" && Number.isSafeInteger(header.eventSequence) && header.eventSequence >= 0) {
          this.eventSequence = header.eventSequence;
        }
      }
    } catch {}
  }
  applyEvent(event) {
    try {
      if (event === null || typeof event !== "object" || typeof event.event !== "string" || typeof event.sequence !== "number" || !Number.isSafeInteger(event.sequence)) {
        return;
      }
      const name = event.event;
      const sequence = event.sequence;
      if (name === "agent.reported") {
        const data = event.data;
        if (!isRecord(data))
          return;
        const pane = asText(data.pane);
        const agent = asText(data.agent);
        const status = asText(data.status);
        if (pane === undefined || agent === undefined || status === undefined || !FLEET_STATUSES.includes(status)) {
          return;
        }
        const authority = asText(data.source) ?? asText(data.authority) ?? "";
        const previous = this.entries.get(pane);
        this.entries.set(pane, {
          paneId: pane,
          agent,
          status,
          authority,
          session: asText(data.session) ?? previous?.session,
          cwd: asText(data.cwd) ?? previous?.cwd ?? "",
          workspaceName: asText(data.workspace) ?? asText(data.workspaceName) ?? previous?.workspaceName,
          workspaceIndex: previous?.workspaceIndex ?? 0,
          tabIndex: previous?.tabIndex ?? 0,
          focused: previous?.focused ?? false,
          updatedAt: this.now()
        });
        this.eventSequence = sequence;
        return;
      }
      if (name === "agent.released") {
        const data = event.data;
        if (!isRecord(data))
          return;
        const pane = asText(data.pane) ?? asText(data.pane_id);
        if (pane === undefined)
          return;
        this.entries.delete(pane);
        this.eventSequence = sequence;
        return;
      }
    } catch {}
  }
  snapshot() {
    return {
      entries: Object.freeze([...this.entries.values()]),
      generation: this.generation,
      eventSequence: this.eventSequence,
      size: this.entries.size
    };
  }
  list(filter = {}) {
    const out = [];
    for (const entry of this.entries.values()) {
      if (filter.agent !== undefined && entry.agent !== filter.agent)
        continue;
      if (filter.status !== undefined && entry.status !== filter.status) {
        continue;
      }
      out.push(entry);
    }
    return out;
  }
  get(paneId) {
    return this.entries.get(paneId);
  }
  get onSnapshot() {
    return (snapshot) => {
      this.bootstrapFromSnapshot(snapshot);
    };
  }
  get onEvent() {
    return (event) => {
      this.applyEvent(event);
    };
  }
}

// src/inspection/metrics.ts
var MAX_LATENCY_SAMPLES = 32;
var MAX_LATENCY_MS = 3600000;

class LatencyRing {
  capacity;
  samples = [];
  constructor(capacity = MAX_LATENCY_SAMPLES) {
    if (!Number.isSafeInteger(capacity) || capacity <= 0) {
      throw new RangeError("LatencyRing requires a positive capacity");
    }
    this.capacity = capacity;
  }
  push(sample) {
    if (typeof sample !== "number" || !Number.isFinite(sample) || sample < 0 || sample > MAX_LATENCY_MS) {
      return;
    }
    this.samples.push(sample);
    while (this.samples.length > this.capacity)
      this.samples.shift();
  }
  get size() {
    return this.samples.length;
  }
  snapshot() {
    return Object.freeze([...this.samples]);
  }
  summary() {
    if (this.samples.length === 0) {
      return { count: 0, last: undefined, avg: undefined, max: undefined };
    }
    let sum = 0;
    let max = 0;
    for (const sample of this.samples) {
      sum += sample;
      if (sample > max)
        max = sample;
    }
    return {
      count: this.samples.length,
      last: this.samples[this.samples.length - 1],
      avg: Math.round(sum / this.samples.length * 100) / 100,
      max
    };
  }
  reset() {
    this.samples.length = 0;
  }
}

class MetricsCollector {
  latency = new LatencyRing;
  telemetryOk = 0;
  telemetryFailed = 0;
  heartbeatsSent = 0;
  heartbeatsFailed = 0;
  retries = 0;
  resyncs = 0;
  generationChanges = 0;
  maxTelemetryQueueDepth = 0;
  lastGeneration;
  seenGeneration = false;
  recordTelemetryOk(latencyMs) {
    this.telemetryOk += 1;
    if (latencyMs !== undefined)
      this.latency.push(latencyMs);
  }
  recordTelemetryFailed() {
    this.telemetryFailed += 1;
  }
  recordHeartbeatSent() {
    this.heartbeatsSent += 1;
  }
  recordHeartbeatFailed() {
    this.heartbeatsFailed += 1;
  }
  recordRetry() {
    this.retries += 1;
  }
  recordQueueDepth(depth) {
    if (typeof depth !== "number" || !Number.isSafeInteger(depth) || depth < 0) {
      return;
    }
    if (depth > this.maxTelemetryQueueDepth)
      this.maxTelemetryQueueDepth = depth;
  }
  noteCoordinatorStatus(status) {
    try {
      if (status === null || typeof status !== "object")
        return;
      const { generation, resyncCount } = status;
      if (typeof resyncCount === "number" && Number.isSafeInteger(resyncCount) && resyncCount > this.resyncs) {
        this.resyncs = resyncCount;
      }
      if (typeof generation === "string" && generation.length > 0) {
        if (!this.seenGeneration) {
          this.seenGeneration = true;
          this.lastGeneration = generation;
        } else if (generation !== this.lastGeneration) {
          this.generationChanges += 1;
          this.lastGeneration = generation;
        }
      }
    } catch {}
  }
  reset() {
    this.latency.reset();
    this.telemetryOk = 0;
    this.telemetryFailed = 0;
    this.heartbeatsSent = 0;
    this.heartbeatsFailed = 0;
    this.retries = 0;
    this.resyncs = 0;
    this.generationChanges = 0;
    this.maxTelemetryQueueDepth = 0;
    this.lastGeneration = undefined;
    this.seenGeneration = false;
  }
  snapshot() {
    return Object.freeze({
      telemetryOk: this.telemetryOk,
      telemetryFailed: this.telemetryFailed,
      heartbeatsSent: this.heartbeatsSent,
      heartbeatsFailed: this.heartbeatsFailed,
      retries: this.retries,
      resyncs: this.resyncs,
      generationChanges: this.generationChanges,
      maxTelemetryQueueDepth: this.maxTelemetryQueueDepth,
      latency: this.latency.summary()
    });
  }
}
function formatMetricsLine(snapshot) {
  const parts = [
    `telemetry ok ${snapshot.telemetryOk}/failed ${snapshot.telemetryFailed}`,
    `heartbeats sent ${snapshot.heartbeatsSent}/failed ${snapshot.heartbeatsFailed}`,
    `retries ${snapshot.retries}`,
    `resyncs ${snapshot.resyncs}`,
    `queue peak ${snapshot.maxTelemetryQueueDepth}`
  ];
  const latency = snapshot.latency;
  if (latency.count > 0) {
    parts.push(`latency n=${latency.count} last=${latency.last}ms avg=${latency.avg}ms max=${latency.max}ms`);
  } else {
    parts.push("latency n=0");
  }
  return parts.join(" \xB7 ");
}

// src/transport/cli.ts
var DEFAULT_BIN = "luvus";
var BIN_ENV_VAR = "LUVUS_BIN_PATH";
var DEFAULT_AGENT = "pi";
var DEFAULT_TIMEOUT_MS = 1500;
var MAX_OUTPUT_BYTES = 64 * 1024;
var MAX_TTL_SECONDS = 86400;

class LuvusCliError extends Error {
  reason;
  bin;
  argv;
  exitCode;
  killed;
  stdout;
  stderr;
  constructor(details) {
    super(describeFailure(details));
    this.name = "LuvusCliError";
    this.reason = details.reason;
    this.bin = details.bin;
    this.argv = [...details.argv];
    this.exitCode = details.exitCode;
    this.killed = details.killed;
    this.stdout = details.stdout;
    this.stderr = details.stderr;
    if (details.cause !== undefined) {
      this.cause = details.cause;
    }
  }
}
function describeFailure(details) {
  const invocation = `${details.bin} ${details.argv.slice(0, 2).join(" ")}`;
  switch (details.reason) {
    case "aborted":
      return `luvus CLI call aborted before start: ${invocation}`;
    case "exec-throw":
      return `luvus CLI execution failed: ${invocation}`;
    case "killed":
      return `luvus CLI call killed: ${invocation}`;
    case "exit-code":
      return `luvus CLI exited with code ${details.exitCode ?? "?"}: ${invocation}`;
  }
}
function truncateOutput(value, maxBytes = MAX_OUTPUT_BYTES) {
  const encoded = new TextEncoder().encode(value);
  if (encoded.length <= maxBytes)
    return value;
  let end = maxBytes;
  while (end > 0 && (encoded[end - 1] & 192) === 128) {
    end -= 1;
  }
  if (end > 0) {
    const lead = encoded[end - 1];
    const need = lead < 128 ? 1 : lead < 224 ? 2 : lead < 240 ? 3 : 4;
    if (maxBytes - (end - 1) < need)
      end -= 1;
  }
  return new TextDecoder().decode(encoded.subarray(0, end));
}
function isAbort(error, signal) {
  if (signal?.aborted === true)
    return true;
  return typeof error === "object" && error !== null && error.name === "AbortError";
}
function resolveCliBin(explicit) {
  if (explicit !== undefined && explicit.trim().length > 0) {
    return explicit.trim();
  }
  const fromEnv = process.env[BIN_ENV_VAR]?.trim();
  if (fromEnv !== undefined && fromEnv.length > 0)
    return fromEnv;
  return DEFAULT_BIN;
}

class LuvusCliClient {
  pane;
  source;
  agent;
  bin;
  timeoutMs;
  exec;
  constructor(options) {
    if (options.pane.length === 0) {
      throw new Error("LuvusCliClient requires a non-empty pane");
    }
    if (options.source.length === 0) {
      throw new Error("LuvusCliClient requires a non-empty source");
    }
    this.pane = options.pane;
    this.source = options.source;
    this.agent = options.agent !== undefined && options.agent.length > 0 ? options.agent : DEFAULT_AGENT;
    this.bin = resolveCliBin(options.bin);
    this.timeoutMs = options.timeoutMs ?? DEFAULT_TIMEOUT_MS;
    this.exec = options.exec;
  }
  async bindSession(sessionId, options) {
    if (sessionId.length === 0) {
      throw new Error("bindSession requires a non-empty sessionId");
    }
    await this.run(["pane", "report", this.pane, "--agent", this.agent, "--session", sessionId], options);
  }
  async reportAgent(report, options) {
    if (report.pane !== this.pane) {
      throw new Error(`reportAgent pane mismatch: client is ${this.pane}, report is ${report.pane}`);
    }
    if (report.source !== this.source) {
      throw new Error(`reportAgent source mismatch: client is ${this.source}, report is ${report.source}`);
    }
    if (report.agent !== this.agent) {
      throw new Error(`reportAgent agent mismatch: client is ${this.agent}, report is ${report.agent}`);
    }
    const args = [
      "agent",
      "report",
      this.pane,
      "--source",
      this.source,
      "--kind",
      this.agent,
      "--status",
      report.status
    ];
    if (report.message !== undefined && report.message.length > 0) {
      args.push("--message", report.message);
    }
    if (report.sessionId !== undefined && report.sessionId.length > 0) {
      args.push("--session", report.sessionId);
    }
    if (report.sequence !== undefined) {
      if (!Number.isSafeInteger(report.sequence) || report.sequence <= 0) {
        throw new RangeError(`reportAgent sequence must be a safe integer > 0; received ${String(report.sequence)}`);
      }
      args.push("--sequence", String(report.sequence));
    }
    if (report.ttlSeconds !== undefined) {
      if (!Number.isSafeInteger(report.ttlSeconds) || report.ttlSeconds < 1 || report.ttlSeconds > MAX_TTL_SECONDS) {
        throw new RangeError(`reportAgent ttlSeconds must be a safe integer within [1, ${MAX_TTL_SECONDS}]; received ${String(report.ttlSeconds)}`);
      }
      args.push("--ttl", String(report.ttlSeconds));
    }
    await this.run(args, options);
  }
  async taskHeartbeat(taskId, ratio, options) {
    if (taskId.length === 0) {
      throw new Error("taskHeartbeat requires a non-empty taskId");
    }
    if (!Number.isFinite(ratio) || ratio < 0 || ratio > 1) {
      throw new RangeError(`taskHeartbeat ratio must be within [0, 1]; received ${String(ratio)}`);
    }
    await this.run(["task", "heartbeat", taskId, "--context", String(ratio)], options);
  }
  async releaseAgent(options) {
    await this.run(["agent", "release", this.pane, "--source", this.source], options);
  }
  async run(args, options) {
    const signal = options?.signal;
    if (signal?.aborted === true) {
      throw new LuvusCliError({
        reason: "aborted",
        bin: this.bin,
        argv: args,
        killed: false,
        stdout: "",
        stderr: ""
      });
    }
    let result;
    try {
      result = signal === undefined ? await this.exec(this.bin, args, { timeout: this.timeoutMs }) : await this.exec(this.bin, args, { timeout: this.timeoutMs, signal });
    } catch (error) {
      throw new LuvusCliError({
        reason: isAbort(error, signal) ? "aborted" : "exec-throw",
        bin: this.bin,
        argv: args,
        killed: false,
        stdout: "",
        stderr: "",
        cause: error
      });
    }
    if (result.killed) {
      throw new LuvusCliError({
        reason: "killed",
        bin: this.bin,
        argv: args,
        exitCode: result.code,
        killed: true,
        stdout: truncateOutput(result.stdout),
        stderr: truncateOutput(result.stderr)
      });
    }
    if (result.code !== 0) {
      throw new LuvusCliError({
        reason: "exit-code",
        bin: this.bin,
        argv: args,
        exitCode: result.code,
        killed: false,
        stdout: truncateOutput(result.stdout),
        stderr: truncateOutput(result.stderr)
      });
    }
  }
}

// src/policy/validators.ts
var MAX_PANE_ID_U642 = 18446744073709551615n;
var U64_PATTERN2 = /^[1-9][0-9]*$/;
var MAX_TASK_ID_LENGTH2 = 256;
var MAX_AGENT_KIND_LENGTH = 128;
var MAX_PROMPT_BYTES = 64 * 1024;
var MAX_LEASE_PATH_LENGTH = 4096;
var AGENT_KIND_PATTERN = /^[A-Za-z0-9_-]+$/;
var TASK_ID_FORBIDDEN_PATTERN = /[\u0000-\u001f\u007f;&|$`\\'"()<>*!?~#\n\r]/;
function hasControlChars(value) {
  for (const ch of value) {
    const code = ch.codePointAt(0) ?? 0;
    if (code < 32 || code === 127)
      return true;
  }
  return false;
}
function byteLength(value) {
  return new TextEncoder().encode(value).length;
}
function validatePaneId(value) {
  if (typeof value !== "string" || !U64_PATTERN2.test(value)) {
    return { valid: false, reason: "pane id must be a canonical positive integer" };
  }
  try {
    if (BigInt(value) > MAX_PANE_ID_U642) {
      return { valid: false, reason: "pane id exceeds u64 range" };
    }
  } catch {
    return { valid: false, reason: "pane id must be a canonical positive integer" };
  }
  return { valid: true, sanitized: value };
}
function validateTaskId(value) {
  if (typeof value !== "string" || value.length === 0) {
    return { valid: false, reason: "task id must be non-empty" };
  }
  if (value.length > MAX_TASK_ID_LENGTH2) {
    return { valid: false, reason: "task id exceeds 256 chars" };
  }
  if (TASK_ID_FORBIDDEN_PATTERN.test(value)) {
    return { valid: false, reason: "task id contains forbidden characters" };
  }
  return { valid: true, sanitized: value };
}
function validateLeasePath(value) {
  if (typeof value !== "string" || value.length === 0) {
    return { valid: false, reason: "lease path must be non-empty" };
  }
  if (value.length > MAX_LEASE_PATH_LENGTH) {
    return { valid: false, reason: "lease path exceeds 4096 chars" };
  }
  if (value.includes("\x00") || value.includes(`
`) || value.includes("\r")) {
    return { valid: false, reason: "lease path contains forbidden characters" };
  }
  return { valid: true, sanitized: value };
}
function validateAgentKind(value) {
  if (typeof value !== "string" || value.length === 0) {
    return { valid: false, reason: "agent kind must be non-empty" };
  }
  if (value.length > MAX_AGENT_KIND_LENGTH || !AGENT_KIND_PATTERN.test(value)) {
    return {
      valid: false,
      reason: "agent kind must match [A-Za-z0-9_-]{1,128}"
    };
  }
  return { valid: true, sanitized: value };
}
function validatePrompt(value) {
  if (typeof value !== "string" || value.length === 0) {
    return { valid: false, reason: "prompt must be non-empty" };
  }
  if (value.includes("\x00")) {
    return { valid: false, reason: "prompt contains a null byte" };
  }
  if (byteLength(value) > MAX_PROMPT_BYTES) {
    return { valid: false, reason: "prompt exceeds 64 KiB" };
  }
  return { valid: true, sanitized: value };
}

class SelfDelegationGuard {
  ownPaneId;
  constructor(ownPaneId) {
    if (ownPaneId.length === 0) {
      throw new Error("SelfDelegationGuard requires a non-empty pane id");
    }
    this.ownPaneId = ownPaneId;
  }
  check(targetPaneId) {
    if (targetPaneId === this.ownPaneId) {
      return {
        allowed: false,
        reason: "self-delegation",
        detail: `cannot delegate to own pane ${this.ownPaneId}`
      };
    }
    return { allowed: true };
  }
}
var DEFAULT_ARC_TTL_MS = 600000;

class LoopDetector {
  ttlMs;
  now;
  arcs = [];
  constructor(options = {}) {
    if (options.ttlMs !== undefined && (!Number.isSafeInteger(options.ttlMs) || options.ttlMs <= 0)) {
      throw new RangeError("LoopDetector ttlMs must be a safe integer > 0");
    }
    this.ttlMs = options.ttlMs ?? DEFAULT_ARC_TTL_MS;
    this.now = options.now ?? Date.now;
  }
  recordArc(sourcePaneId, targetPaneId) {
    const at = this.now();
    this.arcs = this.arcs.filter((arc) => at - arc.at < this.ttlMs);
    if (sourcePaneId === targetPaneId) {
      return { detected: true, chain: [sourcePaneId] };
    }
    const targetsOf = (node) => this.arcs.filter((arc) => arc.source === node).map((arc) => arc.target);
    if (targetsOf(targetPaneId).includes(sourcePaneId)) {
      return {
        detected: true,
        chain: [sourcePaneId, targetPaneId, sourcePaneId]
      };
    }
    for (const middle of targetsOf(targetPaneId)) {
      if (targetsOf(middle).includes(sourcePaneId)) {
        return {
          detected: true,
          chain: [sourcePaneId, targetPaneId, middle, sourcePaneId]
        };
      }
    }
    this.arcs.push({ source: sourcePaneId, target: targetPaneId, at });
    return { detected: false };
  }
  resetSession() {
    this.arcs = [];
  }
}
function validateBranchName(value) {
  if (typeof value !== "string" || value.length === 0) {
    return { valid: false, reason: "branch name must be non-empty" };
  }
  if (value.length > 256 || hasControlChars(value) || value.includes("\x00")) {
    return { valid: false, reason: "branch name is invalid" };
  }
  return { valid: true, sanitized: value };
}

// src/operations/cli-ops.ts
var MAX_CLI_OUTPUT_BYTES = 64 * 1024;
var DEFAULT_OPS_TIMEOUT_MS = 15000;

class CliOpsError extends Error {
  action;
  constructor(action, detail) {
    super(detail !== undefined ? `cli ops ${action} failed: ${detail}` : `cli ops ${action} failed`);
    this.name = "CliOpsError";
    this.action = action;
  }
}
function isRecord2(value) {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
function fail(action, hint) {
  throw new CliOpsError(action, hint);
}
function short(value, max = 256) {
  if (typeof value !== "string")
    return "";
  return value.length <= max ? value : value.slice(0, max);
}
function pick(record, keys) {
  for (const key of keys) {
    const value = record[key];
    if (typeof value === "string" && value.length > 0)
      return value;
  }
  return "";
}

class CliOpsClient {
  exec;
  bin;
  timeoutMs;
  constructor(options) {
    if (options === null || typeof options !== "object" || typeof options.exec !== "function") {
      throw new TypeError("CliOpsClient requires an exec function");
    }
    this.exec = options.exec;
    this.bin = resolveCliBin(options.bin);
    this.timeoutMs = options.timeoutMs ?? DEFAULT_OPS_TIMEOUT_MS;
    if (!Number.isSafeInteger(this.timeoutMs) || this.timeoutMs <= 0) {
      throw new RangeError("CliOpsClient timeoutMs must be a safe integer > 0");
    }
  }
  async run(action, args, signal) {
    if (signal?.aborted === true) {
      throw new CliOpsError(action, "aborted");
    }
    let stdout;
    try {
      const result = signal === undefined ? await this.exec(this.bin, args, { timeout: this.timeoutMs }) : await this.exec(this.bin, args, {
        timeout: this.timeoutMs,
        signal
      });
      if (result.killed || result.code !== 0) {
        throw new CliOpsError(action, "command failed");
      }
      stdout = truncateOutput(result.stdout, MAX_CLI_OUTPUT_BYTES);
    } catch (error) {
      if (error instanceof CliOpsError)
        throw error;
      if (typeof error === "object" && error !== null && error.name === "AbortError") {
        throw new CliOpsError(action, "aborted");
      }
      throw new CliOpsError(action, "execution failed");
    }
    return stdout;
  }
  async runJson(action, args, signal) {
    const stdout = await this.run(action, [...args, "--json"], signal);
    if (stdout.trim().length === 0)
      return [];
    try {
      return JSON.parse(stdout);
    } catch {
      throw new CliOpsError(action, "invalid json output");
    }
  }
  async agentStart(params, signal) {
    const action = "agent/start";
    if (params.name.length === 0 || params.name.length > 128) {
      fail(action, "name");
    }
    const kind = validateAgentKind(params.kind);
    if (!kind.valid)
      fail(action, `kind: ${kind.reason}`);
    const args = ["agent", "start", params.name, "--kind", params.kind];
    if (params.pane !== undefined) {
      const pane = validatePaneId(params.pane);
      if (!pane.valid)
        fail(action, `pane: ${pane.reason}`);
      args.push("--pane", params.pane);
    } else if (params.anchor !== undefined) {
      const anchor = validatePaneId(params.anchor);
      if (!anchor.valid)
        fail(action, `anchor: ${anchor.reason}`);
      args.push("--anchor", params.anchor);
    }
    if (params.down === true)
      args.push("--down");
    if (params.args !== undefined && params.args.length > 0) {
      args.push("--", ...params.args);
    }
    const payload = await this.runJson(action, args, signal);
    if (!isRecord2(payload)) {
      return { paneId: "", name: params.name, kind: params.kind, status: "" };
    }
    return {
      paneId: pick(payload, ["pane_id", "pane", "paneId", "id"]),
      name: short(payload.name ?? params.name, 128),
      kind: short(payload.kind ?? params.kind, 128),
      status: short(payload.status, 64)
    };
  }
  async agentPrompt(params, signal) {
    const action = "agent/prompt";
    if (params.target.length === 0)
      fail(action, "target");
    const prompt = validatePrompt(params.text);
    if (!prompt.valid)
      fail(action, `text: ${prompt.reason}`);
    const args = ["agent", "prompt", params.target, params.text];
    if (params.wait === true) {
      args.push("--wait");
      if (params.until !== undefined)
        args.push("--until", params.until);
    }
    if (params.timeoutS !== undefined) {
      args.push("--timeout", String(params.timeoutS));
    }
    await this.run(action, args, signal);
  }
  async agentFork(params, signal) {
    const action = "agent/fork";
    if (params.target.length === 0)
      fail(action, "target");
    const args = ["agent", "fork", params.target];
    if (params.name !== undefined && params.name.length > 0) {
      args.push("--name", params.name);
    }
    const payload = await this.runJson(action, args, signal);
    if (!isRecord2(payload))
      return { paneId: "" };
    const paneId = pick(payload, ["pane_id", "pane", "paneId", "id"]);
    const name = pick(payload, ["name"]);
    return name.length > 0 ? { paneId, name } : { paneId };
  }
  async agentWait(params, signal) {
    const action = "agent/wait";
    const pane = validatePaneId(params.paneId);
    if (!pane.valid)
      fail(action, `pane: ${pane.reason}`);
    if (params.until !== undefined && params.matchText !== undefined) {
      fail(action, "until and matchText are mutually exclusive");
    }
    const timeoutS = params.timeoutS !== undefined ? Math.max(1, Math.floor(params.timeoutS)) : undefined;
    const isBenignWaitExit = (error) => error instanceof CliOpsError && !error.message.includes("aborted") && !error.message.includes("execution failed");
    if (params.matchText !== undefined) {
      const args = ["wait", "output", params.paneId, "--match", params.matchText];
      if (timeoutS !== undefined)
        args.push("--timeout", String(timeoutS));
      try {
        await this.run(action, args, signal);
        return { paneId: params.paneId, status: "", matched: true };
      } catch (error) {
        if (isBenignWaitExit(error)) {
          return { paneId: params.paneId, status: "", matched: false };
        }
        throw error;
      }
    }
    const until = params.until ?? "done";
    const args = ["wait", "agent-status", params.paneId, "--status", until];
    if (timeoutS !== undefined)
      args.push("--timeout", String(timeoutS));
    try {
      await this.run(action, args, signal);
      return { paneId: params.paneId, status: until, matched: true };
    } catch (error) {
      if (isBenignWaitExit(error)) {
        return { paneId: params.paneId, status: "", matched: false };
      }
      throw error;
    }
  }
  async agentRead(params, signal) {
    const action = "agent/read";
    if (params.target.length === 0)
      fail(action, "target");
    const args = ["agent", "read", params.target];
    if (params.lines !== undefined) {
      if (!Number.isSafeInteger(params.lines) || params.lines < 1 || params.lines > 200) {
        fail(action, "lines must be an integer within [1, 200]");
      }
      args.push("--lines", String(params.lines));
    }
    return this.run(action, args, signal);
  }
  async agentGet(paneId, signal) {
    if (paneId.length === 0)
      fail("agent/get", "empty pane id");
    return this.runJson("agent/get", ["agent", "get", paneId], signal);
  }
  async agentSessions(signal) {
    return this.runJson("agent/sessions", ["agent", "sessions"], signal);
  }
  async taskAdd(params, signal) {
    const action = "task/add";
    if (params.title.length === 0 || params.title.length > 512) {
      fail(action, "title");
    }
    const args = ["task", "add", params.title];
    if (params.paths !== undefined) {
      for (const entry of params.paths) {
        const checked = validateLeasePath(entry);
        if (!checked.valid)
          fail(action, `paths: ${checked.reason}`);
        args.push("--paths", entry);
      }
    }
    if (params.dependsOn !== undefined) {
      for (const id of params.dependsOn) {
        const checked = validateTaskId(id);
        if (!checked.valid)
          fail(action, `dependsOn: ${checked.reason}`);
        args.push("--dep", id);
      }
    }
    if (params.gate !== undefined)
      args.push("--gate", params.gate);
    const payload = await this.runJson(action, args, signal);
    if (!isRecord2(payload))
      return { taskId: "", title: params.title };
    const task = isRecord2(payload.task) ? payload.task : payload;
    return {
      taskId: pick(task, ["id", "task_id", "taskId"]),
      title: short(task.title ?? params.title, 512)
    };
  }
  async taskClaim(taskId, signal) {
    const checked = validateTaskId(taskId);
    if (!checked.valid)
      fail("task/claim", `task: ${checked.reason}`);
    await this.run("task/claim", ["task", "claim", taskId], signal);
  }
  async taskNext(params, signal) {
    const action = "task/next";
    const args = ["task", "next"];
    if (params.start === true)
      args.push("--start");
    if (params.agent !== undefined)
      args.push("--agent", params.agent);
    if (params.mode !== undefined) {
      const mode = params.mode;
      args.push("--mode", mode);
    }
    const payload = await this.runJson(action, args, signal);
    if (!isRecord2(payload))
      return { taskId: "", title: "", started: false };
    const task = isRecord2(payload.task) ? payload.task : payload;
    return {
      taskId: pick(task, ["id", "task_id", "taskId"]),
      title: short(task.title, 512),
      started: typeof payload.started === "boolean" ? payload.started : params.start === true
    };
  }
  async taskStart(params, signal) {
    const action = "task/start";
    const checked = validateTaskId(params.taskId);
    if (!checked.valid)
      fail(action, `task: ${checked.reason}`);
    const args = ["task", "start", params.taskId];
    if (params.branch !== undefined)
      args.push("--branch", params.branch);
    if (params.agent !== undefined)
      args.push("--agent", params.agent);
    if (params.mode !== undefined)
      args.push("--mode", params.mode);
    const payload = await this.runJson(action, args, signal);
    if (!isRecord2(payload))
      return { taskId: params.taskId };
    const out = { taskId: params.taskId };
    const branch = pick(payload, ["branch"]);
    const worktree = pick(payload, ["worktree", "path"]);
    const paneId = pick(payload, ["pane_id", "pane", "paneId"]);
    return {
      ...out,
      ...branch.length > 0 ? { branch } : {},
      ...worktree.length > 0 ? { worktree } : {},
      ...paneId.length > 0 ? { paneId } : {}
    };
  }
  async taskUpdate(params, signal) {
    const action = "task/update";
    const checked = validateTaskId(params.taskId);
    if (!checked.valid)
      fail(action, `task: ${checked.reason}`);
    const args = ["task", "update", params.taskId];
    if (params.status !== undefined)
      args.push("--status", params.status);
    if (params.output !== undefined)
      args.push("--output", params.output);
    if (params.note !== undefined)
      args.push("--note", params.note);
    await this.run(action, args, signal);
  }
  async taskById(action, cmd, taskId, signal) {
    const checked = validateTaskId(taskId);
    if (!checked.valid)
      fail(action, `task: ${checked.reason}`);
    await this.run(action, ["task", cmd, taskId], signal);
  }
  async taskDone(taskId, signal) {
    return this.taskById("task/done", "done", taskId, signal);
  }
  async taskMerge(taskId, signal) {
    return this.taskById("task/merge", "merge", taskId, signal);
  }
  async taskRelease(taskId, signal) {
    return this.taskById("task/release", "release", taskId, signal);
  }
  async taskDelete(taskId, signal) {
    return this.taskById("task/delete", "delete", taskId, signal);
  }
  async taskGet(taskId, signal) {
    const checked = validateTaskId(taskId);
    if (!checked.valid)
      fail("task/get", `task: ${checked.reason}`);
    return this.runJson("task/get", ["task", "get", taskId], signal);
  }
  async taskList(signal) {
    return this.runJson("task/list", ["task", "list"], signal);
  }
  async leaseAcquire(params, signal) {
    const action = "lease/acquire";
    if (params.paths.length === 0)
      fail(action, "paths must be non-empty");
    const checkedTask = validateTaskId(params.taskId);
    if (!checkedTask.valid)
      fail(action, `task: ${checkedTask.reason}`);
    const args = ["lease", "acquire"];
    for (const entry of params.paths) {
      const checked = validateLeasePath(entry);
      if (!checked.valid)
        fail(action, `paths: ${checked.reason}`);
      args.push(entry);
    }
    args.push("--task", params.taskId);
    await this.run(action, args, signal);
  }
  async leaseRelease(taskId, signal) {
    const checked = validateTaskId(taskId);
    if (!checked.valid)
      fail("lease/release", `task: ${checked.reason}`);
    await this.run("lease/release", ["lease", "release", taskId], signal);
  }
  async leaseList(signal) {
    return this.runJson("lease/list", ["lease", "list"], signal);
  }
  async worktreeCreate(branch, signal) {
    const checked = validateBranchName(branch);
    if (!checked.valid)
      fail("worktree/create", `branch: ${checked.reason}`);
    const payload = await this.runJson("worktree/create", ["worktree", "create", branch], signal);
    if (!isRecord2(payload))
      return { path: "" };
    return { path: pick(payload, ["path", "worktree", "dir"]) };
  }
  async worktreeRemove(path, signal) {
    if (path.length === 0)
      fail("worktree/remove", "path");
    await this.run("worktree/remove", ["worktree", "remove", path], signal);
  }
  async worktreeList(signal) {
    return this.runJson("worktree/list", ["worktree", "list"], signal);
  }
}

// src/policy/budgets.ts
var DEFAULT_BUDGET_LIMITS = Object.freeze({
  maxConcurrentDelegations: 3,
  maxDelegationsPerSession: 10,
  maxWaitMs: 600000,
  maxDelegateOutputBytes: 50000
});
function isBudgetRejection(value) {
  return value.reason !== undefined;
}
function assertLimits(limits) {
  for (const [key, value] of Object.entries(limits)) {
    if (!Number.isSafeInteger(value) || value <= 0) {
      throw new RangeError(`budget limit ${key} must be a safe integer > 0`);
    }
  }
}

class DefaultBudgetTracker {
  limits;
  nextId = 1;
  active = new Set;
  totalThisSession = 0;
  constructor(options = {}) {
    const limits = options.limits ?? DEFAULT_BUDGET_LIMITS;
    assertLimits(limits);
    this.limits = { ...limits };
  }
  snapshot() {
    return {
      activeDelegations: this.active.size,
      totalDelegationsThisSession: this.totalThisSession,
      maxConcurrent: this.limits.maxConcurrentDelegations,
      maxPerSession: this.limits.maxDelegationsPerSession
    };
  }
  reserveDelegation() {
    if (this.totalThisSession >= this.limits.maxDelegationsPerSession) {
      return {
        reason: "session-limit",
        detail: `delegation session limit reached ` + `(${this.totalThisSession}/${this.limits.maxDelegationsPerSession})`
      };
    }
    if (this.active.size >= this.limits.maxConcurrentDelegations) {
      return {
        reason: "concurrent-limit",
        detail: `too many concurrent delegations ` + `(${this.active.size}/${this.limits.maxConcurrentDelegations})`
      };
    }
    const token = Object.freeze({ id: this.nextId });
    this.nextId += 1;
    this.active.add(token.id);
    this.totalThisSession += 1;
    return { token };
  }
  releaseDelegation(token) {
    this.active.delete(token.id);
  }
  resetSession() {
    this.active.clear();
    this.totalThisSession = 0;
  }
  maxWaitMs() {
    return this.limits.maxWaitMs;
  }
  maxOutputBytes() {
    return this.limits.maxDelegateOutputBytes;
  }
}

// src/transport/uhp/framing.ts
var MAX_FRAME_BYTES = 1024 * 1024;
var MAX_ID_LENGTH = 128;
var MAX_METHOD_LENGTH = 128;
var MAX_AUTH_LENGTH = 256;
var MAX_REMOTE_MESSAGE_CHARS = 2000;
var ID_PATTERN = /^[A-Za-z0-9._:-]+$/;
var AUTH_PATTERN = /^[!-~]+$/;

class UhpFramingError extends Error {
  reason;
  constructor(reason, options) {
    super(`uhp framing ${reason}`);
    this.name = "UhpFramingError";
    this.reason = reason;
    if (options?.cause !== undefined) {
      this.cause = options.cause;
    }
  }
}

class UhpRemoteError extends Error {
  code;
  details;
  constructor(code, message, details) {
    super(boundCodePoints(message, MAX_REMOTE_MESSAGE_CHARS));
    this.name = "UhpRemoteError";
    this.code = code;
    this.details = sanitizeBody(details ?? {});
  }
}
function isUhpRemoteError(error, code) {
  return error instanceof UhpRemoteError && (code === undefined || error.code === code);
}
function byteLength2(value) {
  return new TextEncoder().encode(value).length;
}
function boundCodePoints(value, max) {
  const points = Array.from(value);
  return points.length <= max ? value : points.slice(0, max).join("");
}
var MAX_SANITIZE_DEPTH = 32;
function sanitizeBody(value, depth = 0) {
  if (typeof value === "string") {
    return boundCodePoints(value, MAX_REMOTE_MESSAGE_CHARS);
  }
  if (depth >= MAX_SANITIZE_DEPTH)
    return "[truncated]";
  if (Array.isArray(value)) {
    return value.map((item) => sanitizeBody(item, depth + 1));
  }
  if (typeof value === "object" && value !== null) {
    const out = {};
    for (const [key, item] of Object.entries(value)) {
      out[key] = sanitizeBody(item, depth + 1);
    }
    return out;
  }
  return value;
}
function isPlainObject(value) {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    return false;
  }
  const proto = Object.getPrototypeOf(value);
  return proto === Object.prototype || proto === null;
}
function encodeUhpRequest(request) {
  if (typeof request.id !== "string" || request.id.length < 1 || request.id.length > MAX_ID_LENGTH || !ID_PATTERN.test(request.id)) {
    throw new UhpFramingError("invalid-id");
  }
  if (typeof request.method !== "string" || request.method.length < 1 || request.method.length > MAX_METHOD_LENGTH) {
    throw new UhpFramingError("invalid-method");
  }
  if (!isPlainObject(request.params)) {
    throw new UhpFramingError("invalid-params");
  }
  if (request.auth !== undefined) {
    if (typeof request.auth !== "string" || request.auth.length < 1 || request.auth.length > MAX_AUTH_LENGTH || !AUTH_PATTERN.test(request.auth)) {
      throw new UhpFramingError("invalid-auth");
    }
  }
  let json;
  try {
    json = request.auth === undefined ? JSON.stringify({
      id: request.id,
      method: request.method,
      params: request.params
    }) : JSON.stringify({
      id: request.id,
      method: request.method,
      params: request.params,
      auth: request.auth
    });
  } catch (error) {
    throw new UhpFramingError("invalid-params", { cause: error });
  }
  const frame = `${json}
`;
  if (byteLength2(frame) > MAX_FRAME_BYTES) {
    throw new UhpFramingError("frame-too-large");
  }
  return frame;
}
function decodeUhpResponse(buffer, expectedId) {
  if (byteLength2(buffer) > MAX_FRAME_BYTES) {
    throw new UhpFramingError("frame-too-large");
  }
  const firstLf = buffer.indexOf(`
`);
  if (firstLf === -1) {
    throw new UhpFramingError("incomplete-frame");
  }
  if (firstLf !== buffer.length - 1) {
    throw new UhpFramingError("trailing-data");
  }
  const line = buffer.slice(0, -1);
  let parsed;
  try {
    parsed = JSON.parse(line);
  } catch (error) {
    throw new UhpFramingError("malformed-json", { cause: error });
  }
  if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) {
    throw new UhpFramingError("bad-envelope");
  }
  const envelope = parsed;
  if (typeof envelope.id !== "string" || envelope.id !== expectedId) {
    throw new UhpFramingError("id-mismatch");
  }
  const hasResult = "result" in envelope;
  const hasError = "error" in envelope;
  if (hasResult === hasError) {
    throw new UhpFramingError("bad-envelope");
  }
  const record = parsed;
  if (hasError) {
    const body = record.error;
    if (typeof body !== "object" || body === null || Array.isArray(body) || typeof body.code !== "string" || body.code.length === 0 || typeof body.message !== "string") {
      throw new UhpFramingError("invalid-error");
    }
    const structured = body;
    return {
      ...record,
      id: envelope.id,
      error: {
        ...structured,
        code: structured.code,
        message: boundCodePoints(structured.message, MAX_REMOTE_MESSAGE_CHARS)
      }
    };
  }
  return { ...record, id: envelope.id, result: envelope.result };
}
function unwrapUhpResponse(response) {
  if (isUhpFailure(response)) {
    throw new UhpRemoteError(response.error.code, response.error.message, {
      ...response.error
    });
  }
  return response.result;
}
function isUhpFailure(response) {
  return "error" in response;
}

// src/transport/uhp/socket.ts
import { lstat } from "fs/promises";
import { createConnection } from "net";
var DEFAULT_UHP_TIMEOUT_MS = 5000;
var MAX_UHP_TIMEOUT_MS = 300000;
var MAX_SOCKET_PATH_LENGTH = 4096;
var WINDOWS_PIPE_PREFIX = "\\\\.\\pipe\\";
var SAFE_CODE_PATTERN = /^[A-Za-z0-9_.-]{1,64}$/;
function safeCauseCode(error) {
  if (typeof error === "object" && error !== null) {
    const code = error.code;
    if (typeof code === "string" && SAFE_CODE_PATTERN.test(code)) {
      return code;
    }
  }
  return;
}

class UhpTransportError extends Error {
  stage;
  mayHaveExecuted;
  causeCode;
  constructor(stage, message, options) {
    super(`uhp ${stage}: ${message}`);
    this.name = "UhpTransportError";
    this.stage = stage;
    this.mayHaveExecuted = options?.mayHaveExecuted ?? (stage !== "validate" && stage !== "connect");
    this.causeCode = options?.causeCode;
  }
}
function currentPlatform() {
  return process.platform === "win32" ? "win32" : "posix";
}
function validateSocketPath(path, platform = currentPlatform()) {
  if (typeof path !== "string" || path.length === 0 || path.length > MAX_SOCKET_PATH_LENGTH) {
    throw new UhpTransportError("validate", "socket path must be a non-empty string within length limit");
  }
  for (const ch of path) {
    const code = ch.codePointAt(0) ?? 0;
    if (code < 32 || code === 127) {
      throw new UhpTransportError("validate", "socket path must not contain control characters");
    }
  }
  if (platform === "win32") {
    if (!path.startsWith(WINDOWS_PIPE_PREFIX) || path.length === WINDOWS_PIPE_PREFIX.length || path.includes("/")) {
      throw new UhpTransportError("validate", "windows endpoint must be a local named pipe");
    }
    return;
  }
  if (!path.startsWith("/")) {
    throw new UhpTransportError("validate", "unix socket path must be absolute");
  }
}
function checkUnixSocketMetadata(meta, currentUid) {
  if (meta.isSymbolicLink()) {
    throw new UhpTransportError("validate", "unix socket endpoint must not be a symlink", { mayHaveExecuted: false });
  }
  if (!meta.isSocket()) {
    throw new UhpTransportError("validate", "unix endpoint must be a socket", { mayHaveExecuted: false });
  }
  if (meta.uid !== currentUid) {
    throw new UhpTransportError("validate", "unix socket must be owned by the current user", { mayHaveExecuted: false });
  }
  if ((meta.mode & 511) !== 384) {
    throw new UhpTransportError("validate", "unix socket must have mode 0600", { mayHaveExecuted: false });
  }
}
async function defaultValidateEndpoint(path) {
  if (process.platform === "win32")
    return;
  let stats;
  try {
    stats = await lstat(path);
  } catch (error) {
    throw new UhpTransportError("validate", "cannot stat unix socket endpoint", { mayHaveExecuted: false, causeCode: safeCauseCode(error) });
  }
  const geteuid = process.geteuid;
  if (typeof geteuid !== "function") {
    throw new UhpTransportError("validate", "cannot determine current user for socket ownership check", { mayHaveExecuted: false });
  }
  checkUnixSocketMetadata(stats, geteuid());
}
var requestCounter = 0;
function defaultRequestId() {
  requestCounter += 1;
  return `req-${requestCounter.toString(36)}-${Date.now().toString(36)}`;
}
function assertTimeoutMs(value) {
  if (!Number.isInteger(value) || value <= 0 || value > MAX_UHP_TIMEOUT_MS) {
    throw new RangeError(`uhp timeoutMs must be an integer within [1, ${MAX_UHP_TIMEOUT_MS}]; received ${String(value)}`);
  }
}

class OneShotRequester {
  connect;
  generateId;
  timeoutMs;
  validateEndpoint;
  constructor(options = {}) {
    if (options.timeoutMs !== undefined) {
      assertTimeoutMs(options.timeoutMs);
    }
    this.connect = options.connect ?? ((path) => createConnection({ path }));
    this.generateId = options.generateId ?? defaultRequestId;
    this.timeoutMs = options.timeoutMs ?? DEFAULT_UHP_TIMEOUT_MS;
    this.validateEndpoint = options.validateEndpoint ?? defaultValidateEndpoint;
  }
  async request(path, method, params, options = {}) {
    const signal = options.signal;
    if (options.timeoutMs !== undefined) {
      assertTimeoutMs(options.timeoutMs);
    }
    if (options.maxFrameBytes !== undefined) {
      assertRequestFrameLimit(options.maxFrameBytes);
    }
    const timeoutMs = options.timeoutMs ?? this.timeoutMs;
    const frameLimit = options.maxFrameBytes ?? MAX_FRAME_BYTES;
    if (signal?.aborted === true) {
      throw new UhpTransportError("aborted", "request aborted before connect", {
        mayHaveExecuted: false
      });
    }
    validateSocketPath(path);
    const id = this.generateId();
    let frame;
    try {
      frame = encodeUhpRequest({ id, method, params });
    } catch (error) {
      if (error instanceof UhpFramingError) {
        throw new UhpTransportError("validate", `invalid request: ${error.reason}`, { mayHaveExecuted: false, causeCode: safeCauseCode(error) });
      }
      throw error;
    }
    if (new TextEncoder().encode(frame).length > frameLimit) {
      throw new UhpTransportError("validate", "invalid request: frame-too-large", {
        mayHaveExecuted: false
      });
    }
    try {
      await this.validateEndpoint(path);
    } catch (error) {
      if (error instanceof UhpTransportError && error.stage === "validate" && error.mayHaveExecuted === false) {
        throw error;
      }
      throw new UhpTransportError("validate", "endpoint ownership validation failed", { mayHaveExecuted: false, causeCode: safeCauseCode(error) });
    }
    if (signal !== undefined && signal.aborted) {
      throw new UhpTransportError("aborted", "request aborted before connect", {
        mayHaveExecuted: false
      });
    }
    return new Promise((resolve, reject) => {
      let settled = false;
      let phase = "connect";
      let writeInitiated = false;
      const timer = setTimeout(() => {
        fail(new UhpTransportError("timeout", `no complete response within ${timeoutMs}ms`, { mayHaveExecuted: writeInitiated }));
      }, timeoutMs);
      timer.unref();
      let socket;
      try {
        socket = this.connect(path);
      } catch (error) {
        clearTimeout(timer);
        reject(new UhpTransportError("connect", "connect failed", {
          mayHaveExecuted: false,
          causeCode: safeCauseCode(error)
        }));
        return;
      }
      const fail = (error) => {
        if (settled)
          return;
        settled = true;
        clearTimeout(timer);
        if (signal !== undefined) {
          signal.removeEventListener("abort", onAbort);
        }
        socket.removeAllListeners();
        try {
          socket.destroy();
        } catch {}
        reject(error);
      };
      const succeed = (value) => {
        if (settled)
          return;
        settled = true;
        clearTimeout(timer);
        if (signal !== undefined) {
          signal.removeEventListener("abort", onAbort);
        }
        socket.removeAllListeners();
        try {
          socket.destroy();
        } catch {}
        resolve(value);
      };
      const onAbort = () => {
        fail(new UhpTransportError("aborted", "request aborted", {
          mayHaveExecuted: writeInitiated
        }));
      };
      if (signal !== undefined) {
        signal.addEventListener("abort", onAbort, { once: true });
      }
      if (signal?.aborted === true) {
        fail(new UhpTransportError("aborted", "request aborted", {
          mayHaveExecuted: writeInitiated
        }));
        return;
      }
      const chunks = [];
      let bytes = 0;
      const fatalUtf8 = new TextDecoder("utf-8", { fatal: true });
      socket.once("connect", () => {
        if (signal?.aborted === true) {
          fail(new UhpTransportError("aborted", "request aborted", {
            mayHaveExecuted: writeInitiated
          }));
          return;
        }
        phase = "write";
        writeInitiated = true;
        try {
          socket.write(frame, "utf8", (error) => {
            if (settled)
              return;
            if (error !== undefined && error !== null) {
              fail(new UhpTransportError("write", "write failed", {
                mayHaveExecuted: true,
                causeCode: safeCauseCode(error)
              }));
              return;
            }
            phase = "read";
          });
        } catch (error) {
          fail(new UhpTransportError("write", "write failed", {
            mayHaveExecuted: true,
            causeCode: safeCauseCode(error)
          }));
        }
      });
      socket.once("error", (error) => {
        fail(new UhpTransportError(phase, `socket ${phase} failed`, {
          mayHaveExecuted: phase !== "connect",
          causeCode: safeCauseCode(error)
        }));
      });
      socket.on("data", (chunk) => {
        if (settled)
          return;
        const buf = typeof chunk === "string" ? Buffer.from(chunk, "utf8") : chunk;
        bytes += buf.length;
        if (bytes > frameLimit) {
          fail(new UhpTransportError("protocol", "response exceeds frame limit", {
            mayHaveExecuted: true
          }));
          return;
        }
        if (buf.indexOf(10) === -1) {
          chunks.push(buf);
          return;
        }
        chunks.push(buf);
        const all = Buffer.concat(chunks);
        let text;
        try {
          text = fatalUtf8.decode(all);
        } catch (error) {
          fail(new UhpTransportError("protocol", "response is not valid UTF-8", { mayHaveExecuted: true, causeCode: safeCauseCode(error) }));
          return;
        }
        let response;
        try {
          response = decodeUhpResponse(text, id);
        } catch (error) {
          if (error instanceof UhpFramingError) {
            fail(new UhpTransportError("protocol", `invalid response: ${error.reason}`, { mayHaveExecuted: true, causeCode: safeCauseCode(error) }));
          } else {
            fail(error);
          }
          return;
        }
        try {
          succeed(unwrapUhpResponse(response));
        } catch (error) {
          fail(error);
        }
      });
      socket.once("close", () => {
        if (settled)
          return;
        fail(new UhpTransportError(phase === "connect" ? "connect" : "read", "connection closed before a complete response"));
      });
    });
  }
}

class AdaptiveUhpRequester {
  inner;
  limit;
  constructor(inner, initialMaxFrameBytes) {
    this.inner = inner;
    if (initialMaxFrameBytes !== undefined) {
      assertRequestFrameLimit(initialMaxFrameBytes);
    }
    this.limit = initialMaxFrameBytes ?? MAX_FRAME_BYTES;
  }
  getMaxFrameBytes() {
    return this.limit;
  }
  setMaxFrameBytes(value) {
    assertRequestFrameLimit(value);
    this.limit = value;
  }
  adaptToAdvertised(frameBytes) {
    if (!Number.isSafeInteger(frameBytes) || frameBytes <= 0) {
      throw new RangeError(`uhp advertised frame_bytes must be a safe integer > 0; received ${String(frameBytes)}`);
    }
    this.setMaxFrameBytes(Math.min(frameBytes, MAX_FRAME_BYTES));
  }
  async request(path, method, params, options = {}) {
    const explicit = options.maxFrameBytes;
    if (explicit !== undefined) {
      assertRequestFrameLimit(explicit);
    }
    return this.inner.request(path, method, params, {
      ...options,
      maxFrameBytes: Math.min(explicit ?? this.limit, this.limit)
    });
  }
}
function assertRequestFrameLimit(value) {
  if (!Number.isInteger(value) || value < 1 || value > MAX_FRAME_BYTES) {
    throw new RangeError(`uhp maxFrameBytes must be an integer within [1, ${MAX_FRAME_BYTES}]; received ${String(value)}`);
  }
}

// src/operations/uhp-ops.ts
var OPS_UHP_METHODS = {
  agentStart: "agent.start",
  agentPrompt: "agent.prompt",
  agentFork: "agent.fork",
  agentWait: "agent.wait",
  agentWaitOutput: "wait.output",
  agentRead: "agent.read",
  agentGet: "agent.get",
  agentSessions: "agent.sessions",
  taskAdd: "task.add",
  taskClaim: "task.claim",
  taskNext: "task.next",
  taskStart: "task.start",
  taskUpdate: "task.update",
  taskDone: "task.done",
  taskMerge: "task.merge",
  taskRelease: "task.release",
  taskDelete: "task.delete",
  taskGet: "task.get",
  taskList: "task.list",
  leaseAcquire: "lease.acquire",
  leaseRelease: "lease.release",
  leaseList: "lease.list",
  worktreeCreate: "worktree.create",
  worktreeRemove: "worktree.remove",
  worktreeList: "worktree.list"
};

class UhpOpsError extends Error {
  method;
  constructor(method, detail) {
    super(detail !== undefined ? `uhp ops ${method} failed: ${detail}` : `uhp ops ${method} failed`);
    this.name = "UhpOpsError";
    this.method = method;
  }
}
var MAX_NAME_LENGTH = 128;
var MAX_TITLE_LENGTH = 512;
var MAX_TEXT_CHARS = 64 * 1024;
var MAX_GATE_LENGTH = 1024;
var MAX_STATUS_LENGTH = 64;
var MAX_NOTE_LENGTH = 4096;
var MAX_ARGS = 32;
var MAX_ARG_LENGTH = 4096;
var MAX_PATHS = 64;
var MAX_READ_LINES = 200;
var MAX_TIMEOUT_S = 3600;
var MAX_LEASE_PATH_LENGTH2 = 4096;
function isRecord3(value) {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
function fail2(method, hint) {
  throw new UhpOpsError(method, hint);
}
function needString(method, value, hint, maxLength, allowEmpty) {
  if (typeof value !== "string" || !allowEmpty && value.length === 0 || value.length > maxLength) {
    fail2(method, hint);
  }
  return value;
}
function checkedPaneId(method, value, hint) {
  const result = validatePaneId(value);
  if (!result.valid)
    fail2(method, `${hint}: ${result.reason}`);
  return value;
}
function checkedTaskId(method, value, hint) {
  const result = validateTaskId(value);
  if (!result.valid)
    fail2(method, `${hint}: ${result.reason}`);
  return value;
}
function checkedTimeoutS(method, value) {
  if (!Number.isSafeInteger(value) || value < 1 || value > MAX_TIMEOUT_S) {
    fail2(method, "timeout_s must be an integer within [1, 3600]");
  }
  return value;
}
function shortString(value, maxLength) {
  if (typeof value !== "string")
    return "";
  return value.length <= maxLength ? value : value.slice(0, maxLength);
}
function pickString(record, keys) {
  for (const key of keys) {
    const value = record[key];
    if (typeof value === "string" && value.length > 0)
      return value;
  }
  return "";
}

class UhpOpsClient {
  requester;
  endpoint;
  constructor(options) {
    if (options === null || typeof options !== "object" || typeof options.endpoint !== "string" || options.endpoint.length === 0) {
      throw new TypeError("UhpOpsClient requires a non-empty endpoint");
    }
    if (options.requester === null || options.requester === undefined || typeof options.requester.request !== "function") {
      throw new TypeError("UhpOpsClient requires a requester");
    }
    this.requester = options.requester;
    this.endpoint = options.endpoint;
  }
  call(method, params, signal) {
    return this.requester.request(this.endpoint, method, params, {
      ...signal === undefined ? {} : { signal }
    });
  }
  async agentStart(params, signal) {
    const method = OPS_UHP_METHODS.agentStart;
    const name = needString(method, params.name, "name", MAX_NAME_LENGTH, false);
    const kindResult = validateAgentKind(params.kind);
    if (!kindResult.valid)
      fail2(method, `kind: ${kindResult.reason}`);
    const body = { name, kind: params.kind };
    if (params.anchor !== undefined) {
      body.anchor = checkedPaneId(method, params.anchor, "anchor");
    }
    if (params.pane !== undefined) {
      body.pane = checkedPaneId(method, params.pane, "pane");
    }
    if (params.down !== undefined)
      body.down = params.down === true;
    if (params.args !== undefined) {
      if (!Array.isArray(params.args) || params.args.length > MAX_ARGS) {
        fail2(method, "args");
      }
      for (const arg of params.args) {
        if (typeof arg !== "string" || arg.length > MAX_ARG_LENGTH) {
          fail2(method, "args");
        }
      }
      body.args = [...params.args];
    }
    const result = await this.call(method, body, signal);
    if (!isRecord3(result))
      fail2(method, "result object");
    return {
      paneId: pickString(result, ["pane_id", "pane", "paneId", "id"]),
      name: shortString(isRecord3(result.agent) ? result.agent : result.name ?? name, MAX_NAME_LENGTH),
      kind: shortString(result.kind, MAX_NAME_LENGTH),
      status: shortString(result.status, MAX_STATUS_LENGTH)
    };
  }
  async agentPrompt(params, signal) {
    const method = OPS_UHP_METHODS.agentPrompt;
    if (params.target.length === 0)
      fail2(method, "target");
    const prompt = validatePrompt(params.text);
    if (!prompt.valid)
      fail2(method, `text: ${prompt.reason}`);
    const body = {
      target: params.target,
      text: params.text
    };
    if (params.wait !== undefined)
      body.wait = params.wait === true;
    if (params.until !== undefined)
      body.until = params.until;
    if (params.timeoutS !== undefined) {
      body.timeout_s = checkedTimeoutS(method, params.timeoutS);
    }
    await this.call(method, body, signal);
  }
  async agentFork(params, signal) {
    const method = OPS_UHP_METHODS.agentFork;
    if (params.target.length === 0)
      fail2(method, "target");
    const body = { target: params.target };
    if (params.name !== undefined) {
      body.name = needString(method, params.name, "name", MAX_NAME_LENGTH, false);
    }
    const result = await this.call(method, body, signal);
    if (!isRecord3(result))
      fail2(method, "result object");
    const paneId = pickString(result, ["pane_id", "pane", "paneId", "id"]);
    const name = pickString(result, ["name"]);
    return name.length > 0 ? { paneId, name } : { paneId };
  }
  async agentWait(params, signal) {
    const paneId = checkedPaneId(OPS_UHP_METHODS.agentWait, params.paneId, "pane");
    if (params.until === undefined && params.matchText === undefined) {
      fail2(OPS_UHP_METHODS.agentWait, "until or matchText is required");
    }
    if (params.until !== undefined && params.matchText !== undefined) {
      fail2(OPS_UHP_METHODS.agentWait, "until and matchText are mutually exclusive");
    }
    if (params.matchText !== undefined) {
      return this.waitForOutput(params, signal);
    }
    const method = OPS_UHP_METHODS.agentWait;
    const body = {
      pane: paneId,
      status: params.until
    };
    if (params.timeoutS !== undefined) {
      body.timeout_s = checkedTimeoutS(method, params.timeoutS);
    }
    const result = await this.call(method, body, signal);
    if (!isRecord3(result))
      fail2(method, "result object");
    return {
      paneId: pickString(result, ["pane", "pane_id", "paneId"]) || paneId,
      status: shortString(result.status, MAX_STATUS_LENGTH),
      matched: typeof result.matched === "boolean" ? result.matched : false
    };
  }
  async waitForOutput(params, signal) {
    const method = OPS_UHP_METHODS.agentWaitOutput;
    const match = needString(method, params.matchText, "match", MAX_TEXT_CHARS, false);
    const body = {
      pane: checkedPaneId(method, params.paneId, "pane"),
      match
    };
    if (params.timeoutS !== undefined) {
      body.timeout_s = checkedTimeoutS(method, params.timeoutS);
    }
    const result = await this.call(method, body, signal);
    if (!isRecord3(result))
      fail2(method, "result object");
    return {
      paneId: pickString(result, ["pane", "pane_id", "paneId"]) || params.paneId,
      status: shortString(result.status, MAX_STATUS_LENGTH),
      matched: typeof result.matched === "boolean" ? result.matched : false,
      ...typeof result.output === "string" ? { output: result.output.slice(0, 2048) } : {}
    };
  }
  async agentRead(params, signal) {
    const method = OPS_UHP_METHODS.agentRead;
    const target = needString(method, params.target, "target", 256, false);
    const lines = params.lines ?? 50;
    if (!Number.isSafeInteger(lines) || lines < 1 || lines > MAX_READ_LINES) {
      fail2(method, "lines must be an integer within [1, 200]");
    }
    const result = await this.call(method, { pane: target, lines }, signal);
    if (typeof result === "string")
      return result.slice(0, MAX_TEXT_CHARS);
    if (isRecord3(result) && typeof result.output === "string") {
      return result.output.slice(0, MAX_TEXT_CHARS);
    }
    return JSON.stringify(result).slice(0, MAX_TEXT_CHARS);
  }
  async agentGet(paneId, signal) {
    const method = OPS_UHP_METHODS.agentGet;
    return this.call(method, { pane: checkedPaneId(method, paneId, "pane") }, signal);
  }
  async agentSessions(signal) {
    return this.call(OPS_UHP_METHODS.agentSessions, {}, signal);
  }
  async taskAdd(params, signal) {
    const method = OPS_UHP_METHODS.taskAdd;
    const title = needString(method, params.title, "title", MAX_TITLE_LENGTH, false);
    const body = { title };
    if (params.paths !== undefined) {
      if (!Array.isArray(params.paths) || params.paths.length > MAX_PATHS) {
        fail2(method, "paths");
      }
      const checked = [];
      for (const entry of params.paths) {
        const result = validateLeasePath(entry);
        if (!result.valid)
          fail2(method, `paths: ${result.reason}`);
        checked.push(entry);
      }
      body.paths = checked;
    }
    if (params.dependsOn !== undefined) {
      if (!Array.isArray(params.dependsOn) || params.dependsOn.length > MAX_PATHS) {
        fail2(method, "dependsOn");
      }
      body.deps = params.dependsOn.map((id) => checkedTaskId(method, id, "dep"));
    }
    if (params.gate !== undefined) {
      body.gate = needString(method, params.gate, "gate", MAX_GATE_LENGTH, false);
    }
    const result = await this.call(method, body, signal);
    const task = isRecord3(result) && isRecord3(result.task) ? result.task : result;
    if (!isRecord3(task))
      fail2(method, "result object");
    return {
      taskId: pickString(task, ["id", "task_id", "taskId"]),
      title: shortString(task.title ?? title, MAX_TITLE_LENGTH)
    };
  }
  async taskClaim(taskId, signal) {
    const method = OPS_UHP_METHODS.taskClaim;
    await this.call(method, { id: checkedTaskId(method, taskId, "task") }, signal);
  }
  async taskNext(params, signal) {
    const method = OPS_UHP_METHODS.taskNext;
    const body = {};
    if (params.start !== undefined)
      body.start = params.start === true;
    if (params.agent !== undefined) {
      body.agent = needString(method, params.agent, "agent", MAX_NAME_LENGTH, false);
    }
    if (params.mode !== undefined)
      body.mode = params.mode;
    const result = await this.call(method, body, signal);
    const task = isRecord3(result) && isRecord3(result.task) ? result.task : result;
    if (!isRecord3(task))
      fail2(method, "result object");
    const started = isRecord3(result) && typeof result.started === "boolean" ? result.started : params.start === true;
    return {
      taskId: pickString(task, ["id", "task_id", "taskId"]),
      title: shortString(task.title, MAX_TITLE_LENGTH),
      started
    };
  }
  async taskStart(params, signal) {
    const method = OPS_UHP_METHODS.taskStart;
    const body = {
      id: checkedTaskId(method, params.taskId, "task")
    };
    if (params.branch !== undefined) {
      const branch = validateBranchName(params.branch);
      if (!branch.valid)
        fail2(method, `branch: ${branch.reason}`);
      body.branch = params.branch;
    }
    if (params.agent !== undefined) {
      body.agent = needString(method, params.agent, "agent", MAX_NAME_LENGTH, false);
    }
    if (params.mode !== undefined)
      body.mode = params.mode;
    const result = await this.call(method, body, signal);
    if (!isRecord3(result))
      fail2(method, "result object");
    const task = isRecord3(result.task) ? result.task : result;
    const nested = isRecord3(task) ? task : {};
    const out = {
      taskId: params.taskId
    };
    const branch = pickString(result, ["branch"]) || pickString(nested, ["branch"]);
    const worktree = pickString(result, ["worktree"]) || pickString(nested, ["worktree"]);
    const paneId = pickString(result, ["pane_id", "pane", "paneId"]) || pickString(nested, ["pane_id", "pane", "paneId"]);
    return {
      ...out,
      ...branch.length > 0 ? { branch } : {},
      ...worktree.length > 0 ? { worktree } : {},
      ...paneId.length > 0 ? { paneId } : {}
    };
  }
  async taskUpdate(params, signal) {
    const method = OPS_UHP_METHODS.taskUpdate;
    const body = {
      id: checkedTaskId(method, params.taskId, "task")
    };
    if (params.status !== undefined) {
      body.status = needString(method, params.status, "status", MAX_STATUS_LENGTH, false);
    }
    if (params.output !== undefined) {
      body.output = needString(method, params.output, "output", MAX_TEXT_CHARS, true);
    }
    if (params.note !== undefined) {
      body.note = needString(method, params.note, "note", MAX_NOTE_LENGTH, true);
    }
    await this.call(method, body, signal);
  }
  async taskById(method, taskId, signal) {
    await this.call(method, { id: checkedTaskId(method, taskId, "task") }, signal);
  }
  async taskDone(taskId, signal) {
    return this.taskById(OPS_UHP_METHODS.taskDone, taskId, signal);
  }
  async taskMerge(taskId, signal) {
    return this.taskById(OPS_UHP_METHODS.taskMerge, taskId, signal);
  }
  async taskRelease(taskId, signal) {
    return this.taskById(OPS_UHP_METHODS.taskRelease, taskId, signal);
  }
  async taskDelete(taskId, signal) {
    return this.taskById(OPS_UHP_METHODS.taskDelete, taskId, signal);
  }
  async taskGet(taskId, signal) {
    const method = OPS_UHP_METHODS.taskGet;
    return this.call(method, { id: checkedTaskId(method, taskId, "task") }, signal);
  }
  async taskList(signal) {
    return this.call(OPS_UHP_METHODS.taskList, {}, signal);
  }
  async leaseAcquire(params, signal) {
    const method = OPS_UHP_METHODS.leaseAcquire;
    if (!Array.isArray(params.paths) || params.paths.length === 0) {
      fail2(method, "paths must be non-empty");
    }
    if (params.paths.length > MAX_PATHS)
      fail2(method, "paths");
    const checked = [];
    for (const entry of params.paths) {
      const result = validateLeasePath(entry);
      if (!result.valid)
        fail2(method, `paths: ${result.reason}`);
      checked.push(entry);
    }
    await this.call(method, {
      paths: checked,
      task: checkedTaskId(method, params.taskId, "task")
    }, signal);
  }
  async leaseRelease(taskId, signal) {
    const method = OPS_UHP_METHODS.leaseRelease;
    await this.call(method, { id: checkedTaskId(method, taskId, "task") }, signal);
  }
  async leaseList(signal) {
    return this.call(OPS_UHP_METHODS.leaseList, {}, signal);
  }
  async worktreeCreate(branch, signal) {
    const method = OPS_UHP_METHODS.worktreeCreate;
    const checked = validateBranchName(branch);
    if (!checked.valid)
      fail2(method, `branch: ${checked.reason}`);
    const result = await this.call(method, { branch }, signal);
    if (!isRecord3(result))
      fail2(method, "result object");
    return {
      path: pickString(result, ["path", "worktree", "dir"])
    };
  }
  async worktreeRemove(path, signal) {
    const method = OPS_UHP_METHODS.worktreeRemove;
    await this.call(method, {
      path: needString(method, path, "path", MAX_LEASE_PATH_LENGTH2, false)
    }, signal);
  }
  async worktreeList(signal) {
    return this.call(OPS_UHP_METHODS.worktreeList, {}, signal);
  }
}

// src/operations/router.ts
class OpsRejectedError extends Error {
  kind;
  action;
  constructor(kind, action, reason) {
    super(`${action} rejected (${kind}): ${reason}`);
    this.name = "OpsRejectedError";
    this.kind = kind;
    this.action = action;
  }
}
function abortError(signal) {
  const reason = signal.reason;
  if (reason instanceof Error)
    return reason;
  return new Error("operation aborted before start");
}
function isAbortSignal(signal) {
  return signal?.aborted === true;
}

class OpsRouter {
  uhp;
  cli;
  telemetry;
  policy;
  budget;
  selfGuard;
  loops;
  ownPaneId;
  constructor(options) {
    if (options.ownPaneId.length === 0) {
      throw new Error("OpsRouter requires a non-empty ownPaneId");
    }
    this.uhp = options.uhp;
    this.cli = options.cli;
    this.telemetry = options.telemetry;
    this.policy = options.policy;
    this.budget = options.budget ?? new DefaultBudgetTracker;
    this.selfGuard = options.guards?.self ?? new SelfDelegationGuard(options.ownPaneId);
    this.loops = options.guards?.loops ?? new LoopDetector;
    this.ownPaneId = options.ownPaneId;
  }
  budgetSnapshot() {
    return this.budget.snapshot();
  }
  async announced(method, signal) {
    const snap = this.telemetry.healthSnapshot();
    if (snap.hasCapabilities) {
      return snap.supportedMethods.includes(method);
    }
    if (snap.coolingDown)
      return false;
    let caps;
    try {
      caps = await this.telemetry.forceReconnect(signal === undefined ? undefined : { signal });
    } catch {
      return false;
    }
    return caps?.supports(method) ?? false;
  }
  async readThrough(method, runUhp, runCli, signal) {
    if (isAbortSignal(signal))
      throw abortError(signal);
    if (await this.announced(method, signal)) {
      try {
        return await runUhp(signal);
      } catch (error) {
        if (error instanceof UhpTransportError && error.stage === "aborted") {
          throw error;
        }
        if (isUhpRemoteError(error))
          throw error;
        if (error instanceof UhpTransportError && !error.mayHaveExecuted) {
          return runCli(signal);
        }
        throw error;
      }
    }
    return runCli(signal);
  }
  async agentGet(paneId, signal) {
    return this.readThrough(OPS_UHP_METHODS.agentGet, (sig) => this.uhp.agentGet(paneId, sig), (sig) => this.cli.agentGet(paneId, sig), signal);
  }
  async agentSessions(signal) {
    return this.readThrough(OPS_UHP_METHODS.agentSessions, (sig) => this.uhp.agentSessions(sig), (sig) => this.cli.agentSessions(sig), signal);
  }
  async agentRead(params, signal) {
    return this.readThrough(OPS_UHP_METHODS.agentRead, (sig) => this.uhp.agentRead(params, sig), (sig) => this.cli.agentRead(params, sig), signal);
  }
  async agentWait(params, signal) {
    return this.readThrough(params.matchText !== undefined ? OPS_UHP_METHODS.agentWaitOutput : OPS_UHP_METHODS.agentWait, (sig) => this.uhp.agentWait(params, sig), (sig) => this.cli.agentWait(params, sig), signal);
  }
  async taskGet(taskId, signal) {
    return this.readThrough(OPS_UHP_METHODS.taskGet, (sig) => this.uhp.taskGet(taskId, sig), (sig) => this.cli.taskGet(taskId, sig), signal);
  }
  async taskList(signal) {
    return this.readThrough(OPS_UHP_METHODS.taskList, (sig) => this.uhp.taskList(sig), (sig) => this.cli.taskList(sig), signal);
  }
  async leaseList(signal) {
    return this.readThrough(OPS_UHP_METHODS.leaseList, (sig) => this.uhp.leaseList(sig), (sig) => this.cli.leaseList(sig), signal);
  }
  async worktreeList(signal) {
    return this.readThrough(OPS_UHP_METHODS.worktreeList, (sig) => this.uhp.worktreeList(sig), (sig) => this.cli.worktreeList(sig), signal);
  }
  async gateWrite(action, call) {
    const resolution = await this.policy.gate(action, call.ctx, {
      summary: call.summary,
      ...call.detail === undefined ? {} : { detail: call.detail }
    });
    if (!resolution.allowed) {
      throw new OpsRejectedError("policy", action, resolution.reason);
    }
  }
  guardPaneTarget(action, target) {
    if (target === undefined || target.length === 0)
      return;
    const self = this.selfGuard.check(target);
    if (!self.allowed) {
      throw new OpsRejectedError("self", action, self.detail);
    }
    const loop = this.loops.recordArc(this.ownPaneId, target);
    if (loop.detected) {
      throw new OpsRejectedError("loop", action, `delegation loop detected: ${loop.chain.join(" \u2192 ")}`);
    }
  }
  async writeThrough(method, runUhp, runCli, signal) {
    if (isAbortSignal(signal))
      throw abortError(signal);
    if (await this.announced(method, signal)) {
      try {
        return await runUhp(signal);
      } catch (error) {
        if (error instanceof UhpTransportError && error.stage === "aborted") {
          throw error;
        }
        if (isUhpRemoteError(error))
          throw error;
        if (error instanceof UhpTransportError && !error.mayHaveExecuted) {
          return runCli(signal);
        }
        throw error;
      }
    }
    return runCli(signal);
  }
  async agentStart(params, call, signal) {
    const action = "agent/start";
    await this.gateWrite(action, call);
    this.guardPaneTarget(action, params.pane ?? params.anchor);
    const reservation = this.budget.reserveDelegation();
    if (isBudgetRejection(reservation)) {
      throw new OpsRejectedError("budget", action, reservation.detail);
    }
    try {
      return await this.writeThrough(OPS_UHP_METHODS.agentStart, (sig) => this.uhp.agentStart(params, sig), (sig) => this.cli.agentStart(params, sig), signal);
    } finally {
      this.budget.releaseDelegation(reservation.token);
    }
  }
  async agentPrompt(params, call, signal) {
    const action = "agent/prompt";
    await this.gateWrite(action, call);
    this.guardPaneTarget(action, params.target);
    return this.writeThrough(OPS_UHP_METHODS.agentPrompt, (sig) => this.uhp.agentPrompt(params, sig), (sig) => this.cli.agentPrompt(params, sig), signal);
  }
  async agentFork(params, call, signal) {
    const action = "agent/fork";
    await this.gateWrite(action, call);
    this.guardPaneTarget(action, params.target);
    return this.writeThrough(OPS_UHP_METHODS.agentFork, (sig) => this.uhp.agentFork(params, sig), (sig) => this.cli.agentFork(params, sig), signal);
  }
  async taskAdd(params, call, signal) {
    const action = "task/add";
    await this.gateWrite(action, call);
    return this.writeThrough(OPS_UHP_METHODS.taskAdd, (sig) => this.uhp.taskAdd(params, sig), (sig) => this.cli.taskAdd(params, sig), signal);
  }
  async taskClaim(taskId, call, signal) {
    const action = "task/claim";
    await this.gateWrite(action, call);
    return this.writeThrough(OPS_UHP_METHODS.taskClaim, (sig) => this.uhp.taskClaim(taskId, sig), (sig) => this.cli.taskClaim(taskId, sig), signal);
  }
  async taskNext(params, call, signal) {
    const action = "task/start";
    if (params.start === true) {
      await this.gateWrite(action, call);
      const next = await this.writeThrough(OPS_UHP_METHODS.taskNext, (sig) => this.uhp.taskNext(params, sig), (sig) => this.cli.taskNext({ ...params, start: false }, sig), signal);
      if (next.taskId.length === 0)
        return next;
      const started = await this.writeThrough(OPS_UHP_METHODS.taskStart, (sig) => this.uhp.taskStart({
        taskId: next.taskId,
        ...params.agent === undefined ? {} : { agent: params.agent },
        ...params.mode === undefined ? {} : { mode: params.mode }
      }, sig), (sig) => this.cli.taskStart({
        taskId: next.taskId,
        ...params.agent === undefined ? {} : { agent: params.agent },
        ...params.mode === undefined ? {} : { mode: params.mode }
      }, sig), signal);
      return { ...next, started: true };
    }
    await this.gateWrite("task/claim", call);
    return this.writeThrough(OPS_UHP_METHODS.taskNext, (sig) => this.uhp.taskNext(params, sig), (sig) => this.cli.taskNext(params, sig), signal);
  }
  async taskStart(params, call, signal) {
    const action = "task/start";
    await this.gateWrite(action, call);
    return this.writeThrough(OPS_UHP_METHODS.taskStart, (sig) => this.uhp.taskStart(params, sig), (sig) => this.cli.taskStart(params, sig), signal);
  }
  async taskUpdate(params, call, signal) {
    const action = "task/update";
    await this.gateWrite(action, call);
    return this.writeThrough(OPS_UHP_METHODS.taskUpdate, (sig) => this.uhp.taskUpdate(params, sig), (sig) => this.cli.taskUpdate(params, sig), signal);
  }
  async taskDone(taskId, call, signal) {
    const action = "task/done";
    await this.gateWrite(action, call);
    return this.writeThrough(OPS_UHP_METHODS.taskDone, (sig) => this.uhp.taskDone(taskId, sig), (sig) => this.cli.taskDone(taskId, sig), signal);
  }
  async taskMerge(taskId, call, signal) {
    const action = "task/merge";
    await this.gateWrite(action, call);
    return this.writeThrough(OPS_UHP_METHODS.taskMerge, (sig) => this.uhp.taskMerge(taskId, sig), (sig) => this.cli.taskMerge(taskId, sig), signal);
  }
  async taskRelease(taskId, call, signal) {
    const action = "task/release";
    await this.gateWrite(action, call);
    return this.writeThrough(OPS_UHP_METHODS.taskRelease, (sig) => this.uhp.taskRelease(taskId, sig), (sig) => this.cli.taskRelease(taskId, sig), signal);
  }
  async taskDelete(taskId, call, signal) {
    const action = "task/delete";
    await this.gateWrite(action, call);
    return this.writeThrough(OPS_UHP_METHODS.taskDelete, (sig) => this.uhp.taskDelete(taskId, sig), (sig) => this.cli.taskDelete(taskId, sig), signal);
  }
  async leaseAcquire(params, call, signal) {
    const action = "lease/acquire";
    await this.gateWrite(action, call);
    return this.writeThrough(OPS_UHP_METHODS.leaseAcquire, (sig) => this.uhp.leaseAcquire(params, sig), (sig) => this.cli.leaseAcquire(params, sig), signal);
  }
  async leaseRelease(taskId, call, signal) {
    const action = "lease/release";
    await this.gateWrite(action, call);
    return this.writeThrough(OPS_UHP_METHODS.leaseRelease, (sig) => this.uhp.leaseRelease(taskId, sig), (sig) => this.cli.leaseRelease(taskId, sig), signal);
  }
  async worktreeCreate(branch, call, signal) {
    const action = "worktree/create";
    await this.gateWrite(action, call);
    return this.writeThrough(OPS_UHP_METHODS.worktreeCreate, (sig) => this.uhp.worktreeCreate(branch, sig), (sig) => this.cli.worktreeCreate(branch, sig), signal);
  }
  async worktreeRemove(path, call, signal) {
    const action = "worktree/remove";
    await this.gateWrite(action, call);
    return this.writeThrough(OPS_UHP_METHODS.worktreeRemove, (sig) => this.uhp.worktreeRemove(path, sig), (sig) => this.cli.worktreeRemove(path, sig), signal);
  }
}

// src/policy/permissions.ts
function defaultAutonomy(now = Date.now()) {
  return { mode: "confirm", setAt: now, setBy: "default" };
}
var CLASSIFICATIONS = {
  "agent/start": { class: "delegate", reinforced: false },
  "agent/prompt": { class: "delegate", reinforced: false },
  "agent/fork": { class: "delegate", reinforced: false },
  "task/add": { class: "task-lifecycle", reinforced: false },
  "task/claim": { class: "task-lifecycle", reinforced: false },
  "task/start": { class: "task-lifecycle", reinforced: false },
  "task/update": { class: "task-lifecycle", reinforced: false },
  "task/done": { class: "task-lifecycle", reinforced: false },
  "task/release": { class: "task-lifecycle", reinforced: false },
  "task/delete": { class: "high-impact", reinforced: true },
  "task/merge": { class: "high-impact", reinforced: true },
  "lease/acquire": { class: "task-lifecycle", reinforced: false },
  "lease/release": { class: "task-lifecycle", reinforced: false },
  "worktree/create": { class: "task-lifecycle", reinforced: false },
  "worktree/remove": { class: "high-impact", reinforced: true }
};
var POLICY_ACTIONS = Object.freeze(Object.keys(CLASSIFICATIONS));
var MAX_SUMMARY_CHARS = 500;
function boundSummary(value) {
  const points = Array.from(value);
  if (points.length <= MAX_SUMMARY_CHARS)
    return value;
  return `${points.slice(0, MAX_SUMMARY_CHARS - 3).join("")}...`;
}

class DefaultPolicyEngine {
  autonomy;
  now;
  constructor(options = {}) {
    this.now = options.now ?? Date.now;
    this.autonomy = defaultAutonomy(this.now());
  }
  classify(action) {
    const row = CLASSIFICATIONS[action];
    if (row === undefined) {
      throw new Error(`unknown policy action: ${action}`);
    }
    return {
      action,
      class: row.class,
      requiresConfirmation: true,
      failClosedWithoutUI: true,
      reinforced: row.reinforced
    };
  }
  mode() {
    return this.autonomy.mode;
  }
  setMode(mode, setBy) {
    this.autonomy = { mode, setAt: this.now(), setBy };
  }
  resetSession(now) {
    this.autonomy = defaultAutonomy(now ?? this.now());
  }
  async gate(action, ctx, details) {
    let classification;
    try {
      classification = this.classify(action);
    } catch {
      return { allowed: false, reason: `unknown action: ${action}` };
    }
    if (this.autonomy.mode === "restricted" && classification.class === "high-impact") {
      return {
        allowed: false,
        reason: `action ${action} is disabled in restricted mode`
      };
    }
    if (!ctx.hasUI) {
      return {
        allowed: false,
        reason: `action ${action} requires UI confirmation and no UI is available`
      };
    }
    const summary = boundSummary(details.summary);
    const title = classification.reinforced ? `Confirm high-impact Luvus action: ${action}` : `Confirm Luvus action: ${action}`;
    const message = details.detail !== undefined && details.detail.length > 0 ? `${summary}
${boundSummary(details.detail)}` : summary;
    let confirmed;
    try {
      confirmed = await ctx.ui.confirm(title, message);
    } catch {
      return { allowed: false, reason: `confirmation failed for ${action}` };
    }
    if (!confirmed) {
      return { allowed: false, reason: `declined by user: ${action}` };
    }
    return { allowed: true };
  }
}

// src/inspection/health.ts
function buildHealthSnapshot(routerHealth, coordinatorStatus, fleetSnapshot) {
  const hasCapabilities = routerHealth?.hasCapabilities === true;
  const coolingDown = routerHealth?.coolingDown === true;
  const resolved = routerHealth === undefined ? "disconnected" : routerHealth.hasCapabilities ? "uhp" : "cli";
  return {
    transport: resolved,
    transportDetail: {
      hasCapabilities,
      coolingDown,
      probeFailures: routerHealth?.probeFailures ?? 0,
      outcomeUncertain: routerHealth?.outcomeUncertain === true,
      supportedMethodCount: routerHealth?.supportedMethods.length ?? 0
    },
    coordinator: {
      present: coordinatorStatus !== undefined,
      phase: coordinatorStatus?.phase ?? "absent",
      generation: coordinatorStatus?.generation,
      resyncCount: coordinatorStatus?.resyncCount ?? 0,
      backoffActive: coordinatorStatus?.backoff.canProbe === false
    },
    fleet: {
      cachedAgentCount: fleetSnapshot?.size ?? 0,
      generation: fleetSnapshot?.generation
    }
  };
}
function shortGeneration(generation) {
  if (generation === undefined || generation.length === 0)
    return;
  return generation.slice(0, 8);
}
function formatHealthLine(health) {
  if (health.transport === "disconnected")
    return "disconnected";
  const gen = shortGeneration(health.coordinator.generation ?? health.fleet.generation);
  const agents = `${health.fleet.cachedAgentCount} agent${health.fleet.cachedAgentCount === 1 ? "" : "s"}`;
  if (health.transport === "cli") {
    return gen === undefined ? `CLI \xB7 ${agents}` : `CLI \xB7 gen ${gen} \xB7 ${agents}`;
  }
  const phase = health.coordinator.present && health.coordinator.phase !== "live" ? ` \xB7 ${health.coordinator.phase}` : " \xB7 live";
  const base = gen === undefined ? `UHP \xB7 ${agents}` : `UHP \xB7 gen ${gen} \xB7 ${agents}`;
  return `${base}${phase}`;
}

// src/inspection/commands.ts
function buildLuvusOverview(deps) {
  const health = buildHealthSnapshot(deps.router?.healthSnapshot(), deps.coordinator?.status(), deps.fleetCache.snapshot());
  const fleet = deps.fleetCache.snapshot();
  const byStatus = new Map;
  for (const entry of fleet.entries) {
    byStatus.set(entry.status, (byStatus.get(entry.status) ?? 0) + 1);
  }
  const agents = fleet.size === 0 ? "Agents: none cached" : `Agents: ${[...byStatus.entries()].map(([status, count]) => `${count} ${status}`).join(", ")}`;
  const lines = [
    `Transport: ${formatHealthLine(health)}`,
    agents,
    `Coordinator: ${health.coordinator.present ? health.coordinator.phase : "absent"}, ${health.coordinator.resyncCount} resyncs`
  ];
  if (deps.config.taskId !== undefined) {
    lines.push(`ORCH task: ${deps.config.taskId}`);
  }
  if (deps.metrics !== undefined) {
    try {
      lines.push(`Metrics: ${formatMetricsLine(deps.metrics.snapshot())}`);
    } catch {}
  }
  return lines;
}
function buildAgentRows(deps) {
  const entries = deps.fleetCache.list().slice(0, 50);
  return entries.map((entry) => `${entry.paneId} ${entry.agent} ${entry.status} ${entry.authority}` + `${entry.workspaceName !== undefined ? ` ${entry.workspaceName}:${entry.tabIndex}` : ""}` + `${entry.focused ? " [focused]" : ""}`);
}
function registerInspectionCommands(pi, deps) {
  pi.registerCommand("luvus", {
    description: "Show Luvus bridge health and fleet summary",
    handler: async (_args, ctx) => {
      const lines = buildLuvusOverview(deps);
      ctx.ui.notify(lines.join(`
`), "info");
    }
  });
  pi.registerCommand("luvus-agents", {
    description: "List cached Luvus agents",
    handler: async (_args, ctx) => {
      const rows = buildAgentRows(deps);
      if (rows.length === 0) {
        ctx.ui.notify("No agents in fleet cache", "info");
        return;
      }
      const picked = await ctx.ui.select("Luvus agents", rows);
      if (picked !== undefined) {
        const detail = deps.fleetCache.get(picked.split(" ")[0] ?? "");
        if (detail !== undefined) {
          ctx.ui.notify(`Agent ${detail.paneId}: ${detail.agent} ${detail.status} ${detail.authority}`, "info");
        }
      }
    }
  });
}

// src/inspection/uhp-inspector.ts
var INSPECT_UHP_METHODS = {
  agentList: "agent.list",
  agentGet: "agent.get",
  agentExplain: "agent.explain",
  taskList: "task.list",
  taskGet: "task.get",
  leaseList: "lease.list",
  missionSnapshot: "mission.snapshot",
  workspaceList: "workspace.list"
};

class UhpInspectError extends Error {
  method;
  constructor(method, detail) {
    super(detail !== undefined ? `uhp inspect ${method} failed: ${detail}` : `uhp inspect ${method} failed`);
    this.name = "UhpInspectError";
    this.method = method;
  }
}
class UhpInspector {
  requester;
  endpoint;
  supported;
  constructor(options) {
    if (options === null || typeof options !== "object" || typeof options.endpoint !== "string" || options.endpoint.length === 0) {
      throw new TypeError("UhpInspector requires a non-empty endpoint");
    }
    if (options.requester === null || options.requester === undefined || typeof options.requester.request !== "function") {
      throw new TypeError("UhpInspector requires a requester");
    }
    this.requester = options.requester;
    this.endpoint = options.endpoint;
    if (options.supportedMethods === undefined) {
      this.supported = undefined;
    } else if (typeof options.supportedMethods === "function") {
      this.supported = options.supportedMethods;
    } else {
      this.supported = new Set(options.supportedMethods);
    }
  }
  supports(method) {
    if (this.supported === undefined)
      return true;
    if (typeof this.supported === "function") {
      const live = this.supported();
      if (live === undefined)
        return true;
      return live.includes(method);
    }
    return this.supported.has(method);
  }
  checkSupported(method) {
    if (!this.supports(method)) {
      throw new UhpInspectError(method, "method not announced by server");
    }
  }
  async read(method, params, signal) {
    this.checkSupported(method);
    try {
      return await this.requester.request(this.endpoint, method, params, {
        ...signal === undefined ? {} : { signal }
      });
    } catch (error) {
      if (error instanceof UhpInspectError)
        throw error;
      throw new UhpInspectError(method, "request failed");
    }
  }
  async agentList(signal) {
    return this.read(INSPECT_UHP_METHODS.agentList, {}, signal);
  }
  async agentGet(paneId, signal) {
    if (paneId.length === 0)
      throw new UhpInspectError("agent.get", "empty pane id");
    return this.read(INSPECT_UHP_METHODS.agentGet, { pane: paneId }, signal);
  }
  async agentExplain(paneId, signal) {
    if (paneId.length === 0) {
      throw new UhpInspectError("agent.explain", "empty pane id");
    }
    return this.read(INSPECT_UHP_METHODS.agentExplain, { pane: paneId }, signal);
  }
  async taskList(signal) {
    return this.read(INSPECT_UHP_METHODS.taskList, {}, signal);
  }
  async taskGet(taskId, signal) {
    if (taskId.length === 0)
      throw new UhpInspectError("task.get", "empty task id");
    return this.read(INSPECT_UHP_METHODS.taskGet, { task: taskId }, signal);
  }
  async leaseList(signal) {
    return this.read(INSPECT_UHP_METHODS.leaseList, {}, signal);
  }
  async missionSnapshot(signal) {
    return this.read(INSPECT_UHP_METHODS.missionSnapshot, {}, signal);
  }
  async workspaceList(signal) {
    return this.read(INSPECT_UHP_METHODS.workspaceList, {}, signal);
  }
}

// src/tools/discover.ts
import { Type as Type3 } from "typebox";

// src/tools/delegate.ts
import {
  DEFAULT_MAX_BYTES,
  DEFAULT_MAX_LINES,
  truncateHead
} from "@earendil-works/pi-agent-core";
import { StringEnum } from "@earendil-works/pi-ai/utils/typebox-helpers";
import { Type } from "typebox";

// src/ui/entries.ts
import { Box, Text } from "@earendil-works/pi-tui";
var DELEGATION_ENTRY_TYPE = "luvus-delegation";
var MAX_DELEGATION_CARD_CHARS = 200;
var MAX_DELEGATION_PANE_CHARS = 64;
var MAX_DELEGATION_AGENT_CHARS = 64;
var MAX_DELEGATION_STATUS_CHARS = 16;
var MAX_DELEGATION_TASK_CHARS = 256;
var PRINTABLE_ASCII = /^[\x20-\x7e]*$/;
function cleanField(value, maxChars, fallback) {
  if (typeof value !== "string" || value.length === 0)
    return fallback;
  const trimmed = value.trim();
  if (trimmed.length === 0 || !PRINTABLE_ASCII.test(trimmed))
    return fallback;
  const points = [...trimmed];
  if (points.length > maxChars) {
    return `${points.slice(0, maxChars - 1).join("")}\u2026`;
  }
  return trimmed;
}
function truncatePoints(line, maxChars) {
  const points = [...line];
  if (points.length <= maxChars)
    return line;
  return `${points.slice(0, maxChars - 1).join("")}\u2026`;
}
function buildDelegationCard(input) {
  const paneId = cleanField(input.paneId, MAX_DELEGATION_PANE_CHARS, "");
  const agent = cleanField(input.agent, MAX_DELEGATION_AGENT_CHARS, "");
  const status = cleanField(input.status, MAX_DELEGATION_STATUS_CHARS, "");
  if (paneId.length === 0 || agent.length === 0 || status.length === 0) {
    return;
  }
  const timestamp = typeof input.timestamp === "number" && Number.isSafeInteger(input.timestamp) && input.timestamp >= 0 ? input.timestamp : Date.now();
  const taskId = input.taskId === undefined || input.taskId === null ? undefined : cleanField(input.taskId, MAX_DELEGATION_TASK_CHARS, "");
  return {
    paneId,
    agent,
    status,
    ...taskId === undefined || taskId.length === 0 ? {} : { taskId },
    timestamp
  };
}
function formatDelegationCardCollapsed(card) {
  const task = card.taskId === undefined ? "" : ` \xB7 task ${card.taskId}`;
  return truncatePoints(`\u25CF pane ${card.paneId} ${card.agent} ${card.status}${task}`, MAX_DELEGATION_CARD_CHARS);
}
function formatDelegationCardExpanded(card) {
  const lines = [formatDelegationCardCollapsed(card)];
  try {
    lines.push(`delegated ${new Date(card.timestamp).toISOString()}`);
  } catch {
    lines.push("delegated (unknown time)");
  }
  return lines;
}
function readCard(entry) {
  const data = entry.data;
  if (data === null || typeof data !== "object")
    return;
  return buildDelegationCard({
    paneId: data.paneId,
    agent: data.agent,
    status: data.status,
    taskId: data.taskId,
    timestamp: data.timestamp
  });
}
function registerDelegationEntryRenderer(pi) {
  pi.registerEntryRenderer(DELEGATION_ENTRY_TYPE, (entry, options, theme) => {
    const card = readCard(entry);
    if (card === undefined)
      return;
    const box = new Box(1, 1, (text) => theme.bg("customMessageBg", text));
    if (options.expanded) {
      for (const line of formatDelegationCardExpanded(card)) {
        box.addChild(new Text(theme.fg("accent", line), 0, 0));
      }
    } else {
      box.addChild(new Text(theme.fg("accent", formatDelegationCardCollapsed(card)), 0, 0));
    }
    return box;
  });
}

// src/operations/results.ts
var MAX_RESULT_LINES = 200;
var MAX_RESULT_BYTES = 50000;
var MAX_SCROLLBACK_LINES = 20;
var MAX_SCROLLBACK_BYTES = 4000;
var MAX_RESULT_FILES = 50;
function isRecord4(value) {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
function shortText(value, maxChars) {
  if (typeof value !== "string")
    return "";
  const points = Array.from(value);
  return points.length <= maxChars ? value : points.slice(0, maxChars).join("");
}
function boundLines(value, maxLines, maxBytes) {
  const lines = value.split(`
`).slice(0, maxLines).join(`
`);
  const encoded = new TextEncoder().encode(lines);
  if (encoded.length <= maxBytes)
    return lines;
  let end = maxBytes;
  while (end > 0 && (encoded[end - 1] & 192) === 128)
    end -= 1;
  return new TextDecoder().decode(encoded.subarray(0, end));
}
function pickString2(record, keys, maxChars) {
  for (const key of keys) {
    const value = record[key];
    if (typeof value === "string" && value.length > 0) {
      return shortText(value, maxChars);
    }
  }
  return "";
}
function pickFiles(value) {
  const list = Array.isArray(value) ? value : isRecord4(value) && Array.isArray(value.files) ? value.files : undefined;
  if (list === undefined)
    return;
  const files = [];
  for (const entry of list) {
    if (typeof entry === "string" && entry.length > 0) {
      files.push(entry.slice(0, 512));
    } else if (isRecord4(entry) && typeof entry.path === "string") {
      files.push(entry.path.slice(0, 512));
    }
    if (files.length >= MAX_RESULT_FILES)
      break;
  }
  return files.length > 0 ? files : undefined;
}
function pickGate(value) {
  if (!isRecord4(value))
    return;
  const gate = isRecord4(value.gate) ? value.gate : value;
  if (typeof gate.command !== "string" || typeof gate.passed !== "boolean") {
    return;
  }
  const out = {
    command: shortText(gate.command, 512),
    passed: gate.passed
  };
  if (typeof gate.output === "string" && gate.output.length > 0) {
    return {
      ...out,
      output: boundLines(gate.output, MAX_SCROLLBACK_LINES, MAX_SCROLLBACK_BYTES)
    };
  }
  return out;
}
function assembleAgentResult(agentInfo, agentOutput, taskInfo, paneId) {
  const info = isRecord4(agentInfo) ? agentInfo : {};
  const task = isRecord4(taskInfo) ? taskInfo : {};
  const taskNested = isRecord4(task.task) && typeof task.task === "object" ? task.task : task;
  const agent = pickString2(info, ["agent", "kind"], 128);
  const name = pickString2(info, ["name"], 128);
  const status = pickString2(info, ["status", "agent_status", "state"], 64);
  const taskId = pickString2(taskNested, ["id", "task_id", "taskId"], 256) || undefined;
  const worktree = pickString2(taskNested, ["worktree", "path"], 4096) || undefined;
  const branch = pickString2(taskNested, ["branch"], 256) || undefined;
  const files = pickFiles(info.files) ?? pickFiles(taskNested.outputs) ?? undefined;
  const diffRaw = pickString2(info, ["diff"], MAX_RESULT_BYTES * 2);
  const diff = diffRaw.length > 0 ? boundLines(diffRaw, MAX_RESULT_LINES, MAX_RESULT_BYTES) : undefined;
  const gate = pickGate(info.gate ?? taskNested.gate) ?? pickGate(taskNested);
  const recentRaw = typeof agentOutput === "string" ? agentOutput : "";
  const recentOutput = recentRaw.length > 0 ? boundLines(recentRaw, MAX_SCROLLBACK_LINES, MAX_SCROLLBACK_BYTES) : undefined;
  const result = {
    paneId,
    agent,
    status
  };
  const withOptionals = {
    ...result,
    ...name.length > 0 ? { name } : {},
    ...taskId !== undefined ? { taskId } : {},
    ...worktree !== undefined ? { worktree } : {},
    ...branch !== undefined ? { branch } : {},
    ...files !== undefined ? { files } : {},
    ...diff !== undefined ? { diff } : {},
    ...gate !== undefined ? { gate } : {},
    ...recentOutput !== undefined ? { recentOutput } : {}
  };
  if (files === undefined && diff === undefined && recentOutput !== undefined) {
    return {
      ...withOptionals,
      inspectHint: `Read more with luvus_inspect agent/get ${paneId} or agent read ${paneId} --lines 200.`
    };
  }
  return withOptionals;
}

// src/operations/wait.ts
var MIN_POLL_MS = 500;
var MAX_POLL_MS = 30000;
var DEFAULT_POLL_MS = 2000;
var MAX_OUTPUT_SNAPSHOT_CHARS = 2048;
function clampPollMs(value) {
  if (value === undefined)
    return DEFAULT_POLL_MS;
  if (!Number.isSafeInteger(value))
    return DEFAULT_POLL_MS;
  return Math.min(MAX_POLL_MS, Math.max(MIN_POLL_MS, value));
}
function defaultSleep(ms, signal) {
  if (signal?.aborted === true)
    return Promise.resolve();
  return new Promise((resolve) => {
    if (signal === undefined) {
      setTimeout(resolve, ms);
      return;
    }
    const timer = setTimeout(cleanup, ms);
    function cleanup() {
      clearTimeout(timer);
      signal?.removeEventListener("abort", cleanup);
      resolve();
    }
    signal.addEventListener("abort", cleanup, { once: true });
  });
}
function abortError2(signal) {
  const reason = signal.reason;
  if (reason instanceof Error)
    return reason;
  return new Error("semantic wait aborted before start");
}
async function semanticWait(options, params, signal) {
  if (signal !== undefined && signal.aborted)
    throw abortError2(signal);
  if (!Number.isSafeInteger(options.timeoutMs) || options.timeoutMs <= 0) {
    throw new RangeError("semanticWait timeoutMs must be a safe integer > 0");
  }
  if (params.until === undefined === (params.matchText === undefined)) {
    throw new Error("semanticWait requires exactly one of until/matchText");
  }
  const pollMs = clampPollMs(options.pollMs);
  const now = options.now ?? Date.now;
  const sleep = options.sleep ?? defaultSleep;
  const deadline = now() + options.timeoutMs;
  const chunkS = Math.max(1, Math.min(3600, Math.floor(pollMs / 1000)));
  let lastStatus = "";
  for (;; ) {
    if (signal !== undefined && signal.aborted) {
      return {
        paneId: params.paneId,
        status: lastStatus,
        cancelled: true,
        timedOut: false
      };
    }
    const remaining = deadline - now();
    if (remaining <= 0) {
      const output = params.until !== undefined ? undefined : await readSnapshot(options.inspector, params.paneId, signal);
      return {
        paneId: params.paneId,
        status: lastStatus,
        ...output === undefined ? {} : { output },
        timedOut: true,
        cancelled: false
      };
    }
    if (params.until !== undefined) {
      const until = params.until;
      const iterationStart = now();
      let wait;
      try {
        wait = await options.inspector.agentWait({
          paneId: params.paneId,
          until,
          timeoutS: Math.max(1, Math.min(chunkS, Math.ceil(remaining / 1000)))
        }, signal);
      } catch (error) {
        if (signal !== undefined && signal.aborted) {
          return {
            paneId: params.paneId,
            status: lastStatus,
            cancelled: true,
            timedOut: false
          };
        }
        throw error;
      }
      if (typeof wait.status === "string" && wait.status.length > 0) {
        lastStatus = wait.status;
      }
      if (wait.matched) {
        return {
          paneId: params.paneId,
          status: lastStatus,
          timedOut: false,
          cancelled: false
        };
      }
      const elapsed = now() - iterationStart;
      if (elapsed < pollMs) {
        await sleep(Math.min(pollMs - elapsed, Math.max(0, deadline - now())), signal);
      }
      continue;
    }
    const output = await readSnapshot(options.inspector, params.paneId, signal);
    if (params.matchText !== undefined && output !== undefined && output.includes(params.matchText)) {
      return {
        paneId: params.paneId,
        status: lastStatus,
        output,
        timedOut: false,
        cancelled: false
      };
    }
    await sleep(Math.min(pollMs, Math.max(0, remaining)), signal);
  }
}
async function readSnapshot(inspector, paneId, signal) {
  try {
    const output = await inspector.agentRead({ target: paneId, lines: 20 }, signal);
    if (typeof output !== "string" || output.length === 0)
      return;
    return output.length <= MAX_OUTPUT_SNAPSHOT_CHARS ? output : output.slice(-MAX_OUTPUT_SNAPSHOT_CHARS);
  } catch {
    return;
  }
}

// src/tools/delegate.ts
var DELEGATE_TOOL_NAME = "luvus_delegate";
var AGENT_KINDS = ["pi", "claude", "codex", "opencode"];
var MAX_NAME_CHARS = 128;
var MAX_TIMEOUT_SECONDS = 600;
var DEFAULT_TIMEOUT_SECONDS = 600;
var DEFAULT_MAX_WAIT_MS = 600000;
var DEFAULT_AGENT_NAME = "luvus-delegate";
var DelegateParams = Type.Object({
  target: Type.Optional(Type.String({
    description: "Existing agent pane id or name. Omit to start a new agent."
  })),
  prompt: Type.String({
    description: "Complete task description for the delegate agent."
  }),
  kind: Type.Optional(StringEnum(AGENT_KINDS, {
    description: "Agent kind for new agents (default: pi)."
  })),
  name: Type.Optional(Type.String({
    description: "Display name for a new agent (max 128 chars)."
  })),
  anchor: Type.Optional(Type.String({
    description: "Pane id to split beside for a new agent."
  })),
  down: Type.Optional(Type.Boolean({
    description: "Split below the anchor instead of beside it."
  })),
  wait: Type.Optional(Type.Boolean({
    description: "Wait locally until the delegate reaches done (default: false)."
  })),
  timeoutSeconds: Type.Optional(Type.Integer({
    minimum: 1,
    maximum: MAX_TIMEOUT_SECONDS,
    description: "Max local wait in seconds (default: 600)."
  })),
  mutation: Type.Optional(Type.Boolean({
    description: "Allow the delegate to write code via an ORCH task (default: false, read-only)."
  })),
  paths: Type.Optional(Type.Array(Type.String(), {
    description: "Glob patterns the delegate may modify (required when mutation is true)."
  })),
  gate: Type.Optional(Type.String({
    description: "Quality gate command recorded on the ORCH task."
  })),
  worktree: Type.Optional(Type.Boolean({
    description: "Use a Git worktree for the ORCH task (default: true for mutations)."
  }))
});
function fail3(hint) {
  throw new Error(`luvus_delegate: ${hint}`);
}
function truncate(text) {
  const result = truncateHead(text, {
    maxLines: DEFAULT_MAX_LINES,
    maxBytes: DEFAULT_MAX_BYTES
  });
  if (!result.truncated)
    return result.content;
  return `${result.content}

[Output truncated: showing ${result.outputLines} of ` + `${result.totalLines} lines.]`;
}
function promptTitle(prompt) {
  const first = prompt.split(`
`, 1)[0] ?? "";
  const flat = first.replace(/\s+/g, " ").trim().slice(0, 120);
  return flat.length > 0 ? flat : "delegated task";
}
function validateParams(params) {
  const promptCheck = validatePrompt(params.prompt);
  if (!promptCheck.valid)
    fail3(`invalid prompt: ${promptCheck.reason}`);
  const hasTarget = params.target !== undefined && params.target.length > 0;
  if (hasTarget) {
    const setters = [
      ["kind", params.kind],
      ["name", params.name],
      ["anchor", params.anchor],
      ["down", params.down],
      ["mutation", params.mutation],
      ["paths", params.paths],
      ["gate", params.gate],
      ["worktree", params.worktree]
    ];
    for (const [key, value] of setters) {
      if (value !== undefined) {
        fail3(`"${key}" applies only to new agents (omit target or omit "${key}")`);
      }
    }
  }
  const mutation = params.mutation === true;
  const paths = [];
  if (params.paths !== undefined) {
    for (const entry of params.paths) {
      const checked = validateLeasePath(entry);
      if (!checked.valid)
        fail3(`invalid paths entry: ${checked.reason}`);
      paths.push(entry);
    }
  }
  if (mutation && paths.length === 0) {
    fail3("mutation requires at least one path pattern in paths");
  }
  if (!mutation) {
    if (params.gate !== undefined)
      fail3('"gate" requires mutation: true');
    if (params.worktree !== undefined) {
      fail3('"worktree" requires mutation: true');
    }
  }
  let kind = "pi";
  if (params.kind !== undefined) {
    const checked = validateAgentKind(params.kind);
    if (!checked.valid)
      fail3(`invalid kind: ${checked.reason}`);
    kind = params.kind;
  }
  let name;
  if (params.name !== undefined && params.name.length > 0) {
    if (params.name.length > MAX_NAME_CHARS) {
      fail3("name exceeds 128 chars");
    }
    name = params.name;
  }
  let anchor;
  if (params.anchor !== undefined && params.anchor.length > 0) {
    const checked = validatePaneId(params.anchor);
    if (!checked.valid)
      fail3(`invalid anchor: ${checked.reason}`);
    anchor = params.anchor;
  }
  let gate;
  if (params.gate !== undefined) {
    if (params.gate.length === 0 || params.gate.length > 1024) {
      fail3("gate must be 1..1024 chars");
    }
    gate = params.gate;
  }
  const timeoutSeconds = params.timeoutSeconds ?? DEFAULT_TIMEOUT_SECONDS;
  if (!Number.isSafeInteger(timeoutSeconds) || timeoutSeconds < 1 || timeoutSeconds > MAX_TIMEOUT_SECONDS) {
    fail3("timeoutSeconds must be an integer within [1, 600]");
  }
  return {
    target: hasTarget ? params.target : undefined,
    prompt: params.prompt,
    kind,
    name,
    anchor,
    down: params.down === true,
    wait: params.wait === true,
    timeoutSeconds,
    mutation,
    paths,
    gate,
    worktree: params.worktree !== false
  };
}
function registerDelegateTool(pi, deps) {
  pi.registerTool({
    name: DELEGATE_TOOL_NAME,
    label: "Luvus Delegate",
    description: "Delegate work to a new or existing Luvus agent, with optional ORCH task, leases, and quality gate (never auto-merges)",
    promptSnippet: "Delegate work to a Luvus agent, optionally with an ORCH task and quality gate",
    promptGuidelines: [
      "Use luvus_delegate when a task should run in a separate Luvus agent instead of the current session.",
      "Use luvus_delegate with mutation for code changes so paths are leased and a worktree is used; luvus_delegate never merges."
    ],
    parameters: DelegateParams,
    async execute(toolCallId, params, signal, _onUpdate, ctx) {
      const run = async () => {
        const ops = deps.ops;
        if (ops === undefined) {
          fail3("operations unavailable on this bridge (CLI-only stack); inspect with luvus_inspect instead");
        }
        if (signal?.aborted === true)
          fail3("aborted");
        const valid = validateParams(params);
        const call = (summary, detail) => ({
          ctx,
          summary,
          ...detail === undefined ? {} : { detail }
        });
        const sig = signal ?? undefined;
        let paneId;
        let taskId;
        let worktreePath;
        let branch;
        if (valid.target !== undefined) {
          paneId = valid.target;
          await ops.agentPrompt({ target: paneId, text: valid.prompt }, call(`Prompt agent in pane ${paneId}`), sig);
        } else if (!valid.mutation) {
          const started = await ops.agentStart({
            name: valid.name ?? DEFAULT_AGENT_NAME,
            kind: valid.kind,
            ...valid.anchor === undefined ? {} : { anchor: valid.anchor },
            ...valid.down ? { down: true } : {}
          }, call(`Start ${valid.kind} agent "${valid.name ?? DEFAULT_AGENT_NAME}"`), sig);
          paneId = started.paneId;
          await ops.agentPrompt({ target: paneId, text: valid.prompt }, call(`Prompt agent in pane ${paneId}`), sig);
        } else {
          const title = promptTitle(valid.prompt);
          const added = await ops.taskAdd({
            title,
            paths: valid.paths,
            ...valid.gate === undefined ? {} : { gate: valid.gate }
          }, call(`Create ORCH task "${title}"`, valid.paths.join(", ")), sig);
          taskId = added.taskId;
          await ops.taskClaim(taskId, call(`Claim ORCH task ${taskId}`), sig);
          await ops.leaseAcquire({ paths: valid.paths, taskId }, call(`Reserve ${valid.paths.length} path(s) for task ${taskId}`, valid.paths.join(", ")), sig);
          const mode = valid.worktree ? "worktree" : "workspace";
          const startedTask = await ops.taskStart({ taskId, mode, agent: valid.kind }, call(`Start worker for task ${taskId} in ${mode} mode`), sig);
          branch = startedTask.branch;
          worktreePath = startedTask.worktree;
          if (startedTask.paneId !== undefined && startedTask.paneId.length > 0) {
            paneId = startedTask.paneId;
          } else {
            const started = await ops.agentStart({
              name: valid.name ?? DEFAULT_AGENT_NAME,
              kind: valid.kind,
              ...valid.anchor === undefined ? {} : { anchor: valid.anchor },
              ...valid.down ? { down: true } : {}
            }, call(`Start ${valid.kind} agent "${valid.name ?? DEFAULT_AGENT_NAME}" (task ${taskId} provisioned no pane)`), sig);
            paneId = started.paneId;
          }
          await ops.agentPrompt({ target: paneId, text: valid.prompt }, call(`Prompt agent in pane ${paneId}`), sig);
        }
        let timedOut = false;
        let cancelled = false;
        if (valid.wait) {
          const ceiling = deps.maxWaitMs ?? DEFAULT_MAX_WAIT_MS;
          const timeoutMs = Math.min(valid.timeoutSeconds * 1000, ceiling);
          try {
            const waited = await semanticWait({ inspector: ops, timeoutMs }, { paneId, until: "done" }, sig);
            timedOut = waited.timedOut;
            cancelled = waited.cancelled;
          } catch (error) {
            if (sig?.aborted === true) {
              cancelled = true;
            } else {
              throw error;
            }
          }
        }
        let info = {};
        try {
          info = await ops.agentGet(paneId, sig);
        } catch {
          info = {};
        }
        let output;
        try {
          output = await ops.agentRead({ target: paneId, lines: 20 }, sig);
        } catch {
          output = undefined;
        }
        let taskInfo = undefined;
        if (taskId !== undefined) {
          try {
            taskInfo = await ops.taskGet(taskId, sig);
          } catch {
            taskInfo = undefined;
          }
        }
        const result = assembleAgentResult(info, output, taskInfo, paneId);
        const lines = [];
        const displayName = result.name ?? (valid.target === undefined ? valid.name ?? DEFAULT_AGENT_NAME : undefined);
        lines.push(`Delegated to pane ${paneId} (${result.agent || valid.kind}` + `${displayName !== undefined ? `, name "${displayName}"` : ""}): ` + `status ${result.status || "unknown"}`);
        const effectiveTaskId = taskId ?? result.taskId;
        const effectiveBranch = branch ?? result.branch;
        const effectiveWorktree = worktreePath ?? result.worktree;
        if (effectiveTaskId !== undefined) {
          lines.push(`Task: ${effectiveTaskId}` + `${effectiveBranch !== undefined ? `, branch ${effectiveBranch}` : ""}` + `${effectiveWorktree !== undefined ? `, worktree ${effectiveWorktree}` : ""}`);
        }
        if (result.files !== undefined) {
          lines.push(`Files: ${result.files.join(", ")}`);
        }
        if (result.diff !== undefined) {
          lines.push(`Diff:
${result.diff}`);
        }
        if (result.gate !== undefined) {
          lines.push(`Gate ${result.gate.command}: ${result.gate.passed ? "passed" : "failed"}` + `${result.gate.output !== undefined ? `
${result.gate.output}` : ""}`);
        }
        if (timedOut) {
          lines.push(`Wait timed out locally after ${valid.timeoutSeconds}s; the remote agent keeps running.`);
        }
        if (cancelled) {
          lines.push("Wait cancelled locally; the remote agent keeps running.");
        }
        if (result.recentOutput !== undefined) {
          lines.push(`Recent output:
${result.recentOutput}`);
        }
        if (result.inspectHint !== undefined) {
          lines.push(result.inspectHint);
        }
        const text = truncate(lines.join(`
`));
        const details = {
          paneId,
          agent: result.agent,
          status: result.status
        };
        if (displayName !== undefined)
          details.name = displayName;
        if (effectiveTaskId !== undefined)
          details.taskId = effectiveTaskId;
        if (effectiveBranch !== undefined)
          details.branch = effectiveBranch;
        if (effectiveWorktree !== undefined) {
          details.worktree = effectiveWorktree;
        }
        if (result.files !== undefined)
          details.files = [...result.files];
        if (result.gate !== undefined)
          details.gate = { ...result.gate };
        if (timedOut)
          details.timedOut = true;
        if (cancelled)
          details.cancelled = true;
        return { text, details };
      };
      try {
        const { text, details } = await run();
        try {
          const card = buildDelegationCard({
            paneId: details.paneId,
            agent: details.agent,
            status: details.status,
            taskId: details.taskId
          });
          if (card !== undefined && typeof pi.appendEntry === "function") {
            pi.appendEntry(DELEGATION_ENTRY_TYPE, card);
          }
        } catch {}
        return {
          content: [{ type: "text", text }],
          details
        };
      } catch (error) {
        if (error instanceof Error)
          throw new Error(error.message);
        throw new Error("luvus_delegate failed");
      }
    }
  });
}

// src/tools/registry.ts
function additiveSetActiveTools(pi, namesToAdd) {
  const current = pi.getActiveTools();
  const present = new Set(current);
  const fresh = [];
  for (const name of namesToAdd) {
    if (present.has(name))
      continue;
    present.add(name);
    fresh.push(name);
  }
  if (fresh.length === 0)
    return [];
  pi.setActiveTools([...current, ...fresh]);
  return fresh;
}

// src/tools/task.ts
import {
  DEFAULT_MAX_BYTES as DEFAULT_MAX_BYTES2,
  DEFAULT_MAX_LINES as DEFAULT_MAX_LINES2,
  truncateHead as truncateHead2
} from "@earendil-works/pi-agent-core";
import { StringEnum as StringEnum2 } from "@earendil-works/pi-ai/utils/typebox-helpers";
import { Type as Type2 } from "typebox";
var TASK_TOOL_NAME = "luvus_task";
var TASK_ACTIONS = [
  "list",
  "get",
  "leases",
  "gate_status",
  "next",
  "add",
  "claim",
  "start",
  "update",
  "done",
  "release",
  "merge",
  "delete"
];
var TaskParams = Type2.Object({
  action: StringEnum2(TASK_ACTIONS, {
    description: "Task operation. Reads: list, get, leases, gate_status. Claim-and-dispatch: next. Writes: add, claim, start, update, done, release. High-impact: merge, delete."
  }),
  taskId: Type2.Optional(Type2.String({
    description: "Task id (required for get, gate_status, claim, start, update, done, release, merge, delete)."
  })),
  title: Type2.Optional(Type2.String({ description: "Task title (required for add)." })),
  paths: Type2.Optional(Type2.Array(Type2.String(), {
    description: "Glob patterns for task-scoped leases (for add)."
  })),
  dependsOn: Type2.Optional(Type2.Array(Type2.String(), {
    description: "Task ids this task depends on (for add)."
  })),
  gate: Type2.Optional(Type2.String({ description: "Quality gate command (for add)." })),
  mode: Type2.Optional(StringEnum2(["worktree", "workspace"], {
    description: "Execution mode (for start/next; default: worktree)."
  })),
  agent: Type2.Optional(Type2.String({
    description: "Agent command for the worker (for start/next)."
  })),
  branch: Type2.Optional(Type2.String({ description: "Branch name for the worker (for start)." })),
  status: Type2.Optional(Type2.String({ description: "New status (for update)." })),
  output: Type2.Optional(Type2.String({
    description: "Progress output, max 4096 chars (for update)."
  })),
  note: Type2.Optional(Type2.String({ description: "Status note (for update)." })),
  startWorker: Type2.Optional(Type2.Boolean({
    description: "Claim and start a worker in one step (for next; default: false)."
  }))
});
function fail4(hint) {
  throw new Error(`luvus_task: ${hint}`);
}
function truncate2(text) {
  const result = truncateHead2(text, {
    maxLines: DEFAULT_MAX_LINES2,
    maxBytes: DEFAULT_MAX_BYTES2
  });
  if (!result.truncated)
    return result.content;
  return `${result.content}

[Output truncated: showing ${result.outputLines} of ` + `${result.totalLines} lines.]`;
}
function formatPayload(label, payload) {
  return truncate2(`${label}:
${JSON.stringify(payload, null, 2)}`);
}
function requireTaskId(taskId, action) {
  if (taskId === undefined || taskId.length === 0) {
    fail4(`action ${action} requires a non-empty taskId`);
  }
  const checked = validateTaskId(taskId);
  if (!checked.valid)
    fail4(`invalid taskId: ${checked.reason}`);
  return taskId;
}
function shortText2(value, max = 120) {
  if (typeof value !== "string")
    return "";
  const flat = value.replace(/\s+/g, " ").trim();
  return flat.length <= max ? flat : `${flat.slice(0, max - 3)}...`;
}
function pickString3(record, keys) {
  for (const key of keys) {
    const value = record[key];
    if (typeof value === "string" && value.length > 0)
      return value;
  }
  return "";
}
function isRecord5(value) {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
function registerTaskTool(pi, deps) {
  pi.registerTool({
    name: TASK_TOOL_NAME,
    label: "Luvus Task",
    description: "Manage Luvus ORCH tasks: add, claim, start, update, done, release, next, gate status (merge/delete require reinforced confirmation)",
    promptSnippet: "Manage Luvus ORCH task lifecycle: create, claim, start, update, finish, or release tasks",
    promptGuidelines: [
      "Use luvus_task for ORCH task writes (add, claim, start, update, done, release); use luvus_inspect for read-only task queries.",
      "Use luvus_task merge only when explicitly asked; luvus_task never merges automatically."
    ],
    parameters: TaskParams,
    async execute(toolCallId, params, signal, _onUpdate, ctx) {
      const run = async () => {
        const ops = deps.ops;
        if (ops === undefined) {
          fail4("operations unavailable on this bridge (CLI-only stack); inspect with luvus_inspect instead");
        }
        if (signal?.aborted === true)
          fail4("aborted");
        const router = ops;
        const sig = signal ?? undefined;
        const call = (summary, detail) => ({
          ctx,
          summary,
          ...detail === undefined ? {} : { detail }
        });
        const action = params.action;
        if (!TASK_ACTIONS.includes(action)) {
          fail4(`unknown action: ${shortText2(action, 64)}`);
        }
        switch (action) {
          case "list": {
            const payload = await router.taskList(sig);
            return {
              text: formatPayload("Tasks", payload),
              details: { action }
            };
          }
          case "get": {
            const id = requireTaskId(params.taskId, action);
            const payload = await router.taskGet(id, sig);
            return {
              text: formatPayload(`Task ${id}`, payload),
              details: { action, taskId: id }
            };
          }
          case "leases": {
            const payload = await router.leaseList(sig);
            return {
              text: formatPayload("Leases", payload),
              details: { action }
            };
          }
          case "gate_status": {
            const id = requireTaskId(params.taskId, action);
            const task = await router.taskGet(id, sig);
            const leases = await router.leaseList(sig);
            return {
              text: truncate2(`Gate status for task ${id}:
` + `${JSON.stringify(task, null, 2)}
` + `Active leases:
${JSON.stringify(leases, null, 2)}`),
              details: { action, taskId: id }
            };
          }
          case "next": {
            if (params.startWorker === true) {
              const mode = params.mode === "workspace" ? "workspace" : "worktree";
              const next = await router.taskNext({
                start: true,
                ...params.agent === undefined ? {} : { agent: params.agent },
                mode
              }, call("Claim and start the next ready ORCH task"), sig);
              if (next.taskId.length === 0) {
                return {
                  text: "No ready tasks in the queue.",
                  details: { action, started: false }
                };
              }
              return {
                text: `Next task ${next.taskId} "${shortText2(next.title)}" ` + `(worker started in ${mode} mode).`,
                details: {
                  action,
                  taskId: next.taskId,
                  started: true,
                  mode
                }
              };
            }
            const next = await router.taskNext({}, call("Claim the next ready ORCH task"), sig);
            if (next.taskId.length === 0) {
              return {
                text: "No ready tasks in the queue.",
                details: { action, started: false }
              };
            }
            return {
              text: `Next ready task ${next.taskId} "${shortText2(next.title)}" (claimed).`,
              details: { action, taskId: next.taskId, started: false }
            };
          }
          case "add": {
            if (params.title === undefined || params.title.length === 0) {
              fail4("action add requires a non-empty title");
            }
            if (params.title.length > 512)
              fail4("title exceeds 512 chars");
            const paths = params.paths === undefined ? undefined : params.paths.map((entry) => {
              const checked = validateLeasePath(entry);
              if (!checked.valid) {
                fail4(`invalid paths entry: ${checked.reason}`);
              }
              return entry;
            });
            const dependsOn = params.dependsOn === undefined ? undefined : params.dependsOn.map((entry) => {
              const checked = validateTaskId(entry);
              if (!checked.valid) {
                fail4(`invalid dependsOn entry: ${checked.reason}`);
              }
              return entry;
            });
            if (params.gate !== undefined && params.gate.length > 1024) {
              fail4("gate exceeds 1024 chars");
            }
            const added = await router.taskAdd({
              title: params.title,
              ...paths === undefined ? {} : { paths },
              ...dependsOn === undefined ? {} : { dependsOn },
              ...params.gate === undefined ? {} : { gate: params.gate }
            }, call(`Create ORCH task "${shortText2(params.title)}"`, paths === undefined ? undefined : paths.join(", ")), sig);
            return {
              text: `Created task ${added.taskId}: ${shortText2(added.title)}.`,
              details: { action, taskId: added.taskId }
            };
          }
          case "claim": {
            const id = requireTaskId(params.taskId, action);
            await router.taskClaim(id, call(`Claim ORCH task ${id}`), sig);
            return {
              text: `Claimed task ${id}.`,
              details: { action, taskId: id }
            };
          }
          case "start": {
            const id = requireTaskId(params.taskId, action);
            const mode = params.mode === "workspace" ? "workspace" : "worktree";
            const started = await router.taskStart({
              taskId: id,
              ...params.branch === undefined ? {} : { branch: params.branch },
              ...params.agent === undefined ? {} : { agent: params.agent },
              mode
            }, call(`Start worker for task ${id} in ${mode} mode`), sig);
            const task = isRecord5(started) ? started : {};
            return {
              text: `Started task ${id} in ${mode} mode` + `${pickString3(task, ["branch"]).length > 0 ? `, branch ${pickString3(task, ["branch"])}` : ""}` + `${pickString3(task, ["worktree", "path"]).length > 0 ? `, worktree ${pickString3(task, ["worktree", "path"])}` : ""}` + `${pickString3(task, ["pane_id", "pane", "paneId"]).length > 0 ? `, pane ${pickString3(task, ["pane_id", "pane", "paneId"])}` : ""}.`,
              details: { action, taskId: id, mode }
            };
          }
          case "update": {
            const id = requireTaskId(params.taskId, action);
            if (params.status === undefined && params.output === undefined && params.note === undefined) {
              fail4("action update requires at least one of status, output, note");
            }
            if (params.output !== undefined && params.output.length > 4096) {
              fail4("output exceeds 4096 chars");
            }
            await router.taskUpdate({
              taskId: id,
              ...params.status === undefined ? {} : { status: params.status },
              ...params.output === undefined ? {} : { output: params.output },
              ...params.note === undefined ? {} : { note: params.note }
            }, call(`Update ORCH task ${id}`), sig);
            return {
              text: `Updated task ${id}.`,
              details: { action, taskId: id }
            };
          }
          case "done": {
            const id = requireTaskId(params.taskId, action);
            await router.taskDone(id, call(`Mark ORCH task ${id} done (releases its leases)`), sig);
            return {
              text: `Task ${id} marked done (leases released).`,
              details: { action, taskId: id }
            };
          }
          case "release": {
            const id = requireTaskId(params.taskId, action);
            await router.taskRelease(id, call(`Return ORCH task ${id} to the queue`), sig);
            return {
              text: `Released task ${id} back to the queue.`,
              details: { action, taskId: id }
            };
          }
          case "merge": {
            const id = requireTaskId(params.taskId, action);
            await router.taskMerge(id, call(`Merge task ${id} into luvus/integration (high-impact)`), sig);
            return {
              text: `Merged task ${id} into luvus/integration.`,
              details: { action, taskId: id }
            };
          }
          case "delete": {
            const id = requireTaskId(params.taskId, action);
            await router.taskDelete(id, call(`Delete ORCH task ${id} (high-impact)`), sig);
            return {
              text: `Deleted task ${id}.`,
              details: { action, taskId: id }
            };
          }
        }
      };
      try {
        const { text, details } = await run();
        return {
          content: [{ type: "text", text }],
          details
        };
      } catch (error) {
        if (error instanceof Error)
          throw new Error(error.message);
        throw new Error("luvus_task failed");
      }
    }
  });
}

// src/tools/discover.ts
var DISCOVER_TOOL_NAME = "luvus_capabilities";
var INSPECT_TOOL_NAME = "luvus_inspect";
var DiscoverParams = Type3.Object({
  query: Type3.String({
    description: "What capability are you looking for? e.g. 'inspect agents', 'list tasks'"
  })
});
var CAPABILITY_CATALOG = [
  {
    keywords: [
      "agent",
      "inspect",
      "fleet",
      "list agents",
      "status",
      "pane",
      "task",
      "lease",
      "mission",
      "workspace"
    ],
    tool: INSPECT_TOOL_NAME,
    description: "Inspect agents, tasks, leases, workspaces, and fleet status"
  },
  {
    keywords: [
      "delegate",
      "delegation",
      "start agent",
      "prompt agent",
      "spawn agent",
      "dispatch",
      "fork agent",
      "subagent",
      "sub-agent"
    ],
    tool: DELEGATE_TOOL_NAME,
    description: "Delegate work to a new or existing Luvus agent, with optional ORCH task, leases, and quality gate"
  },
  {
    keywords: [
      "orch",
      "worktree",
      "quality gate",
      "claim task",
      "create task",
      "task done",
      "task lifecycle",
      "dependencies",
      "task merge",
      "startworker",
      "start worker"
    ],
    tool: TASK_TOOL_NAME,
    description: "Manage ORCH tasks: add, claim, start, update, done, release, merge, delete, next"
  }
];
function matchCatalog(query) {
  const lowered = query.toLowerCase();
  return CAPABILITY_CATALOG.filter((entry) => entry.keywords.some((keyword) => lowered.includes(keyword)));
}
function bound(text, max = 500) {
  return text.length <= max ? text : `${text.slice(0, max - 3)}...`;
}
function registerDiscoverTool(pi, deps) {
  pi.registerTool({
    name: DISCOVER_TOOL_NAME,
    label: "Luvus Capabilities",
    description: "Discover and enable Luvus coordination tools for the current task",
    promptSnippet: "Search for additional Luvus tools when the active tools cannot perform the task",
    promptGuidelines: [
      "Use luvus_capabilities when a task requires inspecting agents, tasks, leases, or fleet status."
    ],
    parameters: DiscoverParams,
    async execute(_toolCallId, params) {
      const run = async () => {
        const query = params.query ?? "";
        const matches = matchCatalog(query);
        if (matches.length === 0) {
          const health = deps.router.healthSnapshot();
          if (!health.hasCapabilities) {
            return bound("Luvus bridge is disconnected. Only basic telemetry is active.");
          }
          return bound(`No Luvus tools match: ${query.slice(0, 80)}. Available: inspect agents/tasks/leases/fleet, delegate to agents, manage ORCH tasks.`);
        }
        const lines = [];
        for (const entry of matches) {
          const active = pi.getActiveTools();
          if (active.includes(entry.tool)) {
            lines.push(`${entry.tool} is already active: ${entry.description}.`);
            continue;
          }
          additiveSetActiveTools(pi, [entry.tool]);
          lines.push(`Activated ${entry.tool}: ${entry.description}.`);
        }
        return bound(lines.join(" "));
      };
      const text = await run();
      return {
        content: [{ type: "text", text }],
        details: {}
      };
    }
  });
}

// src/tools/commands.ts
var POLICY_MODES = ["auto", "confirm", "restricted"];
function bound2(text, max = 2000) {
  return text.length <= max ? text : `${text.slice(0, max - 3)}...`;
}
function isRecord6(value) {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
function shortText3(value, max = 120) {
  if (typeof value !== "string")
    return "";
  const flat = value.replace(/\s+/g, " ").trim();
  return flat.length <= max ? flat : `${flat.slice(0, max - 3)}...`;
}
function registerPhase4Commands(pi, deps) {
  pi.registerCommand("luvus-policy", {
    description: "Show or change the Luvus mutation policy mode",
    handler: async (_args, ctx) => {
      if (!ctx.hasUI) {
        ctx.ui.notify("luvus-policy requires UI", "error");
        return;
      }
      const current = deps.policy.mode();
      const picked = await ctx.ui.select(`Luvus policy: ${current}`, [
        ...POLICY_MODES
      ]);
      if (picked === undefined)
        return;
      if (!POLICY_MODES.includes(picked))
        return;
      deps.policy.setMode(picked, "user");
      ctx.ui.notify(`Luvus policy mode set to ${picked}`, "info");
    }
  });
  pi.registerCommand("luvus-task", {
    description: "Show the active ORCH task or queued tasks",
    handler: async (_args, ctx) => {
      const ops = deps.ops;
      if (ops === undefined) {
        ctx.ui.notify("Luvus operations unavailable (CLI-only bridge)", "warning");
        return;
      }
      const current = deps.config.taskId;
      try {
        if (current !== undefined) {
          const payload = await ops.taskGet(current);
          const task = isRecord6(payload) ? payload : {};
          const nested = isRecord6(task.task) ? task.task : task;
          const title = shortText3(nested.title ?? task.title, 160);
          const status = shortText3(nested.status ?? task.status ?? nested.state ?? task.state, 64);
          ctx.ui.notify(bound2(`ORCH task ${current}${title.length > 0 ? `: ${title}` : ""}` + `${status.length > 0 ? `
Status: ${status}` : ""}`), "info");
          return;
        }
        const payload = await ops.taskList();
        const rows = Array.isArray(payload) ? payload.filter(isRecord6) : isRecord6(payload) && Array.isArray(payload.tasks) ? payload.tasks.filter(isRecord6) : [];
        if (rows.length === 0) {
          ctx.ui.notify("No ORCH tasks in the queue", "info");
          return;
        }
        const lines = rows.slice(0, 20).map((row) => {
          const id = shortText3(row.id ?? row.task_id ?? row.taskId, 64);
          const title = shortText3(row.title, 100);
          const status = shortText3(row.status ?? row.state, 32);
          return `${id} ${status} ${title}`.trim();
        });
        ctx.ui.notify(bound2(`Queued tasks (${rows.length}):
${lines.join(`
`)}`), "info");
      } catch {
        ctx.ui.notify("luvus-task: failed to load tasks", "error");
      }
    }
  });
  pi.registerCommand("luvus-tools", {
    description: "List Luvus tools and their activation status",
    handler: async (_args, ctx) => {
      const active = new Set(pi.getActiveTools());
      const luvusTools = pi.getAllTools().filter((tool) => typeof tool.name === "string" && tool.name.startsWith("luvus_"));
      if (luvusTools.length === 0) {
        ctx.ui.notify("No Luvus tools registered", "info");
        return;
      }
      const lines = luvusTools.map((tool) => `${tool.name} \u2014 ${active.has(tool.name) ? "active" : "inactive"}`);
      ctx.ui.notify(bound2(lines.join(`
`)), "info");
    }
  });
  pi.registerCommand("luvus-reconnect", {
    description: "Force a UHP capability reprobe",
    handler: async (_args, ctx) => {
      const telemetry = deps.telemetry;
      if (telemetry === undefined) {
        ctx.ui.notify("CLI-only bridge: nothing to reconnect", "info");
        return;
      }
      try {
        const caps = await telemetry.forceReconnect();
        if (caps === undefined) {
          ctx.ui.notify("luvus-reconnect: probe failed", "error");
          return;
        }
        ctx.ui.notify(`Reconnected: ${caps.methods.length} methods, gen ${caps.serverGeneration.slice(0, 8)}`, "info");
      } catch {
        ctx.ui.notify("luvus-reconnect failed", "error");
      }
    }
  });
}

// src/tools/inspect.ts
import {
  DEFAULT_MAX_BYTES as DEFAULT_MAX_BYTES3,
  DEFAULT_MAX_LINES as DEFAULT_MAX_LINES3,
  truncateHead as truncateHead3
} from "@earendil-works/pi-agent-core";
import { StringEnum as StringEnum3 } from "@earendil-works/pi-ai/utils/typebox-helpers";
import { Type as Type4 } from "typebox";

// src/inspection/cli-inspector.ts
var MAX_INSPECT_OUTPUT_BYTES = 64 * 1024;

class CliInspectError extends Error {
  action;
  constructor(action, detail) {
    super(detail !== undefined ? `cli inspect ${action} failed: ${detail}` : `cli inspect ${action} failed`);
    this.name = "CliInspectError";
    this.action = action;
  }
}

class CliInspector {
  exec;
  bin;
  timeoutMs;
  constructor(options) {
    if (options === null || typeof options !== "object" || typeof options.exec !== "function") {
      throw new TypeError("CliInspector requires an exec function");
    }
    const explicit = options.bin?.trim() ?? "";
    this.bin = explicit.length > 0 ? explicit : (process.env.LUVUS_BIN_PATH?.trim() ?? "") || "luvus";
    this.timeoutMs = options.timeoutMs ?? 15000;
    this.exec = options.exec;
  }
  async runJson(action, args, signal) {
    if (signal?.aborted === true) {
      throw new CliInspectError(action, "aborted");
    }
    let stdout;
    try {
      const result = signal === undefined ? await this.exec(this.bin, [...args, "--json"], {
        timeout: this.timeoutMs
      }) : await this.exec(this.bin, [...args, "--json"], {
        timeout: this.timeoutMs,
        signal
      });
      if (result.killed || result.code !== 0) {
        throw new CliInspectError(action, "command failed");
      }
      stdout = truncateOutput(result.stdout, MAX_INSPECT_OUTPUT_BYTES);
    } catch (error) {
      if (error instanceof CliInspectError)
        throw error;
      if (typeof error === "object" && error !== null && error.name === "AbortError") {
        throw new CliInspectError(action, "aborted");
      }
      throw new CliInspectError(action, "execution failed");
    }
    if (stdout.trim().length === 0)
      return [];
    try {
      return JSON.parse(stdout);
    } catch {
      throw new CliInspectError(action, "invalid json output");
    }
  }
  async agentList(signal) {
    return this.runJson("fleet/list", ["agent", "list"], signal);
  }
  async agentGet(paneId, signal) {
    if (paneId.length === 0)
      throw new CliInspectError("agent/get", "empty pane id");
    return this.runJson("agent/get", ["agent", "get", paneId], signal);
  }
  async agentExplain(paneId, signal) {
    if (paneId.length === 0) {
      throw new CliInspectError("agent/explain", "empty pane id");
    }
    return this.runJson("agent/explain", ["agent", "explain", paneId], signal);
  }
  async taskList(signal) {
    return this.runJson("task/list", ["task", "list"], signal);
  }
  async taskGet(taskId, signal) {
    if (taskId.length === 0)
      throw new CliInspectError("task/get", "empty task id");
    return this.runJson("task/get", ["task", "get", taskId], signal);
  }
  async leaseList(signal) {
    return this.runJson("lease/list", ["lease", "list"], signal);
  }
  async missionSnapshot(_signal) {
    throw new CliInspectError("mission/snapshot", "unavailable over CLI: mission snapshot is only available over UHP; use fleet/list or workspace/list instead");
  }
  async workspaceList(signal) {
    return this.runJson("workspace/list", ["workspace", "list"], signal);
  }
}

// src/tools/inspect.ts
var INSPECT_TOOL_NAME2 = "luvus_inspect";
var INSPECT_ACTIONS = [
  "fleet/list",
  "agent/get",
  "agent/explain",
  "task/list",
  "task/get",
  "lease/list",
  "mission/snapshot",
  "workspace/list",
  "connection/status"
];
var InspectParams = Type4.Object({
  action: StringEnum3(INSPECT_ACTIONS, {
    description: "Read-only inspection action"
  }),
  target: Type4.Optional(Type4.String({
    description: "Pane id for agent/get|agent/explain, or task id for task/get"
  }))
});
var TARGET_REQUIRED = [
  "agent/get",
  "agent/explain",
  "task/get"
];
var MAX_TABLE_ROWS = 50;
function truncate3(text) {
  const result = truncateHead3(text, {
    maxLines: DEFAULT_MAX_LINES3,
    maxBytes: DEFAULT_MAX_BYTES3
  });
  if (!result.truncated)
    return result.content;
  return `${result.content}

[Output truncated: showing ${result.outputLines} of ` + `${result.totalLines} lines. Full output via CLI: luvus <cmd> --json]`;
}
function isRecord7(value) {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
function shortText4(value, max = 120) {
  if (typeof value !== "string")
    return "";
  const flat = value.replace(/\s+/g, " ").trim();
  return flat.length <= max ? flat : `${flat.slice(0, max - 3)}...`;
}
function projectRows(value) {
  if (Array.isArray(value)) {
    return value.filter(isRecord7).slice(0, MAX_TABLE_ROWS);
  }
  if (isRecord7(value)) {
    for (const key of ["agents", "tasks", "leases", "workspaces", "panes", "items", "rows"]) {
      if (Array.isArray(value[key])) {
        return value[key].filter(isRecord7).slice(0, MAX_TABLE_ROWS);
      }
    }
    return [value];
  }
  return [];
}
function formatRowCells(row) {
  const pane = typeof row.pane === "string" ? row.pane : typeof row.pane_id === "string" ? row.pane_id : typeof row.id === "string" ? row.id : "?";
  const agent = typeof row.agent === "string" ? row.agent : typeof row.name === "string" ? row.name : "?";
  const status = typeof row.status === "string" ? row.status : typeof row.agent_status === "string" ? row.agent_status : typeof row.state === "string" ? row.state : "?";
  const authority = typeof row.authority === "string" ? row.authority : typeof row.source === "string" ? row.source : "?";
  const workspace = typeof row.workspace === "string" ? row.workspace : typeof row.workspaceName === "string" ? row.workspaceName : "";
  return `${pane} ${agent} ${status} ${authority}${workspace.length > 0 ? ` ${workspace}` : ""}`;
}
async function readThrough(deps, method, runUhp, runCli, signal) {
  const caps = deps.router.cachedCapabilities();
  const announced = caps !== undefined && deps.router.healthSnapshot().supportedMethods.includes(method);
  if (announced) {
    try {
      return { payload: await runUhp(signal), via: "uhp" };
    } catch {}
  }
  return { payload: await runCli(signal), via: "cli" };
}
async function handleFleetList(deps, signal) {
  const cached = deps.fleetCache.list();
  if (cached.length > 0) {
    const lines = cached.slice(0, MAX_TABLE_ROWS).map((entry) => `${entry.paneId} ${entry.agent} ${entry.status} ${entry.authority}` + `${entry.workspaceName !== undefined ? ` ${entry.workspaceName}` : ""}`);
    return truncate3(`Fleet (${cached.length} cached):
${lines.join(`
`)}`);
  }
  const { payload, via } = await readThrough(deps, INSPECT_UHP_METHODS.agentList, (sig) => deps.uhpInspector.agentList(sig), (sig) => deps.cliInspector.agentList(sig), signal);
  const rows = projectRows(payload);
  if (rows.length === 0)
    return "Fleet is empty.";
  const lines = rows.map(formatRowCells);
  return truncate3(`Fleet (${rows.length} via ${via}):
${lines.join(`
`)}`);
}
async function handleAgentGet(deps, target, explain, signal) {
  const method = explain ? INSPECT_UHP_METHODS.agentExplain : INSPECT_UHP_METHODS.agentGet;
  if (!explain) {
    const cached = deps.fleetCache.get(target);
    if (cached !== undefined) {
      return truncate3(`Agent ${cached.paneId}: ${cached.agent} ${cached.status} ${cached.authority} cwd=${shortText4(cached.cwd, 80)}`);
    }
  }
  const { payload, via } = await readThrough(deps, method, (sig) => explain ? deps.uhpInspector.agentExplain(target, sig) : deps.uhpInspector.agentGet(target, sig), (sig) => explain ? deps.cliInspector.agentExplain(target, sig) : deps.cliInspector.agentGet(target, sig), signal);
  if (payload === undefined || payload === null || Array.isArray(payload) && payload.length === 0) {
    return `Agent ${target} not found.`;
  }
  if (isRecord7(payload) && payload.found === false) {
    return `Agent ${target} not found.`;
  }
  const text = JSON.stringify(payload, null, 2);
  return truncate3(`${explain ? "Explanation" : "Agent"} for ${target} (via ${via}):
${text}`);
}
async function handleCollection(deps, method, label, runUhp, runCli, signal) {
  const { payload, via } = await readThrough(deps, method, runUhp, runCli, signal);
  const rows = projectRows(payload);
  if (rows.length === 0)
    return `${label} is empty.`;
  return truncate3(`${label} (${rows.length} via ${via}):
${rows.map(formatRowCells).join(`
`)}`);
}
async function handleTaskGet(deps, target, signal) {
  const { payload, via } = await readThrough(deps, INSPECT_UHP_METHODS.taskGet, (sig) => deps.uhpInspector.taskGet(target, sig), (sig) => deps.cliInspector.taskGet(target, sig), signal);
  if (payload === undefined || payload === null || Array.isArray(payload) && payload.length === 0) {
    return `Task ${target} not found.`;
  }
  return truncate3(`Task ${target} (via ${via}):
${JSON.stringify(payload, null, 2)}`);
}
function handleConnectionStatus(deps) {
  const health = deps.router.healthSnapshot();
  const status = deps.coordinator?.status();
  const fleet = deps.fleetCache.snapshot();
  const transport = health.hasCapabilities ? "UHP" : "CLI";
  const phase = status?.phase ?? "absent";
  const gen = (status?.generation ?? fleet.generation ?? "").slice(0, 8);
  const parts = [
    `Transport: ${transport}`,
    `coordinator: ${phase}`,
    `agents: ${fleet.size}`,
    `resyncs: ${status?.resyncCount ?? 0}`,
    `probe failures: ${health.probeFailures}`
  ];
  if (gen.length > 0)
    parts.push(`gen ${gen}`);
  if (health.outcomeUncertain)
    parts.push("outcome uncertain");
  if (health.coolingDown)
    parts.push("cooling down");
  if (deps.metrics !== undefined) {
    try {
      parts.push(`metrics: ${formatMetricsLine(deps.metrics.snapshot())}`);
    } catch {}
  }
  return truncate3(parts.join(" \xB7 "));
}
async function dispatch(deps, action, target, signal) {
  if (!INSPECT_ACTIONS.includes(action)) {
    throw new Error(`unknown inspect action: ${shortText4(action, 64)}. Available: ${INSPECT_ACTIONS.join(", ")}`);
  }
  if (TARGET_REQUIRED.includes(action) && (target === undefined || target.length === 0)) {
    throw new Error(`inspect action ${action} requires a non-empty target`);
  }
  switch (action) {
    case "fleet/list":
      return handleFleetList(deps, signal);
    case "agent/get":
      return handleAgentGet(deps, target, false, signal);
    case "agent/explain":
      return handleAgentGet(deps, target, true, signal);
    case "task/list":
      return handleCollection(deps, INSPECT_UHP_METHODS.taskList, "Tasks", (sig) => deps.uhpInspector.taskList(sig), (sig) => deps.cliInspector.taskList(sig), signal);
    case "task/get":
      return handleTaskGet(deps, target, signal);
    case "lease/list":
      return handleCollection(deps, INSPECT_UHP_METHODS.leaseList, "Leases", (sig) => deps.uhpInspector.leaseList(sig), (sig) => deps.cliInspector.leaseList(sig), signal);
    case "mission/snapshot": {
      const caps = deps.router.cachedCapabilities();
      const announced = caps !== undefined && deps.router.healthSnapshot().supportedMethods.includes(INSPECT_UHP_METHODS.missionSnapshot);
      if (announced) {
        const payload = await deps.uhpInspector.missionSnapshot(signal);
        return truncate3(`Mission snapshot (via uhp):
${JSON.stringify(payload, null, 2)}`);
      }
      return "Mission snapshot is only available over UHP. Use fleet/list or workspace/list instead.";
    }
    case "workspace/list":
      return handleCollection(deps, INSPECT_UHP_METHODS.workspaceList, "Workspaces", (sig) => deps.uhpInspector.workspaceList(sig), (sig) => deps.cliInspector.workspaceList(sig), signal);
    case "connection/status":
      return handleConnectionStatus(deps);
  }
}
function registerInspectTool(pi, deps) {
  pi.registerTool({
    name: INSPECT_TOOL_NAME2,
    label: "Luvus Inspect",
    description: "Inspect Luvus agents, tasks, leases, and fleet status (read-only)",
    promptSnippet: "Inspect Luvus agents, tasks, leases, and fleet status (read-only)",
    promptGuidelines: [
      "Use luvus_inspect for read-only queries about agents, tasks, leases, and fleet health.",
      "luvus_inspect never mutates workspace or agent state."
    ],
    parameters: InspectParams,
    async execute(toolCallId, params, signal, _onUpdate, _ctx) {
      if (signal?.aborted === true)
        throw new Error("luvus_inspect aborted");
      let text;
      try {
        text = await dispatch(deps, params.action, params.target, signal ?? undefined);
      } catch (error) {
        if (error instanceof Error) {
          throw new Error(error.message);
        }
        throw new Error("luvus_inspect failed");
      }
      return {
        content: [{ type: "text", text }],
        details: { action: params.action }
      };
    }
  });
}
function buildInspectDeps(parts) {
  return {
    fleetCache: parts.fleetCache,
    ...parts.metrics === undefined ? {} : { metrics: parts.metrics },
    router: parts.router,
    coordinator: parts.coordinator,
    uhpInspector: parts.uhpInspector,
    cliInspector: new CliInspector({
      exec: parts.exec,
      ...parts.binPath === undefined ? {} : { bin: parts.binPath }
    })
  };
}

// src/transport/scheduler.ts
class TelemetryLane {
  client;
  queue = [];
  pumping = false;
  constructor(client) {
    this.client = client;
  }
  get pendingCount() {
    return this.queue.length;
  }
  bind(sessionId) {
    return this.enqueue({ kind: "bind", sessionId, waiters: [] });
  }
  report(report) {
    const last = this.queue[this.queue.length - 1];
    if (last !== undefined && last.kind === "report") {
      last.report = report;
      return new Promise((resolve, reject) => {
        last.waiters.push({ resolve, reject });
        this.pump();
      });
    }
    return this.enqueue({ kind: "report", report, waiters: [] });
  }
  taskHeartbeat(taskId, ratio) {
    return this.enqueue({ kind: "heartbeat", taskId, ratio, waiters: [] });
  }
  release() {
    return this.enqueue({ kind: "release", waiters: [] });
  }
  drain() {
    return this.enqueue({ kind: "flush", waiters: [] });
  }
  enqueue(slot) {
    return new Promise((resolve, reject) => {
      slot.waiters.push({ resolve, reject });
      this.queue.push(slot);
      this.pump();
    });
  }
  pump() {
    if (this.pumping)
      return;
    this.pumping = true;
    this.drainQueue();
  }
  async drainQueue() {
    while (this.queue.length > 0) {
      const slot = this.queue.shift();
      if (slot === undefined)
        break;
      try {
        await this.execute(slot);
        for (const waiter of slot.waiters)
          waiter.resolve();
      } catch (error) {
        for (const waiter of slot.waiters)
          waiter.reject(error);
      }
    }
    this.pumping = false;
  }
  execute(slot) {
    switch (slot.kind) {
      case "bind":
        return this.client.bindSession(slot.sessionId);
      case "report":
        return this.client.reportAgent(slot.report);
      case "heartbeat":
        return this.client.taskHeartbeat(slot.taskId, slot.ratio);
      case "release":
        return this.client.releaseAgent();
      case "flush":
        return Promise.resolve();
    }
  }
}

// src/transport/uhp/capabilities.ts
var UHP_CAPABILITIES_METHOD = "uhp.capabilities";
var UHP_PROTOCOL_NAME = "luvus-uhp";
var UHP_PROTOCOL_MAJOR = 1;
var MAX_CAPABILITY_SESSION_LENGTH = 512;
var MAX_CAPABILITY_METHODS = 256;
var SERVER_GENERATION_PATTERN = /^[0-9a-f]{32}$/;
var METHOD_NAME_PATTERN = /^[A-Za-z0-9._:-]+$/;
var UHP_SCOPES = [
  "read",
  "workspace",
  "agent",
  "terminal",
  "orchestration",
  "extensions",
  "admin",
  "all"
];

class UhpCapabilityError extends Error {
  reason;
  constructor(reason, detail) {
    super(detail !== undefined ? `uhp capabilities ${reason}: ${detail}` : `uhp capabilities ${reason}`);
    this.name = "UhpCapabilityError";
    this.reason = reason;
  }
}
function isRecord8(value) {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
function fail5(hint) {
  throw new UhpCapabilityError("invalid-shape", hint);
}
function checkString(value, hint, maxLength, pattern) {
  if (typeof value !== "string" || value.length === 0 || value.length > maxLength) {
    fail5(hint);
  }
  if (pattern !== undefined && !pattern.test(value))
    fail5(hint);
  return value;
}
function parseUhpCapabilities(result) {
  if (!isRecord8(result))
    fail5("result object");
  const root = result;
  if (root.type !== "uhp_capabilities")
    fail5("type");
  if (!isRecord8(root.protocol))
    fail5("protocol");
  const protocol = root.protocol;
  if (protocol.name !== UHP_PROTOCOL_NAME)
    fail5("protocol.name");
  if (protocol.major !== UHP_PROTOCOL_MAJOR)
    fail5("protocol.major");
  if (typeof protocol.minor !== "number" || !Number.isInteger(protocol.minor) || protocol.minor < 0) {
    fail5("protocol.minor");
  }
  const serverGeneration = checkString(root.server_generation, "server_generation", 32, SERVER_GENERATION_PATTERN);
  const session = checkString(root.session, "session", MAX_CAPABILITY_SESSION_LENGTH);
  if (typeof root.event_sequence !== "number" || !Number.isSafeInteger(root.event_sequence) || root.event_sequence < 0) {
    fail5("event_sequence");
  }
  if (!Array.isArray(root.methods) || root.methods.length > MAX_CAPABILITY_METHODS) {
    fail5("methods");
  }
  const methods = [];
  const seen = new Set;
  for (const entry of root.methods) {
    if (typeof entry !== "string" || entry.length === 0 || entry.length > MAX_METHOD_LENGTH || !METHOD_NAME_PATTERN.test(entry) || seen.has(entry)) {
      fail5("methods");
    }
    seen.add(entry);
    methods.push(entry);
  }
  if (!Array.isArray(root.method_contracts))
    fail5("method_contracts");
  const contracts = Object.create(null);
  for (const entry of root.method_contracts) {
    if (!isRecord8(entry))
      fail5("method_contracts");
    const record = entry;
    if (typeof record.method !== "string" || !seen.has(record.method)) {
      fail5("method_contracts");
    }
    if (record.method in contracts)
      fail5("method_contracts");
    if (record.access !== "read" && record.access !== "write") {
      fail5("method_contracts");
    }
    if (typeof record.idempotent !== "boolean")
      fail5("method_contracts");
    if (typeof record.scope !== "string" || !UHP_SCOPES.includes(record.scope)) {
      fail5("method_contracts");
    }
    contracts[record.method] = Object.freeze({
      access: record.access,
      idempotent: record.idempotent,
      scope: record.scope
    });
  }
  for (const method of methods) {
    if (!(method in contracts))
      fail5("method_contracts");
  }
  if (!isRecord8(root.limits))
    fail5("limits");
  const frameBytes = root.limits.frame_bytes;
  if (typeof frameBytes !== "number" || !Number.isSafeInteger(frameBytes) || frameBytes <= 0 || frameBytes > MAX_FRAME_BYTES) {
    fail5("limits.frame_bytes");
  }
  const frozenMethods = Object.freeze([...methods]);
  const frozenContracts = Object.freeze({ ...contracts });
  const capabilities = {
    protocol: Object.freeze({
      name: UHP_PROTOCOL_NAME,
      major: UHP_PROTOCOL_MAJOR,
      minor: protocol.minor
    }),
    serverGeneration,
    session,
    eventSequence: root.event_sequence,
    methods: frozenMethods,
    contracts: frozenContracts,
    limits: Object.freeze({ frameBytes }),
    supports(method) {
      return frozenMethods.includes(method);
    },
    contract(method) {
      return frozenContracts[method];
    }
  };
  return Object.freeze(capabilities);
}
async function fetchUhpCapabilities(requester, path, options) {
  const result = await requester.request(path, UHP_CAPABILITIES_METHOD, {}, {
    signal: options?.signal
  });
  return parseUhpCapabilities(result);
}

class UhpCapabilityCache {
  cached;
  get() {
    return this.cached;
  }
  update(next) {
    const current = this.cached;
    if (current !== undefined && next.serverGeneration === current.serverGeneration && next.eventSequence < current.eventSequence) {
      this.cached = undefined;
      throw new UhpCapabilityError("stale-event-sequence");
    }
    this.cached = next;
  }
  invalidate() {
    this.cached = undefined;
  }
}

// src/transport/uhp/client.ts
class UhpClient {
  pane;
  source;
  agent;
  socketPath;
  requester;
  constructor(options) {
    if (options.pane.length === 0) {
      throw new Error("UhpClient requires a non-empty pane");
    }
    if (options.source.length === 0) {
      throw new Error("UhpClient requires a non-empty source");
    }
    validateSocketPath(options.socketPath);
    this.pane = options.pane;
    this.source = options.source;
    this.agent = options.agent !== undefined && options.agent.length > 0 ? options.agent : DEFAULT_AGENT;
    this.socketPath = options.socketPath;
    this.requester = options.requester ?? new OneShotRequester({ timeoutMs: options.timeoutMs });
  }
  async bindSession(sessionId, options) {
    if (sessionId.length === 0) {
      throw new Error("bindSession requires a non-empty sessionId");
    }
    await this.requester.request(this.socketPath, "pane.report_session", { pane: this.pane, agent: this.agent, session_id: sessionId }, { signal: options?.signal });
  }
  async reportAgent(report, options) {
    if (report.pane !== this.pane) {
      throw new Error(`reportAgent pane mismatch: client is ${this.pane}, report is ${report.pane}`);
    }
    if (report.source !== this.source) {
      throw new Error(`reportAgent source mismatch: client is ${this.source}, report is ${report.source}`);
    }
    if (report.agent !== this.agent) {
      throw new Error(`reportAgent agent mismatch: client is ${this.agent}, report is ${report.agent}`);
    }
    const params = {
      pane: this.pane,
      source: this.source,
      agent: this.agent,
      status: report.status
    };
    if (report.message !== undefined && report.message.length > 0) {
      params.message = report.message;
    }
    if (report.sessionId !== undefined && report.sessionId.length > 0) {
      params.session_id = report.sessionId;
    }
    if (report.sequence !== undefined) {
      if (!Number.isSafeInteger(report.sequence) || report.sequence <= 0) {
        throw new RangeError(`reportAgent sequence must be a safe integer > 0; received ${String(report.sequence)}`);
      }
      params.sequence = report.sequence;
    }
    if (report.ttlSeconds !== undefined) {
      if (!Number.isSafeInteger(report.ttlSeconds) || report.ttlSeconds < 1 || report.ttlSeconds > MAX_TTL_SECONDS) {
        throw new RangeError(`reportAgent ttlSeconds must be a safe integer within [1, ${MAX_TTL_SECONDS}]; received ${String(report.ttlSeconds)}`);
      }
      params.ttl_s = report.ttlSeconds;
    }
    await this.requester.request(this.socketPath, "agent.report", params, {
      signal: options?.signal
    });
  }
  async taskHeartbeat(taskId, ratio, options) {
    if (taskId.length === 0) {
      throw new Error("taskHeartbeat requires a non-empty taskId");
    }
    if (!Number.isFinite(ratio) || ratio < 0 || ratio > 1) {
      throw new RangeError(`taskHeartbeat ratio must be within [0, 1]; received ${String(ratio)}`);
    }
    await this.requester.request(this.socketPath, "task.heartbeat", { id: taskId, context: ratio }, { signal: options?.signal });
  }
  async releaseAgent(options) {
    await this.requester.request(this.socketPath, "agent.release", { pane: this.pane, source: this.source }, { signal: options?.signal });
  }
}

// src/transport/uhp/snapshot.ts
var SESSION_SNAPSHOT_METHOD = "session.snapshot";
var MAX_SNAPSHOT_PANES = 1024;
var MAX_SNAPSHOT_WORKSPACES = 256;
var MAX_SNAPSHOT_TABS = 1024;
var MAX_SNAPSHOT_PANE_ENTRIES = 4096;
var MAX_SNAPSHOT_SESSION_LENGTH = 512;
var MAX_SNAPSHOT_NAME_LENGTH = 256;
var MAX_SNAPSHOT_PATH_LENGTH = 4096;
var MAX_SNAPSHOT_ID_LENGTH = 128;
var MAX_PANE_ID_U32 = 4294967295n;
var SERVER_GENERATION_PATTERN2 = /^[0-9a-f]{32}$/;
var CANONICAL_U32_PATTERN = /^[1-9][0-9]*$/;
var AGENT_STATUSES = [
  "idle",
  "working",
  "blocked",
  "done"
];

class UhpSnapshotError extends Error {
  reason;
  constructor(reason, detail) {
    super(detail !== undefined ? `uhp snapshot ${reason}: ${detail}` : `uhp snapshot ${reason}`);
    this.name = "UhpSnapshotError";
    this.reason = reason;
  }
}
function fail6(hint) {
  throw new UhpSnapshotError("invalid-shape", hint);
}
function isRecord9(value) {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
function checkInt(value, hint, minimum) {
  if (typeof value !== "number" || !Number.isSafeInteger(value) || value < minimum) {
    fail6(hint);
  }
  return value;
}
function checkString2(value, hint, maxLength, allowEmpty) {
  if (typeof value !== "string" || !allowEmpty && value.length === 0 || value.length > maxLength) {
    fail6(hint);
  }
  return value;
}
function optionalString(value, hint, maxLength) {
  if (value === null || value === undefined)
    return;
  return checkString2(value, hint, maxLength, true);
}
function checkPaneId(value) {
  if (typeof value !== "string" || !CANONICAL_U32_PATTERN.test(value)) {
    fail6("pane.pane_id");
  }
  const id = value;
  if (BigInt(id) > MAX_PANE_ID_U32)
    fail6("pane.pane_id");
  return id;
}
function parsePane(value, workspace, tabIndex, tabName) {
  if (!isRecord9(value))
    fail6("pane");
  const pane = value;
  if (pane.kind !== "terminal")
    return;
  const status = pane.agent_status;
  if (typeof status !== "string" || !AGENT_STATUSES.includes(status)) {
    fail6("pane.agent_status");
  }
  if (typeof pane.focused !== "boolean")
    fail6("pane.focused");
  const agentSession = optionalString(pane.agent_session, "pane.agent_session", 512);
  return Object.freeze({
    paneId: checkPaneId(pane.pane_id),
    workspaceIndex: workspace.index,
    ...workspace.name === undefined ? {} : { workspaceName: workspace.name },
    ...workspace.branch === undefined ? {} : { workspaceBranch: workspace.branch },
    tabIndex,
    ...tabName === undefined ? {} : { tabName },
    agent: checkString2(pane.agent, "pane.agent", MAX_SNAPSHOT_ID_LENGTH, false),
    status,
    authority: checkString2(pane.agent_authority, "pane.authority", MAX_SNAPSHOT_ID_LENGTH, false),
    ...agentSession === undefined ? {} : { agentSession },
    cwd: checkString2(pane.cwd, "pane.cwd", MAX_SNAPSHOT_PATH_LENGTH, false),
    terminalId: checkString2(pane.terminal_id, "pane.terminal_id", MAX_SNAPSHOT_ID_LENGTH, false),
    contentRevision: checkInt(pane.content_revision, "pane.content_revision", 0),
    focused: pane.focused
  });
}
function parseSessionSnapshot(result) {
  if (!isRecord9(result))
    fail6("result object");
  const root = result;
  if (root.type !== "session_snapshot")
    fail6("type");
  if (!isRecord9(root.protocol))
    fail6("protocol");
  const protocol = root.protocol;
  if (protocol.name !== "luvus-uhp")
    fail6("protocol.name");
  if (protocol.major !== 1)
    fail6("protocol.major");
  if (typeof protocol.minor !== "number" || !Number.isInteger(protocol.minor) || protocol.minor < 0) {
    fail6("protocol.minor");
  }
  const serverGeneration = checkString2(root.server_generation, "server_generation", 32, false);
  if (!SERVER_GENERATION_PATTERN2.test(serverGeneration)) {
    fail6("server_generation");
  }
  const session = checkString2(root.session, "session", MAX_SNAPSHOT_SESSION_LENGTH, false);
  const eventSequence = checkInt(root.event_sequence, "event_sequence", 0);
  if (!Array.isArray(root.workspaces))
    fail6("workspaces");
  const panes = [];
  let workspaceCount = 0;
  let tabCount = 0;
  let paneEntries = 0;
  for (const workspaceValue of root.workspaces) {
    workspaceCount += 1;
    if (workspaceCount > MAX_SNAPSHOT_WORKSPACES)
      fail6("workspaces");
    if (!isRecord9(workspaceValue))
      fail6("workspace");
    const workspace = workspaceValue;
    const workspaceIndex = checkInt(workspace.index, "workspace.index", 0);
    if (!Array.isArray(workspace.tabs))
      fail6("workspace.tabs");
    const context = {
      index: workspaceIndex,
      name: optionalString(workspace.name, "workspace.name", MAX_SNAPSHOT_NAME_LENGTH),
      branch: optionalString(workspace.branch, "workspace.branch", MAX_SNAPSHOT_NAME_LENGTH),
      cwd: checkString2(workspace.cwd, "workspace.cwd", MAX_SNAPSHOT_PATH_LENGTH, false)
    };
    for (const tabValue of workspace.tabs) {
      tabCount += 1;
      if (tabCount > MAX_SNAPSHOT_TABS)
        fail6("tabs");
      if (!isRecord9(tabValue))
        fail6("tab");
      const tab = tabValue;
      if (tab.kind !== "panes")
        continue;
      if (!Array.isArray(tab.panes))
        fail6("tab.panes");
      const tabIndex = checkInt(tab.index, "tab.index", 0);
      const tabName = optionalString(tab.name, "tab.name", MAX_SNAPSHOT_NAME_LENGTH);
      for (const paneValue of tab.panes) {
        paneEntries += 1;
        if (paneEntries > MAX_SNAPSHOT_PANE_ENTRIES)
          fail6("panes");
        const pane = parsePane(paneValue, context, tabIndex, tabName);
        if (pane === undefined)
          continue;
        if (panes.length >= MAX_SNAPSHOT_PANES) {
          throw new UhpSnapshotError("too-many-panes");
        }
        panes.push(pane);
      }
    }
  }
  return Object.freeze({
    header: Object.freeze({
      protocol: Object.freeze({
        name: "luvus-uhp",
        major: 1,
        minor: protocol.minor
      }),
      serverGeneration,
      session,
      eventSequence
    }),
    panes: Object.freeze([...panes])
  });
}
async function fetchSessionSnapshot(requester, path, options) {
  const result = await requester.request(path, SESSION_SNAPSHOT_METHOD, {}, options === undefined ? undefined : { signal: options.signal, maxFrameBytes: options.maxFrameBytes });
  return parseSessionSnapshot(result);
}

// src/transport/uhp/reconcile.ts
var DEFAULT_RECONCILE_BUFFER_EVENTS = 256;
var DEFAULT_RECONCILE_BUFFER_BYTES = 1024 * 1024;
var RESYNC_REQUIRED_EVENT = "events.resync_required";

class UhpSnapshotReconciler {
  serverGeneration;
  ackSequence;
  maxBufferedEvents;
  maxBufferedBytes;
  snapshotSink;
  eventSink;
  phase = "buffering";
  lastSequence;
  reason;
  buffered = [];
  bufferedBytes = 0;
  constructor(options) {
    if (typeof options.serverGeneration !== "string" || options.serverGeneration.length === 0) {
      throw new Error("reconciler requires a non-empty serverGeneration");
    }
    if (!Number.isSafeInteger(options.ackSequence) || options.ackSequence < 0) {
      throw new RangeError("reconciler ackSequence must be a safe integer >= 0");
    }
    const maxEvents = options.maxBufferedEvents ?? DEFAULT_RECONCILE_BUFFER_EVENTS;
    const maxBytes = options.maxBufferedBytes ?? DEFAULT_RECONCILE_BUFFER_BYTES;
    if (!Number.isInteger(maxEvents) || maxEvents <= 0) {
      throw new RangeError("reconciler maxBufferedEvents must be a positive integer");
    }
    if (!Number.isInteger(maxBytes) || maxBytes <= 0) {
      throw new RangeError("reconciler maxBufferedBytes must be a positive integer");
    }
    this.serverGeneration = options.serverGeneration;
    this.ackSequence = options.ackSequence;
    this.maxBufferedEvents = maxEvents;
    this.maxBufferedBytes = maxBytes;
    this.snapshotSink = options.applySnapshot;
    this.eventSink = options.applyEvent;
  }
  status() {
    return {
      phase: this.phase,
      ...this.lastSequence === undefined ? {} : { lastSequence: this.lastSequence },
      ...this.reason === undefined ? {} : { reason: this.reason },
      bufferedEvents: this.buffered.length,
      bufferedBytes: this.bufferedBytes
    };
  }
  bufferEvent(event, wireBytes) {
    if (!this.precheck(event, wireBytes))
      return;
    if (this.phase === "live") {
      this.applyLive(event);
      return;
    }
    this.store(event, wireBytes);
  }
  applyEvent(event, wireBytes) {
    if (!this.precheck(event, wireBytes))
      return;
    if (this.phase === "buffering") {
      this.store(event, wireBytes);
      return;
    }
    this.applyLive(event);
  }
  precheck(event, wireBytes) {
    if (this.phase === "needs_resync")
      return false;
    if (!validWireBytes(wireBytes)) {
      this.enterResync("invalid-frame");
      return false;
    }
    if (event.event === RESYNC_REQUIRED_EVENT) {
      this.enterResync("resync-event");
      return false;
    }
    return true;
  }
  store(event, wireBytes) {
    if (this.buffered.length + 1 > this.maxBufferedEvents || this.bufferedBytes + wireBytes > this.maxBufferedBytes) {
      this.enterResync("buffer-overflow");
      return;
    }
    this.buffered.push({ event, wireBytes });
    this.bufferedBytes += wireBytes;
  }
  applySnapshot(snapshot) {
    if (this.phase === "needs_resync")
      return;
    if (snapshot.header.serverGeneration !== this.serverGeneration) {
      this.enterResync("generation-mismatch");
      return;
    }
    if (snapshot.header.eventSequence < this.ackSequence) {
      this.enterResync("stale-snapshot");
      return;
    }
    const fence = snapshot.header.eventSequence;
    if (this.snapshotSink !== undefined) {
      try {
        this.snapshotSink(snapshot);
      } catch {
        this.enterResync("callback-failed");
        return;
      }
    }
    this.lastSequence = fence;
    let expected = fence + 1;
    for (const entry of this.buffered) {
      if (entry.event.sequence <= fence)
        continue;
      if (entry.event.sequence > expected) {
        this.enterResync("gap");
        return;
      }
      if (entry.event.sequence < expected) {
        this.enterResync("out-of-order");
        return;
      }
      if (!this.deliver(entry.event))
        return;
      expected += 1;
      this.lastSequence = expected - 1;
    }
    this.buffered = [];
    this.bufferedBytes = 0;
    this.phase = "live";
  }
  applyLive(event) {
    const last = this.lastSequence;
    if (last === undefined) {
      this.enterResync("gap");
      return;
    }
    if (event.sequence <= last)
      return;
    if (event.sequence > last + 1) {
      this.enterResync("gap");
      return;
    }
    if (!this.deliver(event))
      return;
    this.lastSequence = event.sequence;
  }
  deliver(event) {
    if (this.eventSink === undefined)
      return true;
    try {
      this.eventSink(event);
      return true;
    } catch {
      this.enterResync("callback-failed");
      return false;
    }
  }
  enterResync(reason) {
    this.phase = "needs_resync";
    this.reason = reason;
    this.buffered = [];
    this.bufferedBytes = 0;
  }
}
function validWireBytes(value) {
  return Number.isSafeInteger(value) && value >= 1;
}

// src/transport/uhp/reconnect.ts
var UHP_BACKOFF_STEPS = [250, 1000, 2000, 5000];

class UhpBackoff {
  clock;
  failures = 0;
  notBeforeMs;
  constructor(clock = () => Date.now()) {
    this.clock = clock;
  }
  canProbe() {
    return this.notBeforeMs === undefined || this.clock() >= this.notBeforeMs;
  }
  recordFailure() {
    this.failures += 1;
    const step = UHP_BACKOFF_STEPS[Math.min(this.failures - 1, UHP_BACKOFF_STEPS.length - 1)];
    const deadline = this.clock() + step;
    this.notBeforeMs = this.notBeforeMs === undefined ? deadline : Math.max(this.notBeforeMs, deadline);
  }
  recordSuccess() {
    this.failures = 0;
    this.notBeforeMs = undefined;
  }
  force() {
    this.notBeforeMs = undefined;
  }
  reset() {
    this.failures = 0;
    this.notBeforeMs = undefined;
  }
  snapshot() {
    return {
      failures: this.failures,
      notBeforeMs: this.notBeforeMs,
      canProbe: this.canProbe()
    };
  }
}

// src/transport/uhp/coordinator.ts
function isAbortError(error) {
  return error instanceof UhpTransportError && error.stage === "aborted";
}

class UhpEventCoordinator {
  endpoint;
  eventClient;
  requester;
  capabilityCache;
  backoff;
  applySnapshotCb;
  applyEventCb;
  onResyncCb;
  onGenerationChangeCb;
  maxBufferedEvents;
  maxBufferedBytes;
  epoch = 0;
  cycle = 0;
  startedFlag = false;
  stoppedFlag = false;
  generation;
  reconciler;
  reconcilerGeneration;
  subscription;
  resyncCount = 0;
  startPromise;
  startResolve;
  startReject;
  startEpoch;
  runPromise;
  stopPromise;
  sleepTimer;
  sleepWake;
  constructor(deps) {
    if (deps === null || typeof deps !== "object" || typeof deps.endpoint !== "string" || deps.endpoint.length === 0) {
      throw new TypeError("coordinator requires a non-empty endpoint");
    }
    if (deps.eventClient === null || deps.eventClient === undefined || typeof deps.eventClient.open !== "function") {
      throw new TypeError("coordinator requires an eventClient");
    }
    if (deps.requester === null || deps.requester === undefined || typeof deps.requester.request !== "function") {
      throw new TypeError("coordinator requires a requester");
    }
    if (!(deps.capabilityCache instanceof UhpCapabilityCache)) {
      throw new TypeError("coordinator requires a capabilityCache");
    }
    this.endpoint = deps.endpoint;
    this.eventClient = deps.eventClient;
    this.requester = deps.requester;
    this.capabilityCache = deps.capabilityCache;
    this.backoff = deps.backoff ?? new UhpBackoff;
    this.applySnapshotCb = deps.applySnapshot;
    this.applyEventCb = deps.applyEvent;
    this.onResyncCb = deps.onResync;
    this.onGenerationChangeCb = deps.onGenerationChange;
    this.maxBufferedEvents = deps.maxBufferedEvents;
    this.maxBufferedBytes = deps.maxBufferedBytes;
  }
  get started() {
    return this.startedFlag;
  }
  get stopped() {
    return this.stoppedFlag;
  }
  status() {
    let phase;
    if (this.stoppedFlag) {
      phase = "stopped";
    } else if (this.startedFlag && this.reconciler !== undefined && this.subscription !== undefined && this.reconciler.status().phase === "live") {
      phase = "live";
    } else if (this.startedFlag) {
      phase = "connecting";
    } else {
      phase = "idle";
    }
    return {
      phase,
      reconciler: this.reconciler?.status(),
      generation: this.generation,
      backoff: this.backoff.snapshot(),
      resyncCount: this.resyncCount
    };
  }
  start() {
    if (this.startedFlag && !this.stoppedFlag && this.startPromise !== undefined) {
      return this.startPromise;
    }
    this.epoch += 1;
    const epoch = this.epoch;
    this.cycle += 1;
    this.startedFlag = true;
    this.stoppedFlag = false;
    this.stopPromise = undefined;
    this.reconciler = undefined;
    this.reconcilerGeneration = undefined;
    this.subscription = undefined;
    let resolveFn;
    let rejectFn;
    this.startPromise = new Promise((resolve, reject) => {
      resolveFn = resolve;
      rejectFn = reject;
    });
    this.startPromise.catch(() => {});
    this.startResolve = resolveFn;
    this.startReject = rejectFn;
    this.startEpoch = epoch;
    this.runPromise = this.run(epoch).catch(() => {});
    return this.startPromise;
  }
  stop() {
    if (this.stoppedFlag && this.stopPromise !== undefined) {
      return this.stopPromise;
    }
    this.stoppedFlag = true;
    this.startedFlag = false;
    if (this.startReject !== undefined) {
      const reject = this.startReject;
      this.startReject = undefined;
      this.startResolve = undefined;
      reject(new Error("uhp coordinator stopped before live"));
    }
    this.interruptSleep();
    const sub = this.subscription;
    this.subscription = undefined;
    let done;
    if (sub !== undefined) {
      try {
        sub.close();
      } catch {}
      done = Promise.resolve().then(() => sub.closed).then(() => {}, () => {});
    } else {
      done = Promise.resolve();
    }
    this.stopPromise = done;
    return done;
  }
  resolveStartIfPending(epoch) {
    if (this.startEpoch === epoch && this.startResolve !== undefined) {
      const resolve = this.startResolve;
      this.startResolve = undefined;
      this.startReject = undefined;
      resolve();
    }
  }
  safeNotifyResync(reason) {
    if (this.onResyncCb === undefined)
      return;
    try {
      this.onResyncCb(reason);
    } catch {}
  }
  safeNotifyGeneration(generation) {
    if (this.onGenerationChangeCb === undefined)
      return;
    try {
      this.onGenerationChangeCb(generation);
    } catch {}
  }
  cancellableSleep(ms) {
    if (ms <= 0) {
      return Promise.resolve();
    }
    return new Promise((resolve) => {
      this.sleepWake = () => {
        if (this.sleepTimer !== undefined) {
          clearTimeout(this.sleepTimer);
          this.sleepTimer = undefined;
        }
        this.sleepWake = undefined;
        resolve();
      };
      this.sleepTimer = setTimeout(() => {
        this.sleepTimer = undefined;
        this.sleepWake = undefined;
        resolve();
      }, ms);
      if (typeof this.sleepTimer === "object" && this.sleepTimer !== null) {
        const timer = this.sleepTimer;
        if (typeof timer.unref === "function") {
          try {
            timer.unref();
          } catch {}
        }
      }
    });
  }
  interruptSleep() {
    const wake = this.sleepWake;
    this.sleepWake = undefined;
    if (this.sleepTimer !== undefined) {
      clearTimeout(this.sleepTimer);
      this.sleepTimer = undefined;
    }
    if (wake !== undefined) {
      try {
        wake();
      } catch {}
    }
  }
  async run(epoch) {
    for (;; ) {
      if (this.stoppedFlag || epoch !== this.epoch)
        return;
      if (!this.backoff.canProbe()) {
        const snap = this.backoff.snapshot();
        const rawDelay = snap.notBeforeMs !== undefined ? snap.notBeforeMs - Date.now() : 0;
        const delay = Number.isFinite(rawDelay) ? Math.max(0, rawDelay) : 0;
        const slice = delay > 0 ? Math.min(delay, 10) : 5;
        await this.cancellableSleep(slice);
        continue;
      }
      let serverGeneration;
      let ackFence;
      try {
        const caps = await fetchUhpCapabilities(this.requester, this.endpoint);
        if (this.stoppedFlag || epoch !== this.epoch)
          return;
        try {
          this.capabilityCache.update(caps);
        } catch {
          this.backoff.recordFailure();
          continue;
        }
        serverGeneration = caps.serverGeneration;
        ackFence = caps.eventSequence;
      } catch (error) {
        if (this.stoppedFlag || epoch !== this.epoch)
          return;
        if (isAbortError(error))
          return;
        this.backoff.recordFailure();
        continue;
      }
      if (serverGeneration !== this.generation) {
        this.generation = serverGeneration;
        this.safeNotifyGeneration(serverGeneration);
      }
      let afterSequence;
      const previous = this.reconciler;
      const previousGeneration = this.reconcilerGeneration;
      if (previous !== undefined && previousGeneration === serverGeneration) {
        const last = previous.status().lastSequence;
        if (last !== undefined) {
          afterSequence = last;
        }
      }
      const reconciler = new UhpSnapshotReconciler({
        serverGeneration,
        ackSequence: ackFence,
        ...this.maxBufferedEvents !== undefined ? { maxBufferedEvents: this.maxBufferedEvents } : {},
        ...this.maxBufferedBytes !== undefined ? { maxBufferedBytes: this.maxBufferedBytes } : {},
        ...this.applySnapshotCb !== undefined ? { applySnapshot: this.applySnapshotCb } : {},
        ...this.applyEventCb !== undefined ? { applyEvent: this.applyEventCb } : {}
      });
      this.cycle += 1;
      const cycle = this.cycle;
      let live = false;
      let resyncReported = false;
      const reportOnce = (reason) => {
        if (epoch !== this.epoch || cycle !== this.cycle)
          return;
        if (resyncReported)
          return;
        resyncReported = true;
        this.resyncCount += 1;
        this.safeNotifyResync(reason);
      };
      let sub;
      try {
        sub = await this.eventClient.open(this.endpoint, {
          ...afterSequence !== undefined ? { afterSequence } : {},
          onAck: () => {
            if (epoch !== this.epoch || cycle !== this.cycle)
              return;
          },
          onEvent: (event, wireBytes) => {
            if (epoch !== this.epoch || cycle !== this.cycle)
              return;
            if (this.stoppedFlag)
              return;
            if (live) {
              reconciler.applyEvent(event, wireBytes);
            } else {
              reconciler.bufferEvent(event, wireBytes);
            }
            const current = reconciler.status();
            if (current.phase === "needs_resync" && !resyncReported) {
              reportOnce(current.reason ?? "gap");
              try {
                sub.close();
              } catch {}
            }
          }
        });
      } catch (error) {
        if (this.stoppedFlag || epoch !== this.epoch)
          return;
        if (isAbortError(error))
          return;
        this.backoff.recordFailure();
        continue;
      }
      if (this.stoppedFlag || epoch !== this.epoch) {
        try {
          sub.close();
        } catch {}
        return;
      }
      this.subscription = sub;
      let snapshot;
      try {
        snapshot = await fetchSessionSnapshot(this.requester, this.endpoint);
      } catch {
        try {
          sub.close();
        } catch {}
        if (this.subscription === sub) {
          this.subscription = undefined;
        }
        if (this.stoppedFlag || epoch !== this.epoch)
          return;
        this.backoff.recordFailure();
        continue;
      }
      if (this.stoppedFlag || epoch !== this.epoch) {
        try {
          sub.close();
        } catch {}
        return;
      }
      if (cycle !== this.cycle) {
        try {
          sub.close();
        } catch {}
        return;
      }
      reconciler.applySnapshot(snapshot);
      const drained = reconciler.status();
      if (drained.phase === "needs_resync") {
        try {
          sub.close();
        } catch {}
        if (this.subscription === sub) {
          this.subscription = undefined;
        }
        this.reconciler = reconciler;
        this.reconcilerGeneration = serverGeneration;
        reportOnce(drained.reason ?? "gap");
        this.backoff.recordFailure();
        continue;
      }
      this.reconciler = reconciler;
      this.reconcilerGeneration = serverGeneration;
      live = true;
      this.backoff.recordSuccess();
      this.resolveStartIfPending(epoch);
      let closed;
      try {
        closed = await sub.closed;
      } catch {
        if (this.subscription === sub) {
          this.subscription = undefined;
        }
        if (this.stoppedFlag || epoch !== this.epoch)
          return;
        if (!resyncReported) {
          reportOnce("gap");
        }
        this.backoff.recordFailure();
        continue;
      }
      if (this.subscription === sub) {
        this.subscription = undefined;
      }
      if (this.stoppedFlag || epoch !== this.epoch)
        return;
      if (cycle !== this.cycle)
        return;
      if (!resyncReported) {
        const current = reconciler.status();
        if (current.phase === "needs_resync") {
          reportOnce(current.reason ?? "gap");
        } else if (closed.resyncRequired) {
          reportOnce("gap");
        } else {
          this.backoff.recordFailure();
          continue;
        }
      }
      this.backoff.recordFailure();
    }
  }
}

// src/transport/uhp/events.ts
import { createConnection as createConnection2 } from "net";
var EVENTS_SUBSCRIBE_METHOD = "events.subscribe";
var MAX_EVENT_NAME_LENGTH = 128;
var MAX_ACK_QUEUE_CAPACITY = 4294967295;
class UhpStreamDecoder {
  parts = [];
  bytes = 0;
  maxFrameBytes;
  fatal = new TextDecoder("utf-8", { fatal: true });
  constructor(options = {}) {
    const max = options.maxFrameBytes ?? MAX_FRAME_BYTES;
    if (!Number.isInteger(max) || max < 1 || max > MAX_FRAME_BYTES) {
      throw new RangeError(`uhp maxFrameBytes must be an integer within [1, ${MAX_FRAME_BYTES}]; received ${String(max)}`);
    }
    this.maxFrameBytes = max;
  }
  get bufferedBytes() {
    return this.bytes;
  }
  push(chunk) {
    const buf = typeof chunk === "string" ? Buffer.from(chunk, "utf8") : chunk;
    const lines = [];
    let offset = 0;
    for (;; ) {
      const lf = buf.indexOf(10, offset);
      if (lf === -1) {
        const rest = buf.subarray(offset);
        if (rest.length > 0) {
          this.bytes += rest.length;
          if (this.bytes > this.maxFrameBytes) {
            throw new UhpFramingError("frame-too-large");
          }
          this.parts.push(rest);
        }
        return lines;
      }
      const segment = buf.subarray(offset, lf);
      this.bytes += segment.length + 1;
      if (this.bytes > this.maxFrameBytes) {
        throw new UhpFramingError("frame-too-large");
      }
      this.parts.push(segment);
      const lineBytes = Buffer.concat(this.parts);
      this.parts = [];
      this.bytes = 0;
      lines.push(this.decodeLine(lineBytes));
      offset = lf + 1;
    }
  }
  finish() {
    if (this.parts.length > 0 || this.bytes > 0) {
      throw new UhpFramingError("incomplete-frame");
    }
  }
  decodeLine(line) {
    try {
      return this.fatal.decode(line);
    } catch (error) {
      throw new UhpFramingError("malformed-json", { cause: error });
    }
  }
}
function parseUhpEvent(line) {
  let parsed;
  try {
    parsed = JSON.parse(line);
  } catch (error) {
    throw new UhpFramingError("malformed-json", { cause: error });
  }
  if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) {
    throw new UhpFramingError("bad-envelope");
  }
  const record = parsed;
  if (typeof record.event !== "string" || record.event.length < 1 || record.event.length > MAX_EVENT_NAME_LENGTH) {
    throw new UhpFramingError("bad-envelope");
  }
  if (typeof record.sequence !== "number" || !Number.isSafeInteger(record.sequence) || record.sequence < 1) {
    throw new UhpFramingError("bad-envelope");
  }
  if (typeof record.data !== "object" || record.data === null || Array.isArray(record.data)) {
    throw new UhpFramingError("bad-envelope");
  }
  return {
    ...record,
    event: record.event,
    sequence: record.sequence,
    data: record.data
  };
}
function parseSubscriptionAck(result) {
  const invalid = () => {
    throw new UhpFramingError("bad-envelope");
  };
  if (typeof result !== "object" || result === null || Array.isArray(result)) {
    invalid();
  }
  const record = result;
  if (record.type !== "subscription_started")
    invalid();
  if (typeof record.sequence !== "number" || !Number.isSafeInteger(record.sequence) || record.sequence < 0) {
    invalid();
  }
  if (typeof record.replayed !== "number" || !Number.isSafeInteger(record.replayed) || record.replayed < 0) {
    invalid();
  }
  if (typeof record.queue_capacity !== "number" || !Number.isSafeInteger(record.queue_capacity) || record.queue_capacity < 1 || record.queue_capacity > MAX_ACK_QUEUE_CAPACITY) {
    invalid();
  }
  if (record.loss_behavior !== "resync_required_then_close")
    invalid();
  return {
    sequence: record.sequence,
    replayed: record.replayed,
    queueCapacity: record.queue_capacity
  };
}

class UhpEventStreamClient {
  connect;
  generateId;
  validateEndpoint;
  timeoutMs;
  maxFrameBytes;
  constructor(options = {}) {
    if (options.timeoutMs !== undefined) {
      assertTimeoutMs2(options.timeoutMs);
    }
    if (options.maxFrameBytes !== undefined) {
      assertMaxFrameBytes(options.maxFrameBytes);
    }
    this.connect = options.connect ?? ((path) => createConnection2({ path }));
    this.generateId = options.generateId ?? defaultRequestId;
    this.validateEndpoint = options.validateEndpoint ?? defaultValidateEndpoint;
    this.timeoutMs = options.timeoutMs ?? DEFAULT_UHP_TIMEOUT_MS;
    this.maxFrameBytes = options.maxFrameBytes ?? MAX_FRAME_BYTES;
  }
  async open(path, options = {}) {
    const signal = options.signal;
    const timeoutMs = options.timeoutMs ?? this.timeoutMs;
    if (options.timeoutMs !== undefined) {
      assertTimeoutMs2(options.timeoutMs);
    }
    if (options.maxFrameBytes !== undefined) {
      assertMaxFrameBytes(options.maxFrameBytes);
    }
    const maxFrameBytes = Math.min(options.maxFrameBytes ?? this.maxFrameBytes, this.maxFrameBytes);
    if (signal?.aborted === true) {
      throw new UhpTransportError("aborted", "subscribe aborted before connect", {
        mayHaveExecuted: false
      });
    }
    validateSocketPath(path);
    if (options.afterSequence !== undefined && (!Number.isSafeInteger(options.afterSequence) || options.afterSequence < 0)) {
      throw new RangeError(`uhp after_sequence must be a safe integer >= 0; received ${String(options.afterSequence)}`);
    }
    const id = this.generateId();
    let frame;
    try {
      frame = encodeUhpRequest({
        id,
        method: EVENTS_SUBSCRIBE_METHOD,
        params: options.afterSequence === undefined ? {} : { after_sequence: options.afterSequence }
      });
    } catch (error) {
      if (error instanceof UhpFramingError) {
        throw new UhpTransportError("validate", `invalid request: ${error.reason}`, { mayHaveExecuted: false });
      }
      throw error;
    }
    if (new TextEncoder().encode(frame).length > maxFrameBytes) {
      throw new UhpTransportError("validate", "invalid request: frame-too-large", {
        mayHaveExecuted: false
      });
    }
    try {
      await this.validateEndpoint(path);
    } catch (error) {
      if (error instanceof UhpTransportError && error.stage === "validate" && error.mayHaveExecuted === false) {
        throw error;
      }
      throw new UhpTransportError("validate", "endpoint ownership validation failed", { mayHaveExecuted: false });
    }
    if (signal !== undefined && signal.aborted) {
      throw new UhpTransportError("aborted", "subscribe aborted before connect", {
        mayHaveExecuted: false
      });
    }
    const onAck = options.onAck;
    const onEvent = options.onEvent;
    const decoder = new UhpStreamDecoder({ maxFrameBytes });
    return new Promise((resolve, reject) => {
      let openSettled = false;
      let streamDone = false;
      let phase = "connect";
      let writeInitiated = false;
      const timer = setTimeout(() => {
        if (!openSettled) {
          failOpen(new UhpTransportError("timeout", `no subscription ack within ${timeoutMs}ms`, { mayHaveExecuted: writeInitiated }));
        }
      }, timeoutMs);
      timer.unref();
      let socket;
      try {
        socket = this.connect(path);
      } catch (error) {
        clearTimeout(timer);
        reject(new UhpTransportError("connect", "connect failed", {
          mayHaveExecuted: false
        }));
        return;
      }
      let closedResolve;
      const closed = new Promise((res) => {
        closedResolve = res;
      });
      const cleanup = () => {
        clearTimeout(timer);
        if (signal !== undefined) {
          signal.removeEventListener("abort", onAbort);
        }
        socket.removeAllListeners();
        try {
          socket.destroy();
        } catch {}
      };
      const finishClosed = (value) => {
        if (streamDone)
          return;
        streamDone = true;
        cleanup();
        closedResolve(value);
      };
      const failOpen = (error) => {
        if (openSettled)
          return;
        openSettled = true;
        cleanup();
        reject(error);
      };
      const onAbort = () => {
        if (!openSettled) {
          failOpen(new UhpTransportError("aborted", "subscribe aborted", {
            mayHaveExecuted: writeInitiated
          }));
        } else {
          finishClosed({ reason: "aborted", resyncRequired: true });
        }
      };
      if (signal !== undefined) {
        signal.addEventListener("abort", onAbort, { once: true });
      }
      if (signal !== undefined && signal.aborted) {
        failOpen(new UhpTransportError("aborted", "subscribe aborted", {
          mayHaveExecuted: writeInitiated
        }));
        return;
      }
      socket.once("connect", () => {
        if (signal !== undefined && signal.aborted) {
          failOpen(new UhpTransportError("aborted", "subscribe aborted", {
            mayHaveExecuted: writeInitiated
          }));
          return;
        }
        phase = "write";
        writeInitiated = true;
        try {
          socket.write(frame, "utf8", (error) => {
            if (openSettled)
              return;
            if (error !== undefined && error !== null) {
              failOpen(new UhpTransportError("write", "write failed", {
                mayHaveExecuted: true
              }));
              return;
            }
            phase = "awaiting-ack";
          });
        } catch {
          failOpen(new UhpTransportError("write", "write failed", {
            mayHaveExecuted: true
          }));
        }
      });
      const failSocketError = (error) => {
        if (!openSettled) {
          failOpen(new UhpTransportError(phase === "connect" ? "connect" : phase === "write" ? "write" : "read", `socket ${phase} failed`, { mayHaveExecuted: phase !== "connect" }));
        } else {
          finishClosed({ reason: "transport", resyncRequired: true });
        }
      };
      socket.once("error", failSocketError);
      socket.on("data", (chunk) => {
        if (streamDone)
          return;
        let lines;
        try {
          lines = decoder.push(chunk);
        } catch (error) {
          if (openSettled) {
            finishClosed({ reason: "protocol", resyncRequired: true });
          } else if (error instanceof UhpFramingError) {
            failOpen(new UhpTransportError("protocol", `invalid stream: ${error.reason}`, { mayHaveExecuted: true }));
          } else {
            failOpen(error);
          }
          return;
        }
        for (const line of lines) {
          if (streamDone)
            return;
          const wireBytes = new TextEncoder().encode(line).length + 1;
          if (!openSettled) {
            if (!handleAckLine(line))
              return;
            continue;
          }
          let event;
          try {
            event = parseUhpEvent(line);
          } catch {
            finishClosed({ reason: "protocol", resyncRequired: true });
            return;
          }
          if (onEvent !== undefined) {
            try {
              onEvent(event, wireBytes);
            } catch {
              finishClosed({ reason: "consumer", resyncRequired: true });
              return;
            }
          }
        }
      });
      socket.once("close", () => {
        if (streamDone)
          return;
        if (!openSettled) {
          try {
            decoder.finish();
          } catch (error) {
            if (error instanceof UhpFramingError) {
              failOpen(new UhpTransportError("protocol", `invalid stream: ${error.reason}`, { mayHaveExecuted: phase !== "connect" }));
            } else {
              failOpen(error);
            }
            return;
          }
          failOpen(new UhpTransportError(phase === "connect" ? "connect" : "read", "connection closed before subscription ack"));
          return;
        }
        try {
          decoder.finish();
        } catch {
          finishClosed({ reason: "protocol", resyncRequired: true });
          return;
        }
        finishClosed({ reason: "eof", resyncRequired: true });
      });
      function handleAckLine(line) {
        let result;
        try {
          const response = decodeUhpResponse(`${line}
`, id);
          result = unwrapUhpResponse(response);
        } catch (error) {
          if (error instanceof UhpRemoteError) {
            failOpen(error);
          } else if (error instanceof UhpFramingError) {
            failOpen(new UhpTransportError("protocol", `invalid stream: ${error.reason}`, { mayHaveExecuted: true }));
          } else {
            failOpen(error);
          }
          return false;
        }
        let ack;
        try {
          ack = parseSubscriptionAck(result);
        } catch (error) {
          if (error instanceof UhpFramingError) {
            failOpen(new UhpTransportError("protocol", `invalid stream: ${error.reason}`, { mayHaveExecuted: true }));
          } else {
            failOpen(error);
          }
          return false;
        }
        if (onAck !== undefined) {
          try {
            onAck(ack);
          } catch (error) {
            failOpen(error);
            return false;
          }
        }
        if (openSettled)
          return false;
        openSettled = true;
        phase = "live";
        clearTimeout(timer);
        const subscription = {
          ack,
          closed,
          close: () => {
            finishClosed({ reason: "explicit", resyncRequired: false });
          }
        };
        resolve(subscription);
        return true;
      }
    });
  }
}
function assertTimeoutMs2(value) {
  if (!Number.isInteger(value) || value <= 0 || value > MAX_UHP_TIMEOUT_MS) {
    throw new RangeError(`uhp timeoutMs must be an integer within [1, ${MAX_UHP_TIMEOUT_MS}]; received ${String(value)}`);
  }
}
function assertMaxFrameBytes(value) {
  if (!Number.isInteger(value) || value < 1 || value > MAX_FRAME_BYTES) {
    throw new RangeError(`uhp maxFrameBytes must be an integer within [1, ${MAX_FRAME_BYTES}]; received ${String(value)}`);
  }
}

// src/transport/uhp/router.ts
var UHP_METHOD_BIND = "pane.report_session";
var UHP_METHOD_REPORT = "agent.report";
var UHP_METHOD_HEARTBEAT = "task.heartbeat";
var UHP_METHOD_RELEASE = "agent.release";
function abortError3(signal) {
  const reason = signal.reason;
  if (reason instanceof Error)
    return reason;
  return new Error("operation aborted before start");
}

class UhpTelemetryRouter {
  uhp;
  cli;
  probeCapabilities;
  backoff;
  applyCapabilities;
  cache = new UhpCapabilityCache;
  probeShared;
  uncertainOutcome = false;
  constructor(deps) {
    this.uhp = deps.uhp;
    this.cli = deps.cli;
    this.probeCapabilities = deps.probe;
    this.backoff = deps.backoff ?? new UhpBackoff;
    this.applyCapabilities = deps.applyCapabilities;
  }
  markReconciled(serverGeneration) {
    const cached = this.cache.get();
    if (cached === undefined)
      return false;
    if (cached.serverGeneration !== serverGeneration)
      return false;
    this.uncertainOutcome = false;
    return true;
  }
  health() {
    const snapshot = this.backoff.snapshot();
    return {
      hasCapabilities: this.cache.get() !== undefined,
      coolingDown: !snapshot.canProbe,
      probeFailures: snapshot.failures,
      outcomeUncertain: this.uncertainOutcome
    };
  }
  healthSnapshot() {
    const cached = this.cache.get();
    return {
      ...this.health(),
      supportedMethods: cached === undefined ? [] : [...cached.methods]
    };
  }
  cachedCapabilities() {
    return this.cache.get();
  }
  async forceReconnect(options) {
    if (options?.signal?.aborted === true) {
      throw abortError3(options.signal);
    }
    this.backoff.force();
    return this.refreshForCaller(options?.signal);
  }
  async bindSession(sessionId, options) {
    return this.execute(UHP_METHOD_BIND, (client) => client.bindSession(sessionId, options), options?.signal);
  }
  async reportAgent(report, options) {
    return this.execute(UHP_METHOD_REPORT, (client) => client.reportAgent(report, options), options?.signal);
  }
  async taskHeartbeat(taskId, ratio, options) {
    return this.execute(UHP_METHOD_HEARTBEAT, (client) => client.taskHeartbeat(taskId, ratio, options), options?.signal);
  }
  async releaseAgent(options) {
    return this.execute(UHP_METHOD_RELEASE, (client) => client.releaseAgent(options), options?.signal);
  }
  async execute(method, run, signal) {
    if (signal?.aborted === true) {
      throw abortError3(signal);
    }
    const cached = this.cache.get();
    if (cached !== undefined) {
      if (cached.supports(method)) {
        return this.callUhp(run);
      }
      return run(this.cli);
    }
    const probed = await this.probeForCaller(signal);
    if (probed !== undefined && probed.supports(method)) {
      return this.callUhp(run);
    }
    return run(this.cli);
  }
  async callUhp(run) {
    try {
      return await run(this.uhp);
    } catch (error) {
      if (error instanceof UhpTransportError && error.stage === "aborted") {
        throw error;
      }
      if (isUhpRemoteError(error)) {
        throw error;
      }
      if (error instanceof UhpTransportError && !error.mayHaveExecuted) {
        this.cache.invalidate();
        this.backoff.recordFailure();
        return run(this.cli);
      }
      this.cache.invalidate();
      this.backoff.recordFailure();
      this.uncertainOutcome = true;
      throw error;
    }
  }
  async probeForCaller(signal) {
    const cached = this.cache.get();
    if (cached !== undefined)
      return cached;
    return this.sharedProbe(signal);
  }
  async refreshForCaller(signal) {
    return this.sharedProbe(signal);
  }
  async sharedProbe(signal) {
    if (!this.backoff.canProbe())
      return;
    if (this.probeShared === undefined) {
      const running = this.runProbe();
      this.probeShared = running;
      running.then(() => {
        if (this.probeShared === running)
          this.probeShared = undefined;
      }, () => {
        if (this.probeShared === running)
          this.probeShared = undefined;
      });
    }
    const shared = this.probeShared;
    if (signal === undefined) {
      const outcome = await shared;
      return outcome.ok ? outcome.caps : undefined;
    }
    let onAbort;
    try {
      const outcome = await Promise.race([
        shared,
        new Promise((_resolve, reject) => {
          if (signal.aborted) {
            reject(abortError3(signal));
            return;
          }
          onAbort = () => reject(abortError3(signal));
          signal.addEventListener("abort", onAbort, { once: true });
        })
      ]);
      return outcome.ok ? outcome.caps : undefined;
    } finally {
      if (onAbort !== undefined) {
        signal.removeEventListener("abort", onAbort);
      }
    }
  }
  async runProbe() {
    let caps;
    try {
      caps = await this.probeCapabilities();
    } catch {
      this.cache.invalidate();
      this.backoff.recordFailure();
      return { ok: false };
    }
    try {
      this.cache.update(caps);
    } catch {
      this.backoff.recordFailure();
      return { ok: false };
    }
    const apply = this.applyCapabilities;
    if (apply !== undefined) {
      try {
        apply(caps);
      } catch {
        this.cache.invalidate();
        this.backoff.recordFailure();
        return { ok: false };
      }
    }
    this.backoff.recordSuccess();
    return { ok: true, caps };
  }
}

// src/ui/status.ts
var LUVUS_STATUS_KEY = "luvus";
function formatStatus(status) {
  if (status.kind === "clear")
    return;
  if (status.kind === "unavailable")
    return "\u25CB Luvus \xB7 unavailable";
  const transport = status.transport === undefined ? "" : ` \xB7 ${status.transport.toUpperCase()}`;
  const base = `\u25CF Luvus \xB7 ${status.state}${transport}`;
  return status.taskId === undefined ? base : `${base} \xB7 ORCH ${status.taskId}`;
}
function applyStatus(ui, status) {
  ui.setStatus(LUVUS_STATUS_KEY, formatStatus(status));
}

// src/ui/widget.ts
var FLEET_WIDGET_KEY = "luvus-fleet";
var MAX_WIDGET_AGENTS = 7;
var MAX_WIDGET_LINE_CHARS = 80;
function truncateLine(line) {
  const points = [...line];
  if (points.length <= MAX_WIDGET_LINE_CHARS)
    return line;
  return `${points.slice(0, MAX_WIDGET_LINE_CHARS - 1).join("")}\u2026`;
}
function agentRow(entry) {
  const base = `${entry.paneId} ${entry.agent} ${entry.status} ${entry.authority}`;
  return entry.focused ? `${base} [focused]` : base;
}
function buildFleetWidgetLines(snapshot, offline) {
  if (offline)
    return ["Luvus fleet: offline"];
  const lines = ["Luvus fleet"];
  const entries = Array.isArray(snapshot.entries) ? snapshot.entries : [];
  if (entries.length === 0) {
    lines.push("no agents cached");
    return lines.map(truncateLine);
  }
  const shown = entries.slice(0, MAX_WIDGET_AGENTS);
  for (const entry of shown) {
    if (entry === null || typeof entry !== "object" || typeof entry.paneId !== "string") {
      continue;
    }
    const row = entry;
    lines.push(agentRow({
      paneId: row.paneId,
      agent: typeof row.agent === "string" ? row.agent : "?",
      status: typeof row.status === "string" ? row.status : "?",
      authority: typeof row.authority === "string" ? row.authority : "?",
      focused: row.focused === true
    }));
  }
  const hidden = entries.length - shown.length;
  if (hidden > 0)
    lines.push(`+${hidden} more`);
  return lines.map(truncateLine);
}
function syncFleetWidget(ui, snapshot, options) {
  if (!options.enabled)
    return;
  try {
    ui.setWidget(FLEET_WIDGET_KEY, buildFleetWidgetLines(snapshot, options.offline));
  } catch {}
}
function clearFleetWidget(ui) {
  try {
    ui.setWidget(FLEET_WIDGET_KEY, undefined);
  } catch {}
}

// src/index.ts
var FALLBACK_BLOCKED_MESSAGE = "waiting for user input";
function firstAskQuestion(args) {
  if (typeof args !== "object" || args === null)
    return;
  const questions = args.questions;
  if (!Array.isArray(questions))
    return;
  for (const entry of questions) {
    if (typeof entry !== "object" || entry === null)
      continue;
    const question = entry.question;
    if (typeof question === "string" && question.length > 0)
      return question;
  }
  return;
}
function uiPromptMessage(event) {
  if (event.title !== undefined && event.title.length > 0)
    return event.title;
  return event.kind;
}

class BridgeRuntime {
  config;
  lane;
  authority;
  heartbeat;
  coordinator;
  transportLabel;
  policy;
  budgets;
  loops;
  metrics;
  fleetCache;
  orch = new OrchContextTracker;
  state = createInitialState();
  identity;
  blockedMessage;
  shuttingDown = false;
  shutDown = false;
  halted = false;
  available = true;
  started = false;
  eligible = false;
  generation = 0;
  currentUiCtx;
  lastStatusText;
  shutdownPromise;
  constructor(deps) {
    this.config = deps.config;
    this.lane = new TelemetryLane(deps.client);
    this.authority = new StateAuthority({
      pane: deps.config.paneId,
      source: deps.config.source,
      agent: deps.config.agent,
      now: deps.now
    });
    this.heartbeat = new HeartbeatController({
      intervalMs: deps.config.heartbeatSeconds * 1000,
      scheduler: deps.scheduler,
      onBeat: () => this.handleHeartbeatTick()
    });
    this.coordinator = deps.coordinator;
    this.transportLabel = deps.transport;
    this.policy = deps.policy;
    this.budgets = deps.budgets;
    this.loops = deps.loops;
    this.metrics = deps.metrics;
    this.fleetCache = deps.fleetCache;
  }
  async handleSessionStart(event, ctx) {
    if (!ctx.hasUI || this.shuttingDown || this.shutDown)
      return;
    this.eligible = true;
    this.generation += 1;
    const gen = this.generation;
    this.started = false;
    this.state = reduce(this.state, mapSessionStart(event));
    this.authority.resetSession();
    this.orch.reset();
    this.policy?.resetSession();
    this.budgets?.resetSession();
    this.loops?.resetSession();
    this.metrics?.reset();
    this.identity = readSessionIdentity(ctx.sessionManager);
    this.blockedMessage = undefined;
    this.halted = false;
    this.available = true;
    this.lastStatusText = undefined;
    this.currentUiCtx = ctx;
    const sessionId = reportSessionId(this.identity);
    if (sessionId === undefined) {
      await this.emitCurrent(ctx, false);
      if (gen !== this.generation || this.shuttingDown || this.shutDown) {
        return;
      }
      this.heartbeat.start();
      this.started = true;
      this.maybeStartCoordinator();
      return;
    }
    try {
      await this.lane.bind(sessionId);
    } catch (error) {
      this.laneFailed(undefined, error);
    }
    if (gen !== this.generation || this.shuttingDown || this.shutDown)
      return;
    await this.emitCurrent(ctx, false);
    if (gen !== this.generation || this.shuttingDown || this.shutDown)
      return;
    this.heartbeat.start();
    this.started = true;
    this.maybeStartCoordinator();
  }
  maybeStartCoordinator() {
    const coordinator = this.coordinator;
    if (coordinator === undefined)
      return;
    try {
      const started = coordinator.start();
      started.then(undefined, () => {});
    } catch {}
  }
  handleAgentStart(event, ctx) {
    if (!this.acceptEvent(ctx))
      return;
    this.blockedMessage = undefined;
    this.state = reduce(this.state, mapAgentStart(event));
    this.publish(ctx);
  }
  handleAgentSettled(event, ctx) {
    if (!this.acceptEvent(ctx))
      return;
    this.blockedMessage = undefined;
    this.state = reduce(this.state, mapAgentSettled(event));
    this.publish(ctx);
    this.orchCheckpoint(ctx);
  }
  handleUiPromptStart(event, ctx) {
    if (!this.acceptEvent(ctx))
      return;
    this.blockedMessage = uiPromptMessage(event);
    this.state = reduce(this.state, mapUiPromptStart(event));
    this.publish(ctx);
  }
  handleUiPromptEnd(event, ctx) {
    if (!this.acceptEvent(ctx))
      return;
    this.state = reduce(this.state, mapUiPromptEnd(event));
    if (this.state.publicState !== "blocked")
      this.blockedMessage = undefined;
    this.publish(ctx);
  }
  handleToolExecutionStart(event, ctx) {
    const mapped = mapToolExecutionStart(event);
    if (mapped === undefined)
      return;
    if (!this.acceptEvent(ctx))
      return;
    this.blockedMessage = firstAskQuestion(event.args) ?? FALLBACK_BLOCKED_MESSAGE;
    this.state = reduce(this.state, mapped);
    this.publish(ctx);
  }
  handleToolExecutionEnd(event, ctx) {
    const mapped = mapToolExecutionEnd(event);
    if (mapped === undefined)
      return;
    if (!this.acceptEvent(ctx))
      return;
    this.state = reduce(this.state, mapped);
    if (this.state.publicState !== "blocked")
      this.blockedMessage = undefined;
    this.publish(ctx);
  }
  handleTurnEnd(_event, ctx) {
    if (!this.acceptEvent(ctx))
      return;
    this.publish(ctx);
    this.orchCheckpoint(ctx);
  }
  handleSessionShutdown(event, ctx) {
    return this.shutdown(ctx, event);
  }
  shutdown(ctx, event) {
    if (this.shutdownPromise !== undefined)
      return this.shutdownPromise;
    this.shutdownPromise = this.doShutdown(ctx, event);
    return this.shutdownPromise;
  }
  async doShutdown(ctx, event) {
    this.shuttingDown = true;
    this.heartbeat.stop();
    if (this.coordinator !== undefined) {
      try {
        await this.coordinator.stop();
      } catch {}
    }
    this.currentUiCtx = undefined;
    this.state = reduce(this.state, mapSessionShutdown(event));
    if (this.eligible) {
      try {
        await this.lane.drain();
      } catch {}
      try {
        await this.lane.release();
      } catch {}
    }
    this.shutDown = true;
    this.clearStatus(ctx);
  }
  acceptEvent(ctx) {
    if (!ctx.hasUI || !this.started || this.shuttingDown || this.shutDown) {
      return false;
    }
    this.currentUiCtx = ctx;
    return true;
  }
  currentSessionId() {
    return this.identity === undefined ? undefined : reportSessionId(this.identity);
  }
  buildCurrentReport(force) {
    try {
      return this.authority.buildReport({
        state: this.state.publicState,
        message: this.blockedMessage,
        sessionId: this.currentSessionId(),
        force
      });
    } catch {
      this.available = false;
      return;
    }
  }
  publish(ctx) {
    if (!this.acceptEvent(ctx))
      return Promise.resolve();
    return this.emitCurrent(ctx, false);
  }
  emitCurrent(ctx, force) {
    if (this.halted) {
      this.refreshStatus(ctx);
      return Promise.resolve();
    }
    const report = this.buildCurrentReport(force);
    this.refreshStatus(ctx);
    if (report === undefined)
      return Promise.resolve();
    return this.submitReport(report, ctx);
  }
  submitReport(report, ctx) {
    const sequence = report.sequence;
    const gen = this.generation;
    this.metrics?.recordQueueDepth(this.lane.pendingCount);
    const startedAt = Date.now();
    let sent;
    try {
      sent = this.lane.report(report);
    } catch {
      this.available = false;
      this.metrics?.recordTelemetryFailed();
      if (ctx !== undefined)
        this.refreshStatus(ctx);
      return Promise.resolve();
    }
    return sent.then(() => {
      if (gen !== this.generation)
        return;
      this.metrics?.recordTelemetryOk(Date.now() - startedAt);
      this.laneSucceeded();
      if (ctx !== undefined)
        this.refreshStatus(ctx);
    }, (error) => {
      if (gen !== this.generation)
        return;
      this.metrics?.recordTelemetryFailed();
      this.laneFailed(sequence, error);
      if (ctx !== undefined)
        this.refreshStatus(ctx);
    });
  }
  handleHeartbeatTick() {
    if (this.shuttingDown || this.shutDown || this.halted || !this.started || !this.eligible) {
      return Promise.resolve();
    }
    const report = this.buildCurrentReport(true);
    if (report === undefined)
      return Promise.resolve();
    const done = this.submitReport(report, undefined);
    done.then(() => {
      this.metrics?.recordHeartbeatSent();
      this.refreshHeartbeatStatus();
    }, () => {
      this.metrics?.recordHeartbeatFailed();
      this.refreshHeartbeatStatus();
    });
    return done;
  }
  refreshHeartbeatStatus() {
    const ctx = this.currentUiCtx;
    if (ctx === undefined)
      return;
    this.refreshStatus(ctx);
  }
  orchCheckpoint(ctx) {
    if (this.shuttingDown || this.shutDown)
      return;
    const taskId = this.config.taskId;
    if (taskId === undefined)
      return;
    let usage;
    try {
      usage = ctx.getContextUsage();
    } catch {
      return;
    }
    const ratio = this.orch.attempt(usage);
    if (ratio === undefined)
      return;
    const gen = this.generation;
    let sent;
    try {
      sent = this.lane.taskHeartbeat(taskId, ratio);
    } catch {
      this.orch.fail(ratio);
      return;
    }
    sent.then(() => {
      if (gen === this.generation)
        this.orch.ack(ratio);
    }, () => {
      if (gen === this.generation)
        this.orch.fail(ratio);
    });
  }
  laneSucceeded() {
    if (this.shuttingDown || this.shutDown)
      return;
    this.available = true;
  }
  laneFailed(sequence, error) {
    if (this.shuttingDown || this.shutDown)
      return;
    if (sequence !== undefined)
      this.authority.invalidateForRetry(sequence);
    if (classifyReportError(error) === "authority_conflict") {
      this.halted = true;
      this.heartbeat.stop();
    } else {
      this.available = false;
      this.metrics?.recordRetry();
    }
  }
  refreshStatus(ctx) {
    if (!ctx.hasUI || this.shuttingDown || this.shutDown)
      return;
    try {
      this.metrics?.noteCoordinatorStatus(this.coordinatorStatusLike());
    } catch {}
    let transport;
    try {
      transport = this.transportLabel?.();
    } catch {
      transport = undefined;
    }
    const status = this.halted || !this.available ? { kind: "unavailable" } : {
      kind: "state",
      state: this.state.publicState,
      taskId: this.config.taskId,
      ...transport === undefined ? {} : { transport }
    };
    const text = formatStatus(status);
    if (text === this.lastStatusText) {
      this.syncWidget(ctx);
      return;
    }
    this.lastStatusText = text;
    try {
      applyStatus(ctx.ui, status);
    } catch {}
    this.syncWidget(ctx);
  }
  syncWidget(ctx) {
    if (this.config.widget !== true)
      return;
    if (this.fleetCache === undefined)
      return;
    if (!ctx.hasUI || this.shuttingDown || this.shutDown)
      return;
    syncFleetWidget(ctx.ui, this.fleetCache.snapshot(), {
      offline: this.coordinator === undefined,
      enabled: true
    });
  }
  coordinatorStatusLike() {
    try {
      const coordinator = this.coordinator;
      const status = coordinator?.status?.();
      if (status === null || typeof status !== "object")
        return;
      const record = status;
      const generation = typeof record.generation === "string" ? record.generation : undefined;
      const resyncCount = typeof record.resyncCount === "number" ? record.resyncCount : undefined;
      if (generation === undefined && resyncCount === undefined) {
        return;
      }
      return { generation, resyncCount };
    } catch {
      return;
    }
  }
  clearStatus(ctx) {
    if (!ctx.hasUI)
      return;
    try {
      applyStatus(ctx.ui, { kind: "clear" });
    } catch {}
    if (this.config.widget === true)
      clearFleetWidget(ctx.ui);
    this.lastStatusText = undefined;
  }
}
var disconnectedRequester = {
  request: () => Promise.reject(new Error("uhp disconnected"))
};
function createTransports(config, exec) {
  const cli = new LuvusCliClient({
    pane: config.paneId,
    source: config.source,
    agent: config.agent,
    exec
  });
  const fleetCache = new FleetCache;
  const metrics = new MetricsCollector;
  const policy = new DefaultPolicyEngine;
  const budgets = new DefaultBudgetTracker;
  const loops = new LoopDetector;
  const selfGuard = new SelfDelegationGuard(config.paneId);
  const uhpEndpoint = config.apiAddress ?? config.socketPath ?? "";
  if (uhpEndpoint.length === 0) {
    return {
      client: cli,
      coordinator: undefined,
      router: undefined,
      fleetCache,
      uhpInspector: new UhpInspector({
        requester: disconnectedRequester,
        endpoint: "cli-only",
        supportedMethods: []
      }),
      opsRouter: undefined,
      policy,
      budgets,
      loops,
      metrics
    };
  }
  try {
    const base = new OneShotRequester;
    const adaptive = new AdaptiveUhpRequester(base);
    const uhp = new UhpClient({
      pane: config.paneId,
      source: config.source,
      agent: config.agent,
      socketPath: uhpEndpoint,
      requester: adaptive
    });
    const router = new UhpTelemetryRouter({
      uhp,
      cli,
      probe: () => fetchUhpCapabilities(adaptive, uhpEndpoint),
      applyCapabilities: (caps) => {
        adaptive.adaptToAdvertised(caps.limits.frameBytes);
      }
    });
    const coordinator = new UhpEventCoordinator({
      endpoint: uhpEndpoint,
      eventClient: new UhpEventStreamClient,
      requester: adaptive,
      capabilityCache: new UhpCapabilityCache,
      applySnapshot: fleetCache.onSnapshot,
      applyEvent: fleetCache.onEvent
    });
    const uhpInspector = new UhpInspector({
      requester: adaptive,
      endpoint: uhpEndpoint,
      supportedMethods: () => router.healthSnapshot().supportedMethods
    });
    const opsRouter = new OpsRouter({
      uhp: new UhpOpsClient({ requester: adaptive, endpoint: uhpEndpoint }),
      cli: new CliOpsClient({
        exec,
        ...config.binPath === undefined ? {} : { bin: config.binPath }
      }),
      telemetry: router,
      policy,
      budget: budgets,
      guards: { self: selfGuard, loops },
      ownPaneId: config.paneId
    });
    return {
      client: router,
      coordinator,
      router,
      fleetCache,
      uhpInspector,
      opsRouter,
      policy,
      budgets,
      loops,
      metrics
    };
  } catch {
    return {
      client: cli,
      coordinator: undefined,
      router: undefined,
      fleetCache,
      uhpInspector: new UhpInspector({
        requester: disconnectedRequester,
        endpoint: "cli-only",
        supportedMethods: []
      }),
      opsRouter: undefined,
      policy,
      budgets,
      loops,
      metrics
    };
  }
}
function createLuvusExtension(pi) {
  const config = loadLuvusConfig();
  if (!config.enabled)
    return;
  const exec = (command, args, options) => pi.exec(command, args, options);
  const transports = createTransports(config, exec);
  try {
    registerDelegationEntryRenderer(pi);
  } catch {}
  const runtime = new BridgeRuntime({
    config,
    client: transports.client,
    coordinator: transports.coordinator,
    metrics: config.metricsEnabled === false ? undefined : transports.metrics,
    fleetCache: transports.fleetCache,
    transport: () => transports.router === undefined ? "cli" : transports.router.healthSnapshot().hasCapabilities ? "uhp" : "cli",
    policy: transports.policy,
    budgets: transports.budgets,
    loops: transports.loops
  });
  const routerForTools = transports.router ?? new UhpTelemetryRouter({
    uhp: transports.client,
    cli: transports.client,
    probe: () => Promise.reject(new Error("uhp disconnected"))
  });
  registerDiscoverTool(pi, { router: routerForTools });
  registerInspectTool(pi, buildInspectDeps({
    fleetCache: transports.fleetCache,
    router: routerForTools,
    coordinator: transports.coordinator,
    uhpInspector: transports.uhpInspector,
    exec,
    binPath: config.binPath,
    metrics: config.metricsEnabled === false ? undefined : transports.metrics
  }));
  registerDelegateTool(pi, {
    ops: transports.opsRouter,
    maxWaitMs: transports.budgets.maxWaitMs()
  });
  registerTaskTool(pi, { ops: transports.opsRouter });
  registerPhase4Commands(pi, {
    policy: transports.policy,
    ops: transports.opsRouter,
    telemetry: transports.router,
    config
  });
  registerInspectionCommands(pi, {
    fleetCache: transports.fleetCache,
    router: transports.router,
    coordinator: transports.coordinator,
    config,
    metrics: config.metricsEnabled === false ? undefined : transports.metrics
  });
  pi.on("session_start", (event, ctx) => {
    if (ctx.hasUI) {
      try {
        additiveSetActiveTools(pi, [DISCOVER_TOOL_NAME]);
      } catch {}
    }
    return runtime.handleSessionStart(event, ctx);
  });
  pi.on("agent_start", (event, ctx) => runtime.handleAgentStart(event, ctx));
  pi.on("agent_settled", (event, ctx) => runtime.handleAgentSettled(event, ctx));
  pi.on("ui_prompt_start", (event, ctx) => runtime.handleUiPromptStart(event, ctx));
  pi.on("ui_prompt_end", (event, ctx) => runtime.handleUiPromptEnd(event, ctx));
  pi.on("tool_execution_start", (event, ctx) => runtime.handleToolExecutionStart(event, ctx));
  pi.on("tool_execution_end", (event, ctx) => runtime.handleToolExecutionEnd(event, ctx));
  pi.on("turn_end", (event, ctx) => runtime.handleTurnEnd(event, ctx));
  pi.on("session_shutdown", (event, ctx) => runtime.handleSessionShutdown(event, ctx));
}
export {
  BridgeRuntime,
  createLuvusExtension as default
};
