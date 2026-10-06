# Varynth Roadmap

Varynth is a local coding agent built as a Rust CLI. The current release includes a terminal UI, provider streaming, goal auditing, session messaging, a local dashboard gateway, MCP transports, skill installation, permission controls, checkpoints, and optional computer-use tools.

## Candidate Improvements

- Add automated Windows release checks and a broader operating-system test matrix.
- Add provider fallback with explicit model selection and usage accounting.
- Expand deterministic tests for cron scheduling, command cancellation, and headless output.
- Improve MCP health reporting and refresh schemas after server changes.
- Expand the plugin manifest format with declared tool permissions and compatibility checks.
- Add session and memory search with bounded, project-scoped results.
- Add signed webhook triggers with replay protection and idempotency keys.
- Improve the editable paste modal, model selection, and keyboard accessibility.
- Add richer agent activity visualization and live file-diff review.
- Extend onboarding with validated configuration and keyring guidance.

## Release Boundaries

- Windows is the currently verified binary release platform.
- Workspace file restrictions and shell policies are not operating-system process isolation.
- Docker execution requires an installed daemon and a suitable sandbox image.
- Computer-use tools remain explicit opt-in and must not be exercised by ordinary automated tests.
- Credentials, local sessions, memory files, logs, control journals, and terminal registries must not be included in source packages.
