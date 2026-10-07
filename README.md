# Varynth

Varynth local coding workspace with a TUI, secure tools, MCP, goals and an operator dashboard.

Varynth runs as a single Rust binary. It uses the model you configure, stores sessions locally, and reads or edits files through the workspace policy.

The default screen is a full-screen terminal UI. A plain line prompt, one-shot `exec`, a local dashboard, and scheduled tasks are the same agent.

## Install

Windows is the currently verified release platform. You need a current Rust toolchain (`cargo`).

Install the published package:

```powershell
cargo install varynth --locked
```

Or build from the GitHub repository:

```powershell
git clone https://github.com/ZuziaDev/varynth.git
cd varynth
cargo build --release
```

The binary is `target\release\varynth.exe`. To copy it onto your PATH:

```powershell
varynth install
varynth install --startup   # also register `varynth serve` at Windows logon
```

`install` copies the executable to `~/.local/bin`.

## First run

```powershell
varynth init
varynth doctor
varynth
```

`init` writes `~/.varynth/config.toml` if it is missing. `doctor` checks that the selected provider has credentials and that the endpoint answers. A model call does not start until that check passes. `init`, `doctor`, `sessions`, and `task list` stay local.

## Screen

`varynth` opens the full-screen UI. `varynth --plain`, or any non-terminal stdout, falls back to a line prompt.

| Key | Action |
| --- | --- |
| Enter | Send the draft. While a turn is running, Enter queues the next one. |
| Shift+Enter | New line in the draft |
| Alt+V | Attach the image on the clipboard |
| Left, draft empty | Previous sessions |
| Down, draft empty | Shells, subagents, and scheduled tasks |
| Up | Earlier drafts |
| Shift+Tab | Cycle permission mode |
| Esc, Ctrl+C, Ctrl+D | Leave |

Slash commands handled in the UI, without a model call:

```text
/help  /commands
/goal <condition>     keep going until an independent auditor confirms the condition is met
/goal --rounds N <c>  cap the goal loop at N rounds; --minutes M caps wall time instead
/goal status          condition, round N/M, elapsed time, last auditor verdict
/goal clear           stop the current goal
/new                  archive the active session and start a fresh one
/clear                clear current context without a model call
/rename <title>       persist a session title
/context              show a component/token budget breakdown
/config               edit theme, model, permission, sandbox and keyring-backed secrets
/mcp                  inspect configured MCP servers and tools
/plugin list|search|install|remove  manage curated integrations
/effort               low, medium, high, xhigh, max, ultra
/effort <level>       set the effort directly, no menu
/permission           choose acceptEdits, prompt or bypass
/sandbox              choose read-only, workspace-write, danger-full-access or docker-isolated
/status  /skills  /models  /model
/model                pick from a select menu: arrows to move, Enter to choose, Esc to close
/model <id>           set the model directly, no menu
/compact              summarize older turns in place; also runs automatically when the session grows long
/diff                 show what changed in the working copy
/review               ask the model to review the working-copy diff
/cost                 tokens and time for this session
/doctor               same checks as `varynth doctor`
/resume [id]          open a saved session
/agents name: prompt  one background subagent, reply lands in the chat
/send <id|latest> <text>  message another session on the local bus
/inbox                read and clear messages other sessions sent to this one
/undo                 revert the last write_file/edit_file this session made
/skills-search [q]    search/install skills.sh skills without restarting
/quit  /exit
```

Type `/` and the composer lists these plus any installed skill. Arrow keys move, Tab or Enter completes, Esc closes the list.

In `--plain` the local commands are `/status`, `/models`, `/skills`, `/remind <seconds> <message>`, `/serve`, and `/quit`. A skill is still `/name`.

## Commands

```text
varynth                         full-screen UI
varynth --plain                 line prompt
varynth -C <dir>                working directory
varynth exec "fix the tests"    one shot, print the reply
varynth --mcp-config mcp.json exec "use the configured MCP tools"
varynth resume [id]
varynth sessions
varynth models
varynth doctor
varynth doctor --fix            try to start the configured local proxy
varynth install [--startup]
varynth serve [--host HOST] [--port PORT]
varynth init
varynth channels
varynth task add "name" "prompt" --at 2030-01-01T09:00:00Z
varynth task add "name" "prompt" --every-seconds 3600
varynth task list
varynth task run <id>
varynth task disable <id>
varynth task enable <id>
varynth task remove <id>
```

Global flags: `--model`, `--provider`, `--permission-mode`, `--sandbox`, `--mcp-config`.

## Config

`~/.varynth/config.toml`. Pick one provider and put the secret in the file or in the matching environment variable. Do not commit either.

