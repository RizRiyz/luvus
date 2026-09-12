// OpenCode >=2.0.2 CLI integration. The CLI owns the selected pane/session;
// the shared OpenCode service must never report ownership for another TUI.
import net from "node:net"
import { createEffect } from "solid-js"

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

export default {
  id: "luvus.session.v2",
  setup(context) {
    const address = process.env.LUVUS_API_ADDRESS || process.env.LUVUS_SOCKET_PATH
    if (process.env.LUVUS_ENV !== "1" || !address || !process.env.LUVUS_PANE_ID) return
    let disposed = false
    let pending
    let current
    let last = ""
    let sequence = 0
    const send = () => {
      if (disposed || current || !pending) return
      const params = pending
      pending = undefined
      const fingerprint = JSON.stringify(params)
      const id = `luvus-v2-${++sequence}`
      const socket = net.createConnection(address)
      current = socket
      let response = ""
      let bytes = 0
      let finished = false
      const finish = (ok) => {
        if (finished) return
        finished = true
        clearTimeout(timeout)
        socket.destroy()
        current = undefined
        if (ok) last = fingerprint
        send()
      }
      const timeout = setTimeout(() => finish(false), 500)
      socket.on("connect", () => socket.write(`${JSON.stringify({ id, method: "pane.report_session", params })}\n`))
      socket.on("data", (chunk) => {
        bytes += chunk.length
        if (bytes > 65536) return finish(false)
        response += chunk.toString("utf8")
        const newline = response.indexOf("\n")
        if (newline < 0) return
        try {
          const reply = JSON.parse(response.slice(0, newline))
          finish(reply.id === id && reply.result?.type === "ok")
        } catch { finish(false) }
      })
      socket.on("error", () => finish(false))
      socket.on("end", () => finish(false))
    }
    const publish = () => {
      if (disposed) return
      const params = reportFor(context)
      if (!params) {
        pending = undefined
        last = ""
        return
      }
      if (JSON.stringify(params) === last) return
      pending = params
      send()
    }
    // Reactive route/session reads handle navigation without an idle poll.
    const removeSlot = context.ui.slot({
      append: "app",
      render: () => { createEffect(publish); return null },
    })
    // A cache refresh can also make an initially cold selected session ready.
    const stop = context.data.on("session.updated", publish)
    return () => {
      disposed = true
      pending = undefined
      current?.destroy()
      stop()
      removeSlot()
    }
  },
}
