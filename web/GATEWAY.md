# Varynth gateway

WebSocket control plane served by the dashboard: one bidirectional
JSON-RPC 2.0 socket at `GET /ws/gateway`. Events flow out, control calls
flow in. No frameworks, no extra dependencies — plain WebSocket + JSON.

## Connecting

- URL: `ws://127.0.0.1:7420/ws/gateway` (default loopback bind).
- Auth: when `VARYNTH_DASHBOARD_TOKEN` is set, the token must arrive as
  `?token=<token>` (browser WebSockets cannot set request headers) or as an
  `Authorization: Bearer <token>` header. Comparison is constant time.
  When the token is unset, any host-allowlisted client may connect — same
  behavior as the dashboard's HTTP API (loopback by default; remote binds
  without a token are refused at startup).
- Missing or wrong token → HTTP 401 before the upgrade.
- The deck (dashboard index page) has a **gateway** panel that connects
  with the token stored in `localStorage` under `varynth_gateway_token`.

## Server → client: events

Every agent event of every turn (dashboard chat, SSE stream, OpenAI
endpoint, Telegram turns) is broadcast as a JSON-RPC notification:

```json
{
  "jsonrpc": "2.0",
  "method": "event",
  "params": {
    "type": "event",
    "session": "<session id>",
    "event": { "kind": "tool", "text": "bash: cargo test" },
    "ts": "2026-10-06T12:34:56.789+00:00"
  }
}
```

- `kind` is one of `tool`, `assistant`, `system`, `error`, `done`,
  `approval_request`, ... (the runtime's `AgentEvent` kinds).
- A client that falls more than 1024 events behind receives one
  `params.type == "lagged"` catch-up note (`missed` count) and the stream
  continues; a closed event stream ends the socket. Slow sockets never
  block producers or other clients — fan-out is a bounded broadcast queue.
- **Approval requests**: when the runtime opens a remote approval slot, the
  frame's `event` additionally carries `id`, `tool` and `detail`, and the
  text ends with an `[id:<uuid>]` token. Answer with `approval.respond`;
  the raw id is shown copyable in the deck so it can also be answered from
  Telegram with `approve <id> once|always|deny`. First answer wins.

## Client → server: requests

Send JSON-RPC request objects (`{"jsonrpc":"2.0","id":1,"method":...,
"params":{...}}`). Responses echo the request `id`.

| method             | params                                   | result |
|--------------------|------------------------------------------|--------|
| `ping`             | —                                        | `"pong"` |
| `status.get`       | —                                        | dashboard status object (with `busy`) |
| `sessions.list`    | —                                        | `{"ok":true,"sessions":[...]}` |
| `session.get`      | `{"id":"<session id>"}`                  | `{"ok":true,"meta":...,"messages":[...],"unread":n}` |
| `session.message`  | `{"id":"<session id>","text":"..."}`     | `{"ok":true,"to":"<id>"}` (delivered over the mailbox bus) |
| `models.list`      | —                                        | `{"ok":true,"models":[...]}` |
| `model.set`        | `{"id":"<model id>"}`                    | `{"ok":true,"model":"<id>"}` |
| `approval.respond` | `{"id":"<relay id>","decision":"once"\|"always"\|"deny"}` | `true` when a pending slot was fed, else `false` |

Errors: `-32601` unknown method, `-32602` missing/invalid params,
`-32603` internal (e.g. no live runtime), `-32700` unparseable JSON.
Unknown fields are ignored; `id` may be a string, number or null.

## Diff viewer

`/diff` (source: `web/diff.html`) renders a unified diff side by side with
line numbers and +/-/context coloring — paste the diff into the box, press
RENDER (or Ctrl+Enter), or pass `?diff=<urlencoded unified diff>`.
No dependencies; accent color `#7E57C2`.
