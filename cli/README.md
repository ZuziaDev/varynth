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
| Left, draft empty | Open the session picker (after a short delay) |
| Left, Left (draft empty) | Jump to the previous session in place |
| Ctrl+C ×2 | Quit; the first press stops the turn and shows the session id. The id is printed again on exit for `varynth resume <id>` |
| Down, draft empty | Shells, subagents, and scheduled tasks |
| Up | Earlier drafts |
| Shift+Tab | Cycle permission mode |
| Esc | Stop the running turn; when idle, send the draft (empty draft just hints at Ctrl+C) |
| Ctrl+D | Leave |

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
/permission           choose acceptEdits, prompt, bypass or plan
/plan <task>          investigate and propose a plan; writes and mutating commands are refused
/plan approve|deny    execute the approved plan (switches to acceptEdits) or stay in plan mode
/permissions          show the active mode and the loaded permission rules
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
varynth tools [--json]          list the built-in tool schemas
varynth config get [KEY]        print allowlisted non-secret settings (--json)
varynth config set KEY VALUE    validate and persist one setting
varynth config edit             open config.toml in $VISUAL/$EDITOR
varynth doctor
varynth doctor --fix            try to start the configured local proxy
varynth install [--startup]
varynth serve [--host HOST] [--port PORT]
varynth init
varynth trust                   trust this workspace's project-scoped rules
varynth trust --revoke          revoke it
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
provider = "openai"                 # openai | anthropic | proxy | google | google-vertex, or a profile id
openai_base_url = "https://api.example.com"   # no trailing /v1
openai_api_key = ""                 # or OPENAI_API_KEY

