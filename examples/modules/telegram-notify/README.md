# Telegram Notify

Get a Telegram message when one of your agents needs you (`blocked`) or
finishes (`done`), plus a right-click **Send a test message** action on any
live agent.

Pure `sh` + `curl` (macOS / Linux) -- no dependency beyond the shell. The
whole module is `luvus-module.toml` and `notify.sh`.

## Setup

1. Create a bot with [@BotFather](https://t.me/BotFather) (`/newbot`) and
   copy the token it gives you.
2. Find your chat id: message your new bot once, then open
   `https://api.telegram.org/bot<YOUR-TOKEN>/getUpdates` in a browser and
   read the `"chat":{"id":...}` value from the response. (For a group chat,
   add the bot to the group and use the group's negative id.)
3. Link the module and fill in the settings:

   ```sh
   luvus module link ./examples/modules/telegram-notify
   ```

   **Settings -> Modules -> Telegram Notify**:

   | Setting | Meaning |
   |---|---|
   | Bot token | The BotFather token (stored as a secret; masked in Settings) |
   | Chat ID | Where messages are delivered |
   | Notify on | `blocked` (default), `done`, or `both` -- which agent status changes send a message |

4. Right-click a live agent -> **Send a test message** to confirm the wiring.
   The test action ignores the *Notify on* filter and always sends.

## Behavior

- One plain `sendMessage` per status change. No retries, no queueing, no
  toasts -- the Telegram message is the notification.
- The event message reads `<agent> is <status> in <workspace>`, where
  `<workspace>` is the changed pane's workspace (from the event payload),
  not whichever workspace has focus.
- If the message text contains quotes or backslashes (agent names can), the
  JSON body escapes them, so odd names do not break the request.

## If nothing arrives

Failures -- missing settings, network errors, non-2xx Telegram responses --
are recorded as a single redacted line in `luvus module log`. Check it with:

```sh
luvus module log
```

The bot token and the request URL (which embeds it) are never written to the
log under any outcome. A `curl exit` line means the request never completed;
an `HTTP <code>` line means Telegram answered and rejected it (401 = wrong
token, 400 = usually a wrong chat id).

## Live smoke test (operator)

The success path needs real credentials, so it is run by hand:

1. Start a dev instance in an isolated home -- never your production server:

   ```sh
   env -u LUVUS_SOCKET_PATH -u LUVUS_SESSION LUVUS_HOME="$HOME/.luvus-dev" luvus
   ```

2. In that instance: `luvus module link ./examples/modules/telegram-notify`,
   then enter the real bot token and chat id in Settings -> Modules.
3. Start any agent in a pane and let it go `blocked` or `done` -- a Telegram
   message should arrive. Right-click the agent -> **Send a test message**
   for a second one, regardless of its current status.
4. If either message is missing, `luvus module log` carries the reason.