OpenAI-compatible endpoint:

```toml
model = "your-model-id"
provider = "openai"                 # openai | anthropic | proxy
openai_base_url = "https://api.example.com"   # no trailing /v1
openai_api_key = ""                 # or OPENAI_API_KEY

permission_mode = "acceptEdits"     # acceptEdits | prompt | bypass
sandbox = "workspace-write"         # read-only | workspace-write | danger-full-access | docker-isolated
effort = "xhigh"                    # low | medium | high | xhigh | max | ultra
max_tokens = 8192                   # optional: provider max output tokens
temperature = 0.2                   # optional: sampling temperature
stream = true                       # optional: set false to disable SSE streaming
theme = "purple"                   # purple | cyberpunk | dark | minimal
goal_max_rounds = 25                # optional: cap for /goal loops
goal_max_minutes = 30               # optional: wall-time cap for /goal
docker_image = "varynth-sandbox:latest"  # optional Docker image
docker_network = "none"            # none | bridge | host
use_keyring = true                  # store provider/bot/dashboard secrets in OS keyring
dashboard_host = "127.0.0.1"
dashboard_port = 7420
```

In `prompt` mode the TUI pops an allow dialog for every gated write and shell call: `o` once, `a` always for this session, `d` deny. A connected gateway can also answer a pending approval; the first answer wins. Non-interactive runs with no attached approver deny gated tools. Desktop control never auto-approves in `acceptEdits`: it requires explicit opt-in and `prompt` approval or `bypass`.

`docker-isolated` runs shell commands inside Docker with the workspace mounted at `/work` and network disabled by default. File tools still access the host workspace through the jail; configured MCP servers are external processes/services, not automatically containerized. `workspace-write` is a file boundary plus shell policy, not an operating-system process sandbox. Use Docker or a disposable virtual machine for untrusted commands.

Use `varynth keyring set <field>` to store a hidden-input secret. Supported fields are `proxy-token`, `openai-api-key`, `anthropic-api-key`, `telegram-bot-token` and `dashboard-token`. When `use_keyring = true`, these fields are omitted from saved TOML. `varynth keyring status` reports presence without printing values.

Values outside these lists stop startup with an error instead of silently falling back. `effort` maps to real provider parameters: Anthropic gets extended thinking with a budget scaled by level, OpenAI-compatible endpoints get `reasoning_effort`. HTTP 429/5xx provider responses are retried with backoff (Retry-After is honored).

Anthropic uses `ANTHROPIC_API_KEY` or `ANTHROPIC_AUTH_TOKEN`. A local proxy uses `proxy_url` (for example `http://127.0.0.1:8787`) and `proxy_token`, or `VARYNTH_PROXY_URL` and `VARYNTH_PROXY_TOKEN`. `doctor --fix` starts `varynth-proxy` from PATH when the configured `proxy_url` is a loopback address. It does not download one.

Other environment overrides: `VARYNTH_MODEL`, `VARYNTH_PROVIDER`, `VARYNTH_DASHBOARD_TOKEN`, `VARYNTH_MCP_CONFIG`.

`permission_mode` controls writes. `sandbox` controls how far shell and file tools may reach. `workspace-write` is the default and stays inside the working directory.

## MCP

`--mcp-config <file>` loads one JSON file for that command. `VARYNTH_MCP_CONFIG` loads it for every command. HTTP, streamable HTTP, and stdio (spawned local server) endpoints are supported. Tools from these servers still pass through the permission mode and the sandbox.

```json
{
  "mcpServers": {
    "local-tools": {
      "type": "http",
      "url": "http://127.0.0.1:9000/mcp",
      "headers": { "Authorization": "Bearer <token>" }
    },
    "filesystem": {
      "type": "stdio",
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "C:/some/dir"]
    }
  }
}
```

A server entry with a `command` and no `type` is treated as stdio. Stdio children are spawned on first use, killed on exit, and restarted automatically if they die.

Model replies stream token by token (SSE) into the TUI and the dashboard when the provider supports it; set `stream = false` to turn that off.

## Sessions, skills, tasks

Sessions are stored under `~/.varynth`. `resume` without an id opens the latest one.

Project instructions are read from `VARYNTH.md`, `AGENTS.md`, or `CLAUDE.md` in the working directory. Skills are a `SKILL.md` under `.varynth/skills`, `.claude/skills`, or `~/.varynth/skills`, and are called with `/name`.

Agent files (`SOUL.md`, `USER.md`, `MEMORY.md`, `DREAM.md`, `HEART.md`) are created in `~/.varynth` and in `<project>/.varynth` only when missing. Project files layer over the global ones and are passed to the model as untrusted context. Memory appends stay inside the project `MEMORY.md`.