permission_mode = "acceptEdits"     # acceptEdits | prompt | bypass | plan
sandbox = "workspace-write"         # read-only | workspace-write | danger-full-access | docker-isolated
effort = "xhigh"                    # low | medium | high | xhigh | max | ultra
max_tokens = 8192                   # optional: provider max output tokens
temperature = 0.2                   # optional: sampling temperature
stream = true                       # optional: set false to disable SSE streaming
theme = "magenta"                  # magenta | system | dark | light | safe | custom:<name>
goal_max_rounds = 25                # optional: cap for /goal loops
goal_max_minutes = 30               # optional: wall-time cap for /goal
docker_image = "varynth-sandbox:latest"  # optional Docker image
docker_network = "none"            # none | bridge | host
use_keyring = true                  # store provider/bot/dashboard secrets in OS keyring
dashboard_host = "127.0.0.1"
dashboard_port = 7420
```

In `prompt` mode the TUI pops an allow dialog for every gated write and shell call: `o` once, `a` always for this session, `d` deny. A connected gateway can also answer a pending approval; the first answer wins. Non-interactive runs with no attached approver deny gated tools. `acceptEdits` auto-approves only `write_file`/`edit_file` inside the workspace; `bash`, MCP tools and `memory_append` still ask. Desktop control never auto-approves in `acceptEdits`: it requires explicit opt-in and `prompt` approval or `bypass`.

`docker-isolated` runs shell commands inside Docker with the workspace mounted at `/work` and network disabled by default. File tools still access the host workspace through the jail; configured MCP servers are external processes/services, not automatically containerized.

On Windows, `workspace-write` and `read-only` are a **write** boundary, not a confidentiality or network boundary: `bash` children run under a restricted Low-integrity token inside a kill-on-close Job Object. `workspace-write` labels the workspace and the sandbox temp directory Low so the child can write there; every Medium-or-higher object elsewhere (profile, other repositories, `%TEMP%`, registry) rejects writes with an OS access-denied error. Reads and network access stay open by design. The integrity label requires an NTFS volume; FAT/exFAT, network shares and other non-NTFS targets are not covered, and `varynth doctor` reports the resolved enforcement state instead of assuming it. Workspace files keep the Low integrity label after a `workspace-write` run; a later `read-only` run re-raises the workspace to Medium first. On macOS and Linux no OS backend is implemented yet; the mode is the path jail plus shell policy only and `varynth doctor` reports it as `degraded` for that platform. Use Docker or a disposable virtual machine for untrusted commands.

The default shell allowlist is a fixed set of build and file-inspection commands. Interpreters and network clients (`python`, `node`, `npx`, `curl`, `wget`, `powershell`, `cmd`) are not on it, and the sandbox refuses exec-shaped argv (`python -c/-m`, `node -e`, `bun -e/-x`, `npm exec`, `tar --to-command`, `-EncodedCommand`, `cmd /c`) even after a binary is added back, so an allowlist entry is not a one-line code-execution grant.

Use `varynth keyring set <field>` to store a hidden-input secret. Supported fields are `proxy-token`, `openai-api-key`, `anthropic-api-key`, `google-api-key`, `telegram-bot-token`, `dashboard-token`, `openrouter-api-key`, `vercel-api-key`, `opencode-api-key`, `cloudflare-api-token`, `github-token` and `gitlab-token`. When `use_keyring = true`, the first six fields are omitted from saved TOML. `varynth keyring status` reports presence without printing values.

### Accounts

`varynth account` stores one credential per official service and replaces it on the next connect. Model accounts are provider profiles with the key in the OS keyring, never in config:

```powershell
varynth account connect openai -
varynth account connect claude -
varynth account connect openrouter -
varynth account connect vercel -
varynth account connect opencode -
varynth account connect cloudflare --account-id <32-hex> -
varynth account connect github -
varynth account connect gitlab -
varynth account status
varynth account disconnect openai
```

`-` reads the secret from stdin. `workers` is the same Cloudflare account, not a second one. Select a linked model as `openai/<model>`, `claude/<model>`, `openrouter/<model>`, `vercel/<provider>/<model>`, `opencode/<model>` or `cloudflare/<model>`. GitHub and GitLab only fill the existing plugin token (`GITHUB_PERSONAL_ACCESS_TOKEN`, `GITLAB_PERSONAL_ACCESS_TOKEN`); GitHub Models is not called. Google stays the single `varynth google login` flow.

`varynth` opens on the public Varynth models without an account. `varynth login` opens the site, writes `~/.varynth/auth.env`, and then lists the account catalog separately. `varynth logout` deletes that file. `varynth doctor` reports `varynth_auth` as `signed-in` or `signed-out` and never prints the token. Starter skills are written under `~/.varynth/skills` on first home create. Desktop control stays off unless `computer_use` is saved. It still requires a tool approval before any click, and `computer_stop` remains available when it is off. When `open-computer-use` is on `PATH`, desktop tools use that MCP server; otherwise the built-in backend remains.

`varynth endpoint add <id> --kind openai|anthropic --base-url <url> -` stores one compatible endpoint. The secret is read from stdin when the value is `-`.

`/autocompact` is on by default. Before each model call, including the next round after a tool result, once the session reaches 76% of the 96,000-token context budget, earlier history is summarized in place and the last few messages stay verbatim. `/autocompact off` disables that. `/autocompact 80` moves the line. `/compact` still summarizes immediately.


Values outside these lists stop startup with an error instead of silently falling back. `effort` maps to real provider parameters: Anthropic gets extended thinking with a budget scaled by level, OpenAI-compatible endpoints get `reasoning_effort`. HTTP 429/5xx provider responses are retried with backoff (Retry-After is honored).

Anthropic uses `ANTHROPIC_API_KEY` or `ANTHROPIC_AUTH_TOKEN`. A local proxy uses `proxy_url` (for example `http://127.0.0.1:8787`) and `proxy_token`, or `VARYNTH_PROXY_URL` and `VARYNTH_PROXY_TOKEN`. `doctor --fix` starts `varynth-proxy` from PATH when the configured `proxy_url` is a loopback address. It does not download one.

### Google Gemini and Vertex AI

The `google` and `google-vertex` providers use the official Google APIs: `generativelanguage.googleapis.com` for Gemini and `aiplatform.googleapis.com` for Vertex. Nothing else is contacted.

API-key mode (Gemini only): set `provider = "google"` plus `google_api_key` or the `GOOGLE_API_KEY`/`GEMINI_API_KEY` environment variable.

