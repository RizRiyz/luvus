// Luvus OpenCode integration. Reports only the root session selected by this
// TUI and its structured cumulative usage over the inherited owner-local API.
import net from "node:net"

const MAX_REPLY_BYTES = 64 * 1024
const REQUEST_TIMEOUT_MS = 500
const ACTIVE_POLL_MS = 100
const STABLE_POLL_MS = 750
const ACTIVE_WINDOW_MS = 2000

function samePath(left, right) {
  if (typeof left !== "string" || typeof right !== "string") return false
  const clean = (value) => {
    let next = value.replaceAll("\\", "/").replace(/\/+$/, "")
    if (process.platform === "win32") next = next.toLowerCase()
    return next
  }
  return clean(left) === clean(right)
}

function currentRoute(api) {
  const value = api?.route?.current
  return typeof value === "function" ? value.call(api.route) : value
}

function selectedSessionID(api) {
  const route = currentRoute(api)
  if (route?.name !== "session") return undefined
  const id = route?.params?.sessionID
  return typeof id === "string" && id.length > 0 ? id : undefined
}

function safeCounter(value) {
  return Number.isSafeInteger(value) && value >= 0 ? value : undefined
}

function reportFor(api, id) {
  const info = api?.state?.session?.get?.(id)
  if (!info || info.id !== id || info.parentID) return undefined
  if (!samePath(info.directory, api?.state?.path?.directory)) return undefined

  const params = { pane: process.env.LUVUS_PANE_ID, agent: "opencode", session_id: id }
  const tokens = info.tokens
  const input = safeCounter(tokens?.input)
  const output = safeCounter(tokens?.output)
  const cacheRead = safeCounter(tokens?.cache?.read)
  const cacheWrite = safeCounter(tokens?.cache?.write)
  const updatedAt = safeCounter(info?.time?.updated)
  if ([input, output, cacheRead, cacheWrite, updatedAt].every((value) => value !== undefined) && updatedAt > 0) {
    const modelID = typeof info?.model?.id === "string" ? info.model.id : ""
    const providerID = typeof info?.model?.providerID === "string" ? info.model.providerID : ""
    const model = providerID && modelID && !modelID.includes("/") ? `${providerID}/${modelID}` : modelID
    const cost = typeof info.cost === "number" && Number.isFinite(info.cost) && info.cost >= 0 ? info.cost : null
    params.usage = {
      model,
      tokens_in: input,
      tokens_out: output,
      cache_read: cacheRead,
      cache_write: cacheWrite,
      cost,
      updated_at: updatedAt,
    }
  }
  return params
}

function sendRequest(address, id, params) {
  return new Promise((resolve) => {
    let complete = false
    let bytes = 0
    let reply = ""
    const socket = net.createConnection(address)
    const finish = (ok) => {
      if (complete) return
      complete = true
      clearTimeout(timer)
      socket.destroy()
      resolve(ok)
    }
    const timer = setTimeout(() => finish(false), REQUEST_TIMEOUT_MS)
    socket.setTimeout(REQUEST_TIMEOUT_MS, () => finish(false))
    socket.on("connect", () => {
      socket.write(`${JSON.stringify({ id, method: "pane.report_session", params })}\n`)
    })
    socket.on("data", (chunk) => {
      bytes += chunk.length
      if (bytes > MAX_REPLY_BYTES) return finish(false)
      reply += chunk.toString("utf8")
      const newline = reply.indexOf("\n")
      if (newline < 0) return
      try {
        const response = JSON.parse(reply.slice(0, newline))
        finish(response?.id === id && response?.result?.type === "ok")
      } catch {
        finish(false)
      }
    })
    socket.on("error", () => finish(false))
    socket.on("end", () => finish(false))
  })
}

async function tui(api) {
  const address = process.env.LUVUS_API_ADDRESS || process.env.LUVUS_SOCKET_PATH
  const pane = process.env.LUVUS_PANE_ID
  if (process.env.LUVUS_ENV !== "1" || !address || !pane) return

  let disposed = false
  let requestID = 0
  let selected = ""
  let lastFingerprint = ""
  let pending
  let sending = false
  let pollTimer
  let eventTimer
  let activeUntil = Date.now() + ACTIVE_WINDOW_MS

  const drain = async () => {
    if (sending) return
    sending = true
    while (!disposed && pending) {
      const current = pending
      pending = undefined
      const id = `luvus-opencode-${++requestID}`
      if (await sendRequest(address, id, current.params)) lastFingerprint = current.fingerprint
    }
    sending = false
  }

  const enqueue = (params) => {
    const fingerprint = JSON.stringify(params)
    if (fingerprint === lastFingerprint || fingerprint === pending?.fingerprint) return
    // Keep at most the newest unsent report while one request is in flight.
    pending = { params, fingerprint }
    void drain()
  }

  const publishSelected = () => {
    if (disposed) return
    const id = selectedSessionID(api)
    if (!id) return
    const params = reportFor(api, id)
    if (params) enqueue(params)
  }

  const scheduleEvent = (event) => {
    if (disposed || event?.sessionID !== selectedSessionID(api)) return
    clearTimeout(eventTimer)
    eventTimer = setTimeout(publishSelected, 50)
  }

  const poll = () => {
    if (disposed) return
    const next = selectedSessionID(api) || ""
    if (next !== selected) {
      selected = next
      lastFingerprint = ""
      activeUntil = Date.now() + ACTIVE_WINDOW_MS
      publishSelected()
    }
    const delay = Date.now() < activeUntil ? ACTIVE_POLL_MS : STABLE_POLL_MS
    pollTimer = setTimeout(poll, delay)
  }

  const unsubscribeCreated = api?.event?.on?.("session.created", scheduleEvent)
  const unsubscribeUpdated = api?.event?.on?.("session.updated", scheduleEvent)
  api?.lifecycle?.onDispose?.(() => {
    disposed = true
    clearTimeout(pollTimer)
    clearTimeout(eventTimer)
    if (typeof unsubscribeCreated === "function") unsubscribeCreated()
    if (typeof unsubscribeUpdated === "function") unsubscribeUpdated()
  })
  poll()
}

export default { id: "luvus.session", tui }
