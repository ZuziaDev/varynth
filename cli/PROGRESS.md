# Varynth İlerleme Durumu

Güncelleme: 2026-10-07

## 4. Tur — Dashboard gateway, gelişmiş TUI, plugin/MCP, computer-use ve final hardening

### Tamamlananlar
- Bidirectional `/ws/gateway` JSON-RPC 2.0: status, sessions, messages, pause/resume, bounded context replacement, model selection, remote approval.
- Dashboard event fan-out and local cross-process `~/.varynth/control` journal; TUI, `serve` and gateway share events without trusting arbitrary paths.
- Constant-time dashboard token checks, same-origin/Host protections, WebSocket frame/message limits and lag handling.
- Purple/cyberpunk/dark/minimal theme palette; `/config`, `/context`, `/mcp`, `/new`, `/clear`, `/rename`, `/sandbox`, `/permission` controls; runtime provider reconfiguration is atomic.
- Prefix composer: `#` jailed file pins, `@` persona/terminal/session addressing, `$` skill rows; multiline paste cards are editable and preserve exact content.
- Skills.sh search/install client with frontmatter validation, 2 MiB cap, safe names, env requirement warnings, offline install tests and live slash refresh.
- Curated plugin marketplace with owned manifests, atomic MCP merge/removal and official Spotify currently-playing API path; no fake credentials or package execution.
- Computer-use tools are explicit opt-in (`VARYNTH_COMPUTER_USE=1`), sandbox/prompt-gated, coordinate-bounded and failsafe-protected; screenshots are jailed. Docker shell uses network none, dropped capabilities, read-only root and resource limits by default.
- Shell output drains stdout/stderr concurrently with bounded retention and timeout process-tree cleanup; web fetch rejects private/mapped/documentation IPs, disables proxies and revalidates every redirect's DNS/IP.
- Keyring-backed secrets, atomic config writes, symlink-safe jail resolution, session checkpoints/undo, goal auditor/budgets, provider streaming/retry/usage and MCP stdio remain enabled.

### Final verification
- `cargo fmt --all` clean.
- `cargo check --all-targets` clean.
- `cargo test --no-fail-fast`: **319 library + 3 binary + 22 integration = 344 passed, 0 failed**.
- `npm --prefix bindings/node test`: passed (TypeScript build + 1 Node test).
- `cargo run -- --help` and `cargo run -- models`: passed.
- `cargo build --release`: passed.
- Windows release SHA-256: `9772b6d0d2629989eb7eaf51f69f9c82ff2af0ee8ba4e58c3188088e941e6bcc`.

### Environmental limits
- Docker daemon/image checks are fail-open tests and were not used as proof of live container execution when unavailable.
- No real desktop clicks, keyboard input, window focus/resize or screenshot capture was performed during verification; computer-use is intentionally opt-in.
- No live provider credentials, Telegram bot polling, external skills.sh install or authenticated third-party API call was used in tests.

## 3. Tur — Streaming, MCP stdio, izin onayı, checkpoint + güvenlik (4 ajan, 2 dalga)

