# OpenClaw Benzeri Agent Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use subagent-driven development to implement this plan task-by-task.

**Goal:** Mevcut Varynth Rust CLI'ına zorunlu model sağlayıcısı, dosya tabanlı agent kimliği/belleği, her tur model çağrısı ve kalıcı otomasyon/hatırlatıcı altyapısı eklemek.

**Architecture:** Mevcut Rust crate korunur. Agent dosyaları global `~/.varynth` ve proje `.varynth` katmanlarından okunur; güvenli boyut sınırı ve delimiter ile model bağlamına eklenir. Provider credential doğrulaması model kullanan tüm akışların kapısında çalışır. Otomasyon, `tokio` interval ve atomik JSON/JSONL repository ile başlar; `SchedulerStore` ve `NotificationSink` trait'leri sonraki SQLite/kanal adaptörlerini mümkün kılar.

**Tech Stack:** Rust 2021, Tokio, Serde/serde_json, mevcut provider/runtime/session/tools modülleri, Clap.

## Global Constraints

- API key/token yoksa model kullanan komutlar ve agent turları açık hata ile durur.
- Her gerçek kullanıcı veya otomasyon turunda provider çağrısı zorunludur; fallback/local fake model yoktur.
- Agent markdown dosyaları kullanıcı verisi olarak delimiter'lanır, boyutlandırılır ve tool talimatı olarak yorumlanmaz.
- Background job'lar varsayılan read-only'dir; shell/dosya yazma açık izin olmadan çalışmaz.
- Secret değerleri log, hata, status veya prompt içine yazılmaz.
- Public API ile iç implementasyon modülleri ayrılır; yeni davranış test edilir.

---

### Task 1: Agent Files ve Context Loader

**Files:**
- Create: `src/agent_files.rs`
- Modify: `src/lib.rs`
- Modify: `src/config.rs`
- Test: `tests/agent_files.rs`

**Deliverable:** `SOUL.md`, `USER.md`, `MEMORY.md`, `DREAM.md`, `HEART.md` dosyalarının global/proje katmanlarından güvenli okunması, eksiklerin overwrite etmeden oluşturulması ve bounded system context üretimi.

**Required interfaces:** `AgentFiles::load(cwd)`, `AgentFiles::ensure(cwd)`, `AgentFiles::system_context()`, `AgentFiles::append_memory(entry)`.

### Task 2: Provider Credential Gate ve Runtime Turn Contract

**Files:**
- Modify: `src/config.rs`
- Modify: `src/runtime.rs`
- Modify: `src/providers/proxy.rs`
- Modify: `src/main.rs`
- Test: `tests/provider_credentials.rs`

**Deliverable:** OpenAI, Anthropic ve proxy için credential zorunluluğu; `exec`, `repl`, `serve`, `models` ve runtime turn provider olmadan çalışmaz. Her gerçek turn için fake provider call-count testleri eklenir.

**Required interfaces:** `Config::require_provider_credentials() -> Result<()>`, `Runtime::turn(...)` provider çağrısını tool round'ları için zorunlu tutar.

### Task 3: Session Context ve Memory Tools

**Files:**
- Modify: `src/session.rs`
- Modify: `src/runtime.rs`
- Modify: `src/tools/mod.rs`
- Test: `tests/session_context.rs`

**Deliverable:** Her session system prompt'una AgentFiles context eklenir. `memory_read`, `memory_append` ve kontrollü `memory_replace` yalnızca izinli agent dosyalarında çalışır. Session/job ID path-safe doğrulanır.

### Task 4: Automation Store ve Scheduler

**Files:**
- Create: `src/automation.rs`
- Modify: `src/lib.rs`
- Modify: `src/config.rs`
- Modify: `src/main.rs`
- Test: `tests/automation.rs`

**Deliverable:** `At(RFC3339)` ve `Every(duration)` trigger'ları, JSON atomik persistence, lease/run idempotency, due job polling ve dedicated session ile model-driven çalıştırma.

**Required interfaces:** `Task`, `Trigger`, `TaskStore`, `AutomationEngine`, `NotificationSink`.

### Task 5: CLI Task Commands ve Serve Integration

**Files:**
- Modify: `src/main.rs`
- Modify: `src/dashboard.rs`
- Modify: `src/telegram.rs`
- Test: `tests/automation_cli.rs`

**Deliverable:** `task add/list/run/disable/remove`, `/remind` ve `serve` içinde scheduler lifecycle. Dashboard/Telegram bildirimi mevcut adapter'lara güvenli biçimde bağlanır.

### Task 6: Security Hardening

**Files:**
- Modify: `src/sandbox.rs`
- Modify: `src/dashboard.rs`
- Modify: `src/session.rs`
- Modify: `src/telegram.rs`
- Test: `tests/security_regressions.rs`

**Deliverable:** shell command chaining bypass engeli, dashboard auth/loopback varsayılanı, session path traversal engeli ve Telegram allowlist testleri.

### Task 7: Documentation ve Full Verification

**Files:**
- Modify: `README.md`
- Modify: `config.example.toml`
- Create: `docs/agent-files.md`
- Test: mevcut tüm testler ve yeni integration testler

**Deliverable:** Credential kurulumu, agent dosyaları, model-driven task örnekleri ve güvenlik davranışı gerçek CLI ile uyumlu şekilde belgelenir. `cargo fmt --check`, `cargo check`, `cargo test --no-fail-fast` geçer.

### Task 8: Node.js/TypeScript Binding

**Files:**
- Create: `bindings/node/package.json`
- Create: `bindings/node/tsconfig.json`
- Create: `bindings/node/src/index.ts`
- Create: `bindings/node/test/workspace.test.mjs`
- Modify: `README.md`

**Deliverable:** ESM TypeScript package exposes `VarynthClient` for the Rust binary and `AgentWorkspace` for layered agent files. `npm test` compiles the package and validates workspace memory behavior.