OAuth mode (Gemini or Vertex): sign in once, then set `google_auth = "oauth"`:

```powershell
varynth google login --client-id <installed-app-client-id>
varynth google status
varynth google logout
```

`login` runs the documented authorization-code flow with PKCE against `accounts.google.com`, receives the redirect on a loopback port, and stores the refresh token in the OS keyring — never in config, logs, or printed output. Vertex also needs `google_project` and `google_location`.

Automatic sign-in: when `google_auth = "oauth"` (directly or through a profile) is selected without a stored credential, model-using commands open the browser login automatically before the first request, as long as a non-secret OAuth client id is configured. Set `google_client_id` in config.toml (or `VARYNTH_GOOGLE_CLIENT_ID`) to your installed-app client id. `google_auto_login = true` is the default; set it to `false` to require an explicit `varynth google login`. When sign-in cannot start — no client id, or auto-login disabled — the command stops with an error naming the fix instead of asking for a token. The dashboard and the WebSocket gateway run the same check before applying a profile/model change; a failed sign-in leaves the active runtime unchanged.

### Provider profiles

`[[provider_profiles]]` entries add named endpoints beyond the built-in providers. Select one with `provider = "<id>"` or by a model id prefixed `<id>/` (for example `antigravity/gemini-2.5-pro`); `varynth models` lists qualified ids from every enabled profile. Each profile has its own `kind`, `base_url`, credentials (`api_key`, `api_key_env`, or a keyring slot), and an optional pinned `models` list used when the live catalog is unreachable.

Antigravity-style gateways are supported only this way: as a user-supplied OpenAI-compatible `base_url`. No internal or undocumented Antigravity API, account rotation, or device fingerprint spoofing is implemented.

Other environment overrides: `VARYNTH_MODEL`, `VARYNTH_PROVIDER`, `VARYNTH_DASHBOARD_TOKEN`, `VARYNTH_MCP_CONFIG`.

`permission_mode` controls writes. `sandbox` controls how far shell and file tools may reach. `workspace-write` is the default; on Windows its kernel boundary restricts writes only, while reads and network stay open (see the platform notes above).

Scoped rules refine the mode. `~/.varynth/permissions.json` holds user rules; `.varynth/permissions.json` in a repository is ignored until `varynth trust` records the workspace (canonical path hash) in `~/.varynth/trust.json`. A rule is `{"tool":"bash","match":"cargo *","effect":"allow"}` where `tool` and `match` are globs; evaluation is deny > plan policy > ask > allow > mode default. A deny rule wins even in `bypass`, and no rule relaxes the sandbox path/shell checks. "always + save rule" in the approval dialog appends a user rule.

`plan` mode refuses `write_file`/`edit_file`/`memory_append`, limits `bash` to read-only commands, and denies MCP tools that are not classified read-only. `/plan <task>` captures the final reply as the plan artifact; `/plan approve` switches the same session to `acceptEdits` and runs the implementation turn.

## Lifecycle hooks

Hooks are local subprocesses that run at fixed runtime boundaries. There is no plugin ABI and no in-process extension loader.

* `~/.varynth/hooks.json` is always active. `.varynth/hooks.json` in a workspace is inert until `varynth trust` records the workspace.
* Events: `pre_tool`, `post_tool`, `user_prompt`, `session_start`, `session_stop`.
* Each invocation is one process. stdin gets `{"event","tool","args","cwd","session_id"}`; stdout may answer `{"decision":"allow|deny|ask","reason":"...","context":"..."}`.
* A `pre_tool` deny blocks the call even in `bypass`; a non-zero `pre_tool` exit denies with its stderr as the reason. `post_tool` `context` is appended to the tool result; `user_prompt` `context` is appended to the prompt. A `user_prompt` deny cancels the turn before the model call.
* Every invocation has a hard timeout: `hooks_timeout_secs` in config.toml (default 10 s), overridable per hook. `hooks_timeout_policy` (default `deny`; `ask` is the only alternative) decides a `pre_tool` timeout, so a hung guard never silently allows the call; timeouts on other events are a warning and the turn continues.