Tasks live in `~/.varynth/tasks.json`. `--at` takes an RFC3339 timestamp. `--every-seconds` repeats. `varynth serve` checks for due tasks every 15 seconds. Each run opens a session and calls the configured model. Background runs cannot use the shell or write files unless that task was created with `--allow-background-tools`.

Built-in tools: `read_file`, `write_file`, `edit_file`, `glob_search`, `grep_search`, `bash`, `web_fetch` (http and https only; loopback, private, link-local, and cloud-metadata addresses are blocked, including across redirects), `memory_read`, `memory_append`, `session_send`, and `session_inbox`.

## Goals and session-to-session mail

`/goal <condition>` turns the agent into a supervised loop. After every turn an independent auditor model call judges the transcript against the condition and answers `{"met": bool, "reason": str}`; the loop continues on a negative verdict and stops the moment the auditor confirms the objective (a model `GOAL_COMPLETE` claim alone is not enough). `--rounds N` / `--minutes M` or `goal_max_rounds` / `goal_max_minutes` cap the loop; the header shows `goal N/M`.

Sessions talk over a local bus at `~/.varynth/bus.jsonl` (lock-protected, pruned after a day). The `session_send` and `session_inbox` tools let a running agent exchange mail with other sessions; `/send <id|latest> <text>` and `/inbox` do the same from the keyboard. Mail waiting for a session is injected into its next turn automatically. The dashboard can post mail too: `POST /api/session/{id}/messages` with `{"text": "..."}`, and session listings carry an `unread` count.

Every `write_file` and `edit_file` first checkpoints the file's previous content under `~/.varynth/checkpoints` (per session, last 100, files over 2 MB are skipped). `/undo` reverts the most recent write — repeat it to walk backwards.

## Dashboard and Telegram

```powershell
varynth serve
```

Open `http://127.0.0.1:7420`. Control Room has a searchable session rail, live conversation streaming, activity and approval tabs, model selection, skills, health checks and a runtime inspector. On narrow screens the session rail and inspector open as dismissible side panels. The Connection dialog keeps its dashboard token only in page memory and applies it to HTTP, SSE and WebSocket requests. Requests with a foreign `Host` or `Origin` are rejected. For a non-loopback bind, set `VARYNTH_DASHBOARD_TOKEN`.

Telegram needs its own bot token from BotFather, not a token shared with another app. Put it in `~/.varynth/telegram.env` as `TELEGRAM_BOT_TOKEN=...`, or set `VARYNTH_TELEGRAM_BOT_TOKEN`, then run `varynth serve`. The first direct message tells you your numeric user id. Add that id to `telegram_allow_from` and restart serve.

```toml
telegram_allow_from = ["123456789"]
```

Discord and WhatsApp adapters are not in this binary.

## Computer Use

Desktop tools are opt-in: set `VARYNTH_COMPUTER_USE=1` in the Varynth process environment, then use `prompt` for interactive decisions or explicitly select `bypass`. They provide screenshot capture with a coordinate grid, mouse click/drag, keyboard text/key combinations and Windows window list/focus/resize. `computer_stop` remains available without opt-in. The emergency stop is the pointer corner `(0,0)` or Esc / Ctrl+Alt+Q. Docker mode denies desktop access; read-only mode denies desktop input. Screenshots are written under the workspace jail, not arbitrary paths.

The implementation's parsers, jail checks and failsafe paths are covered by offline tests. Live desktop interactions are not run by the project test suite.

## Gateway

The dashboard exposes `/ws/gateway` with JSON-RPC 2.0 methods: `ping`, `status.get`, `sessions.list`, `session.get`, `session.message`, `session.pause`, `session.resume`, `session.context`, `models.list`, `model.set`, and `approval.respond`. Browser connections pass the dashboard token as `?token=...`; non-browser clients may use a Bearer header. Foreign Origin requests are rejected as well as invalid Host headers.

Terminal events are forwarded across local processes through `~/.varynth/control`. `session.pause` stops at the next model/command boundary, `session.resume` continues, and `session.context` queues a bounded text-only context replacement for the next turn. Remote approval ids can also be answered by an allowlisted Telegram user with `approve <id> once|always|deny`. The `/diff` page renders pasted unified diffs side by side.

## Node.js

`bindings/node` is an ESM package. `VarynthClient` calls the CLI. `AgentWorkspace` reads the layered agent files. Set `VARYNTH_BINARY` when the executable is not inside the repository.

```powershell
cd bindings\node
npm install
npm run build
```