### Yeni özellikler
- **SSE token streaming**: `Provider::complete_streaming` + Anthropic/OpenAI-uyumlu SSE ayrıştırıcılar (araç çağrı delta'ları dahil); TUI'da canlı yanıt balonu, delta'sız akış hatasında tek `complete` düşüşü, `stream = false` ile kapatılabilir.
- **MCP stdio transport**: `{"type":"stdio","command":...}` sunucuları ilk kullanımda başlatılıyor, JSON-RPC 2.0 satır framing'i, 30 s istek deadline'ı, ölünce otomatik yeniden başlatma, Drop'ta temizleme.
- **Etkileşimli izin onayı**: `prompt` modu TUI'da onay diyaloğu açıyor (o bir kez / a her zaman / d reddet); TUI'sız bağlamlarda net reddetme mesajı; `approved_always` oturum içi hafıza.
- **Checkpoint & /undo**: her write/edit öncesi dosya anlık görüntüsü (`~/.varynth/checkpoints`, oturum başına 100, 2 MB üstü atlanır); `/undo` LIFO geri alma.

### Güvenlik düzeltmeleri
- **Sandbox symlink kaçışı kapatıldı**: çok seviyeli eksik yollarda (`jail/link/a/b/new.txt`)lexical fallback symlink'i görmezden geliyordu; artık ata zincirinde canonicalize ediliyor (junction ile test edildi).
- **Credential hasadı kaldırıldı**: `ANTHROPIC_AUTH_TOKEN` artık sessizce proxy token'ı olmuyor; OneDrive PowerShell profili kazıma fonksiyonu silindi. Proxy token yalnızca `proxy_token` config veya `VARYNTH_PROXY_TOKEN`.
- **Telegram**: 8.000 karakter üstü girdiler modele iletilmiyor; hata yanıtları 500 karakterde kırpılıyor (char-boundary güvenli).

### Doğrulama
- `cargo fmt --check`, `cargo check --all-targets` temiz.
- `cargo test`: 198 geçti, 0 başarısız (dalga 1 sonrası 187).
- `cargo build --release` başarılı.

## 2. Tur — Goal motoru ve oturumlar arası iletişim (3 paralel ajan)

### Goal motoru (Claude Code/Codex tarzı)
- `GoalState` durum makinesi: condition, rounds/max_rounds, süre bütçesi, status (active/met/stopped), son denetim kararı.
- Bağımsız **auditor**: her turdan sonra ayrı bir model çağrısı transkripti koşula karşı denetliyor, `{"met": bool, "reason": str}` dönüyor; modelin `GOAL_COMPLETE` iddiası tek başına yetmiyor, auditor onaylamadan hedef kapanmıyor.
- Bütçe: `/goal --rounds N` / `--minutes M` veya config'den `goal_max_rounds` (varsayılan 25) / `goal_max_minutes`; bütçe bitince `goal stopped` ve otomatik devam duruyor.
- `/goal status`, `/goal clear`; TUI başlığında `goal N/M` göstergesi; eski hardcoded 12-tur limiti kalktı.
- `status_json` goal bloğu: koşul, tur, bütçe, geçen süre, durum, son karar.

### Oturumlar arası iletişim (mesaj veriyolu)
- Yeni `src/mailbox.rs`: `~/.varynth/bus.jsonl` üzerinde lock dosyalı (TaskStore deseni), 24 saatlik prune'lu JSONL veriyolu; `send/unread/drain/peers`, `latest` takma adı.
- Yeni araçlar: `session_send` (oturumdan oturuma mesaj), `session_inbox` (gelen kutusunu oku ve temizle) — `tools::dispatch` artık `ToolCtx` (bus + session_id) alıyor.
- Turn başında bekleyen mesajlar otomatik olarak bağlama enjekte ediliyor (en fazla 20), model aynı turda görüyor.
- TUI/REPL yerel komutları: `/send <id|latest> <text>`, `/inbox` (model çağrısı olmadan).
- Dashboard: `POST /api/session/{id}/messages` + oturum listelerinde `unread` sayacı; Host-guard yeniden düzenlendi (test edilebilirlik).
- Testler: veriyolu (send/drain/prune/peers/latest/truncation/bozuk satır), araçlar (dispatch roundtrip, doğrulama), goal (auditor onayı/redmi, bütçe durması, enjeksiyon, `/send`/`/inbox`), dashboard (400/host guard).

### Doğrulama
- `cargo fmt --check`, `cargo check --all-targets` temiz.
- `cargo test`: 155 geçti, 0 başarısız (baseline 139).
- `cargo build --release` başarılı.

## 1. Tur — Güvenlik, provider ve runtime düzeltmeleri (3 paralel ajan)

### Güvenlik düzeltmeleri
- `truncate` UTF-8 char-boundary paniki düzeltildi (`tools`, `providers/proxy`).
- `web_fetch` SSRF koruması: loopback/private/link-local/metadata IP'leri engellendi, her redirect adımı yeniden kontrol ediliyor; blocking client artık async runtime dışında OS thread'inde çalışıyor.
- Dashboard: token karşılaştırması constant-time, yabancı `Host` header'ı 400, permissive CORS kaldırıldı.
- Config doğrulama: bilinmeyen `provider`/`permission_mode`/`sandbox`/`effort` değeri sessiz fallback yerine hata veriyor.
- `doctor --fix` makineye özel hardcoded yol kaldırıldı; PATH'te `varynth-proxy` aranıyor.
- Bilinmeyen araç çağrısı artık modele hata sinyali veriyor (Ok-string değil).
- `config.example.toml` var olmayan anahtarlar temizlendi, `telegram_allow_from` string formuna çevrildi.

### Provider katmanı
- Üç provider da `usage` (input/output token) döndürüyor; `/cost` artık gerçek sayaç + süre gösteriyor.
- Retry/backoff: 429/5xx ve bağlantı hatalarında 2 ek deneme, Retry-After desteği.
- `effort` gerçek parametreye bağlandı: Anthropic extended thinking (high/xhigh/max), OpenAI-uyumlu `reasoning_effort`; parametre reddedilirse otomatik düşüş.
- Anthropic `list_models` gerçek `/v1/models` çağrısı, hata durumunda hardcoded katalog fallback'i.
- `max_tokens` ve `temperature` config'den yönetilebilir.

### Runtime / UI
- `compact.rs` modülü lib.rs'e bağlandı (ölü kod değildi artık): oturum bütçeyi aşınca tur öncesi yerinde otomatik özetleme; `/compact` yeni oturum açmak yerine aynı oturumu sıkıştırıyor.
- `session.rs` meta kaydı artık her append'de kopyalanmıyor; yerinde compaction diskte `replace` kaydıyla taşıyor.
- `/review` TUI ve plain REPL'e bağlandı (ölü koddu).
- `activity.rs` TaskStore desenine uygun lock + atomic write'a geçti.
- `status_json` araç listesini `tools::schemas()`'dan türetiyor.

### Doğrulama
- `cargo fmt --check` başarılı.
- `cargo check` uyarısız başarılı.
- `cargo test`: 133 test geçti, 0 başarısız (110 lib + 23 entegrasyon; baseline 90'dı).

## Önceki Turlar

### 2026-10-02

- Rust agent runtime korunarak OpenClaw benzeri temel yapı eklendi.
- `SOUL.md`, `USER.md`, `MEMORY.md`, `DREAM.md`, `HEART.md` dosyaları eklendi.
- Global `~/.varynth` ve proje `.varynth` katmanları destekleniyor.
- Agent dosyaları model context'ine bounded ve untrusted delimiter ile ekleniyor.
- `memory_read` ve `memory_append` araçları eklendi.
- Provider credential zorunluluğu eklendi.
- API key/token yoksa model kullanan komutlar başlamıyor.
- `task add/list/run/enable/disable/remove` komutları eklendi.
- `--at` ve `--every-seconds` otomasyonları eklendi.
- `serve` içinde 15 saniyelik scheduler loop eklendi.
- REPL'e `/remind <seconds> <message>` eklendi.
- Background task'lerde shell ve write araçları varsayılan olarak engellendi.
- Shell command chaining ve session path traversal güvenlik düzeltmeleri eklendi.
- Dashboard için loopback varsayılanı ve uzak API Bearer token doğrulaması eklendi.
- `bindings/node` altında ESM TypeScript Node.js paketi eklendi.
- `VarynthClient` ve `AgentWorkspace` API'leri eklendi.
- README, config örneği ve uygulama planı güncellendi.

## Son Turda Tamamlananlar

- `TaskStore::mark_run` artık `run_id` ve aktif lease sahipliğini doğruluyor.
- Lease süresi dolduktan sonra eski çalıştırmanın yeni çalıştırmanın sonucunu ezmesi engellendi.
- Scheduler completion akışı lease kaybını loglayıp güvenli şekilde atlıyor.
- Manuel `task run` akışı lease sahipliği ile uyumlu hale getirildi.
- Duplicate completion ve lease-expiry regresyon testleri eklendi.

## Son Doğrulama

- `cargo fmt --check` başarılı.
- `cargo check` başarılı.
- `cargo test --no-fail-fast` başarılı: 14 Rust unit testi ve entegrasyon testleri.
- `cargo build --release` başarılı.
- `bindings/node/npm test` başarılı.

## İlgili Dosyalar

- `src/agent_files.rs`
- `src/automation.rs`
- `src/runtime.rs`
- `src/config.rs`
- `src/dashboard.rs`
- `src/session.rs`
- `src/sandbox.rs`
- `bindings/node/src/index.ts`
- `README.md`
- `docs/superpowers/plans/2026-10-02-openclaw-agent.md`