```json
{
  "timeout_secs": 10,
  "hooks": {
    "pre_tool": [{ "command": "python tools/guard.py", "tools": ["bash"] }],
    "post_tool": ["python tools/format.py"],
    "user_prompt": ["python tools/redact.py"],
    "session_start": ["python tools/hello.py"]
  }
}
```

An entry is a command string or `{ "command": ..., "timeout_secs": ..., "tools": [globs] }`.

## User-defined commands

Drop markdown files in `~/.varynth/commands/` or `.varynth/commands/` (project files load only after `varynth trust`). The file stem is the slash name and appears in the `/` menu after the built-ins.

```markdown
---
description: Write a commit message from the staged diff
argument-hint: [scope]
---

!git diff --cached

Write a conventional commit message for the diff above. Extra context: $ARGUMENTS
```

`description` is required; a file without one is ignored with a warning. `$ARGUMENTS` is replaced with the raw text after the command. A line starting with `!` runs through the existing `bash` tool and permission gate (so `prompt` mode asks, rules and plan mode apply, and `pre_tool` hooks see it) and its output is injected into the message. Invoking a command expands to a user message in the same turn — it is not a new turn type. `varynth commands list [--json]` prints the discovered surface and any skipped files.

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

Open `http://127.0.0.1:7420`. Control Room has a searchable session rail, live conversation streaming, activity and approval tabs, model selection, skills, health checks and a runtime inspector. On narrow screens the session rail and inspector open as dismissible side panels. The dashboard token is always required, loopback included. Without a configured `dashboard_token`/`VARYNTH_DASHBOARD_TOKEN`, `varynth serve` generates an ephemeral token and prints the tokenized URL only when stderr is a terminal; for redirected output, automation or a LAN bind, set the token explicitly. The Connection dialog keeps its token in this tab's `sessionStorage` (reload-safe; "Clear token" removes it) and applies it to HTTP, SSE and WebSocket requests. Requests with a foreign `Host` or `Origin` are rejected.

Telegram needs its own bot token from BotFather, not a token shared with another app. Put it in `~/.varynth/telegram.env` as `TELEGRAM_BOT_TOKEN=...`, or set `VARYNTH_TELEGRAM_BOT_TOKEN`, then run `varynth serve`. The first direct message tells you your numeric user id. Add that id to `telegram_allow_from` and restart serve.

```toml
telegram_allow_from = ["123456789"]
```

Discord and WhatsApp adapters are not in this binary.

## Computer Use

Desktop tools are opt-in: set `VARYNTH_COMPUTER_USE=1` in the Varynth process environment, then use `prompt` for interactive decisions or explicitly select `bypass`. They provide screenshot capture with a coordinate grid, mouse click/drag, keyboard text/key combinations and Windows window list/focus/resize. `computer_stop` remains available without opt-in. The emergency stop is the pointer corner `(0,0)` or Esc / Ctrl+Alt+Q. Docker mode denies desktop access; read-only mode denies desktop input. Screenshots are written under the workspace jail, not arbitrary paths.

The implementation's parsers, jail checks and failsafe paths are covered by offline tests. Live desktop interactions are not run by the project test suite.

## Notifications

Desktop notifications are off by default. Set `notifications = "auto"` in `config.toml`, or `VARYNTH_NOTIFICATIONS=auto`, to raise one OS toast when an approval is waiting, a background subagent or automation task finishes, or a turn ends while the terminal is unfocused. A focused terminal is re-checked after `notifications_delay_secs` (default 5) and stays quiet if you are still there. Repeated approvals for the same tool inside 30 seconds are coalesced into one toast.

Toast text is a fixed label plus the tool name (`Approval needed: bash`); command lines, paths and message bodies are never included. On Windows the toast uses the built-in PowerShell WinRT projection with the registered Windows PowerShell app id, so no administrator rights or tray application are needed; if PowerShell is unavailable, `msg.exe` is used and `varynth doctor` reports the fallback. Check capability with `varynth notify status` and fire one toast with `varynth notify test`.

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
