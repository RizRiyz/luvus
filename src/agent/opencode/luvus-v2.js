// OpenCode V2 CLI integration. The CLI owns the selected pane/session; the
// shared OpenCode service must never claim ownership for another TUI.
import net from "node:net"
import { createEffect } from "solid-js"

const MAX_REPLY_BYTES = 64 * 1024
const REQUEST_TIMEOUT_MS = 500
const INITIAL_RETRY_MS = 100
const MAX_RETRY_MS = 2000

function samePath(left, right) {
  if (typeof left !== "string" || typeof right !== "string") return false
  const clean = (value) => {
    const path = value.replaceAll("\\", "/").replace(/\/+$/, "")
    return process.platform === "win32" ? path.toLowerCase() : path
  }
  return clean(left) === clean(right)
}

export function reportFor(context) {
  const route = context.ui.router.current()
  if (route?.type !== "session" || !route.sessionID) return
  const info = context.data.session.get(route.sessionID)
  if (!info || info.id !== route.sessionID || info.parentID) return
  const location = context.location ?? context.data.location.default()
  if (!samePath(info.directory, location?.directory)) return
  return { pane: process.env.LUVUS_PANE_ID, agent: "opencode", session_id: info.id }
}

function fingerprint(operation) {
  return `${operation.method}:${JSON.stringify(operation.params)}`
}

function sameSession(left, right) {
  return left?.pane === right?.pane
    && left?.agent === right?.agent
    && left?.session_id === right?.session_id
}

// Export the small delivery state machine so transient-failure and navigation
// races can be tested without opening a socket or starting OpenCode.
export function createReporter(
  readReport,
  request,
  scheduleRetry = (callback, delay) => setTimeout(callback, delay),
  cancelRetry = (timer) => clearTimeout(timer),
) {
  let disposed = false
  let pending
  let current
  let confirmed
  let retryTimer
  let retryDelay = INITIAL_RETRY_MS

  const clearRetry = () => {
    if (retryTimer !== undefined) cancelRetry(retryTimer)
    retryTimer = undefined
  }

  const queue = (operation) => {
    operation.fingerprint = fingerprint(operation)
    if (pending?.fingerprint === operation.fingerprint) return
    pending = operation
    retryDelay = INITIAL_RETRY_MS
    clearRetry()
    send()
  }

  const publish = () => {
    if (disposed) return
    const params = readReport()
    if (params) {
      if (!current && sameSession(confirmed, params)) {
        pending = undefined
        clearRetry()
        return
      }
      queue({ method: "pane.report_session", params })
      return
    }

    // A report already in flight may still win after navigation. Wait for its
    // response, then release exactly the identity that actually committed.
    if (current?.method === "pane.report_session") {
      pending = undefined
      clearRetry()
      return
    }
    if (confirmed) {
      queue({ method: "pane.release_session", params: confirmed })
    } else {
      pending = undefined
      clearRetry()
    }
  }

  const retry = () => {
    if (disposed || retryTimer !== undefined || !pending) return
    const delay = retryDelay
    retryDelay = Math.min(retryDelay * 2, MAX_RETRY_MS)
    retryTimer = scheduleRetry(() => {
      retryTimer = undefined
      send()
    }, delay)
  }

  function send() {
    if (disposed || current || !pending) return
    const operation = pending
    current = operation
    void request(operation).then((ok) => {
      if (disposed || current !== operation) return
      current = undefined
      if (ok) {
        retryDelay = INITIAL_RETRY_MS
        if (operation.method === "pane.report_session") {
          confirmed = operation.params
        } else if (sameSession(confirmed, operation.params)) {
          confirmed = undefined
        }
        if (pending?.fingerprint === operation.fingerprint) pending = undefined
      }

      // Re-read the route after every response. This turns a report that won
      // just after navigation into an exact release, and lets newer selection
      // B replace a failed report for selection A without retrying stale A.
      publish()
      if (!pending) return
      if (ok || pending.fingerprint !== operation.fingerprint) send()
      else retry()
    })
  }

  return {
    publish,
    dispose() {
      disposed = true
      pending = undefined
      clearRetry()
    },
  }
}

function sendRequest(address, id, operation) {
  return new Promise((resolve) => {
    let complete = false
    let bytes = 0
    let response = ""
    const socket = net.createConnection(address)
    const finish = (ok) => {
      if (complete) return
      complete = true
      clearTimeout(timeout)
      socket.destroy()
      resolve(ok)
    }
    const timeout = setTimeout(() => finish(false), REQUEST_TIMEOUT_MS)
    socket.setTimeout(REQUEST_TIMEOUT_MS, () => finish(false))
    socket.on("connect", () => {
      socket.write(`${JSON.stringify({ id, method: operation.method, params: operation.params })}\n`)
    })
    socket.on("data", (chunk) => {
      bytes += chunk.length
      if (bytes > MAX_REPLY_BYTES) return finish(false)
      response += chunk.toString("utf8")
      const newline = response.indexOf("\n")
      if (newline < 0) return
      try {
        const reply = JSON.parse(response.slice(0, newline))
        finish(reply.id === id && !reply.error && reply.result)
      } catch {
        finish(false)
      }
    })
    socket.on("error", () => finish(false))
    socket.on("end", () => finish(false))
  })
}

export default {
  id: "luvus.session.v2",
  setup(context) {
    const address = process.env.LUVUS_API_ADDRESS || process.env.LUVUS_SOCKET_PATH
    if (process.env.LUVUS_ENV !== "1" || !address || !process.env.LUVUS_PANE_ID) return
    let sequence = 0
    const reporter = createReporter(
      () => reportFor(context),
      (operation) => sendRequest(address, `luvus-v2-${++sequence}`, operation),
    )

    // Reactive route/session reads handle navigation without an idle poll.
    const removeSlot = context.ui.slot({
      append: "app",
      render: () => {
        createEffect(reporter.publish)
        return null
      },
    })
    // A cache refresh can also make an initially cold selected session ready.
    const stop = context.data.on("session.updated", reporter.publish)
    return () => {
      reporter.dispose()
      stop()
      removeSlot()
    }
  },
}
