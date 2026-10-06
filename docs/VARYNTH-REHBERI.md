# Varynth CLI — Detaylı Rehber

> Güncel durum özeti, 2026-10-06. Tek Rust binary'si (`varynth`), 198 test yeşil,
> release build doğrulanmış. Bu belge ajanın bugünkü tüm yeteneklerini açıklar.

---

## 1. Varynth nedir?

Varynth, kullanıcının kendi makinesinde çalışan **yerel bir kodlama ajanıdır**.
OpenAI / Anthropic / yerel proxy uçlarındaki bir modele bağlanır, dosyaları okuyup
düzenler, komut çalıştırır, web'e erişir — hepsi belirlenen sandbox ve izin
politikaları içinde. Tek bir `varynth.exe` içinde şu yüzler vardır:

| Yüz | Komut | Ne için |
|---|---|---|
| Tam ekran TUI | `varynth` | Günlük kullanım; akışlı yanıt, goal rozeti, onay diyaloğu |
| Satır REPL | `varynth --plain` | Terminal uyumsuz ortamlar |
| Tek atımlık | `varynth exec "..."` | Betikler, CI, tek soruluk işler |
| Web panosu | `varynth serve` → `127.0.0.1:7420` | Tarayıcıdan sohbet + oturum yönetimi + API |
| Telegram | `varynth serve` + bot token | Cebinden ajanı sürmek |
| Zamanlanmış görevler | `varynth task ...` | `--at` / `--every-seconds` otomasyon |
| Node.js paketi | `bindings/node` | JS/TS'ten `VarynthClient` ile kullanım |

---

## 2. Mimari haritası

```
src/
├── main.rs            CLI girişi: subcommand'lar, plain REPL
├── runtime.rs         Ajan döngüsü: tur → araç çağrısı → goal denetimi
│                      (ApprovalRequest, GoalState, usage/compaction takibi)
├── tui.rs             Ratatui tabanlı tam ekran arayüz, akışlı çizim, onay diyaloğu
├── composer.rs        Giriş kutusu: satır sarma, otomatik tamamlama, kuyruk
├── providers/         openai.rs · anthropic.rs · proxy.rs (+ ortak mod.rs)
│                      SSE streaming, retry/backoff, usage, effort→parametre
├── tools/mod.rs       Yerleşik araç şemaları + dispatch (ToolCtx ile)
├── sandbox.rs         Dizin hapishesi + shell allowlist/meta-karakter filtresi
├── permissions.rs     acceptEdits | prompt | bypass modları
├── checkpoint.rs      /undo için yazma öncesi dosya anlık görüntüleri
├── mailbox.rs         Oturumlar arası mesaj veriyolu (~/.varynth/bus.jsonl)
├── compact.rs         Bağlam bütçesi aşımında yerinde özetleme
├── session.rs         JSONL oturum kaydı (meta/message/replace)
├── goal → runtime     GoalState durum makinesi + bağımsız auditor
├── mcp.rs             MCP istemcisi: HTTP + streamable HTTP + stdio transport
├── dashboard.rs       Axum web panosu + REST + SSE sohbet + auth/host guard
├── telegram.rs        Telegram bot adaptörü (allowlist + pairing)
├── automation.rs      TaskStore: lease'li görev çalıştırma, 15 sn scheduler
├── agent_files.rs     SOUL/USER/MEMORY/DREAM/HEART kişilik dosyaları
├── skills.rs          SKILL.md keşfi (/isim ile çağrım)
├── plugins.rs         Bağlayıcı durumu (GitHub/Google, env tabanlı)
├── activity.rs        Lock'lu etkinlik günlüğü
├── clipboard.rs       Pano görsel eki (Alt+V)
├── mascot.rs          TUI maskot animasyonu
└── channels.rs        Kanal soyutlaması (Discord/WhatsApp ileride)
```

Depolama: her şey `~/.varynth/` altında — `config.toml`, `sessions/*.jsonl`,
`tasks.json`, `bus.jsonl`, `checkpoints/`, agent dosyaları. Proje katmanı
`<proje>/.varynth/` ve `VARYNTH.md`/`AGENTS.md`/`CLAUDE.md`.

---

## 3. Ajan döngüsü (kalbi)

`Runtime::turn` her kullanıcı mesajında şunu yapar:

1. **Yerel komut ayrımı** — `/goal`, `/send`, `/inbox` model çağrısı olmadan yanıtlanır.
2. **Posta enjeksiyonu** — veriyolunda bu oturuma gelen okunmamış mesajlar (≤20)
   `[messages from other sessions]` bloğu olarak bağlama eklenir.
3. **Skill çözümü** — `/isim` ile çağrılan skill'in gövdesi prompt'a sarılır.
4. **Otomatik compaction** — bağlam bütçeyi aşarsa (token bütçesi ~96k,
   gerçek `usage.input_tokens` varsa o esas alınır) eski turlar modelle
   özetlenip **aynı oturumda** değiştirilir; özetleme başarısızsa tur düşmez.
5. **Araç döngüsü** — en fazla `max_tool_rounds` (24) tur: model çağrılır
   (SSE akışlı), tool_calls varsa izin kapısından geçirilip çalıştırılır,
   sonuçlar modele geri verilir.
6. **Goal denetimi** — aktif goal varsa bağımsız auditor kararı işlenir (bkz. §5).
7. **Kullanım kaydı** — input/output token'ları, tur sayısı, duvar saati süresi
   birikir; `/cost` ve `status_json` bunları raporlar.

### Yerleşik araçlar (11)

`read_file` · `write_file` · `edit_file` (benzersiz eşleşme zorunlu) ·
`glob_search` · `grep_search` (regex + glob filtresi) · `bash` (hapisheli,
timeout'lu, PowerShell/sh) · `web_fetch` (SSRF'e kapalı) · `memory_read` ·
`memory_append` · `session_send` · `session_inbox`.

`write_file`/`edit_file` çalışmadan önce dosyanın eski hali `CheckpointStore`
tarafından kaydedilir → `/undo` son yazmayı geri alır (LIFO, oturum başına
100 kayıt, 2 MB üzeri dosyalar atlanır).

---

## 4. Goal motoru (Claude Code/Codex tarzı denetimli döngü)

```
/goal <koşul>            hedefi kur, ajan koşul tutana kadar çalışır
/goal --rounds 15 <...>  tur bütçesi; --minutes 30 duvar saati bütçesi
/goal status             koşul, tur N/M, geçen süre, son auditor kararı
/goal clear | /goal stop hedefi durdur (terminal durum görünür kalır)
```

- **Bağımsız auditor**: her turdan sonra ayrı bir model çağrısı son ~30 mesajın
  transkriptini koşula karşı değerlendirir ve `{"met": bool, "reason": str}`
  döner. Modelin `GOAL_COMPLETE` satırı **tek başına yetmez** — auditor onayınca
  hedef kapanır. Auditor model iddia etmeden de kapatabilir (kanıta bakar).
- **Bütçe**: `--rounds`/`--minutes` veya config'ten `goal_max_rounds`
  (varsayılan 25) / `goal_max_minutes`. Bütçe bitince `[goal stopped]`.
- **Görünürlük**: TUI başlığında `goal N/M` rozeti + geçen süre; her turda
  `[goal N/M] <gerekçe>` veya `[goal met]` olayı sohbete düşer.
- Auditor hata verirse tur asla düşmez; karar "met değil" sayılır.

---

## 5. Oturumlar arası iletişim

`~/.varynth/bus.jsonl` üzerinde lock dosyalı (çoklu süreç güvenli), 24 saatte
budanan JSONL veriyolu. Mesaj: `{id, from, to, text, sent_at, read}`.

| Kanal | Kullanım |
|---|---|
| Ajan → ajan | `session_send {to, text}` / `session_inbox` araçları (`latest` takma adı en yeni diğer oturumu çözer) |
| Klavye → ajan | `/send <id\|latest> <metin>`, `/inbox` |
| Dashboard → ajan | `POST /api/session/{id}/messages` `{"text": "..."}` |
| Otomatik teslim | Bekleyen posta hedef oturumun **sonraki turunun başında** bağlama enjekte edilir |

Böylece iki terminal sekmesindeki iki Varynth oturumu, ya da TUI ile dashboard,
birbirine mesaj atarak iş birliği yapabilir.

---

## 6. Güvenlik modeli

**İzin modları** (`permission_mode`):
- `acceptEdits` (varsayılan) — yazma/kabuk otomatik onay
- `prompt` — **etkileşimli onay**: yazma/kabuk/MCP çağrılarında TUI diyaloğu
  çıkar (`izin ver / her zaman izin ver / reddet`); `approved_always` kümesi
  kalıcı onayları tutar; 120 sn içinde yanıt gelmezse reddedilir. Dashboard/
  Telegram tarafında onaycı yoksa sessiz reddetmek yerine hata bildirilir.
- `bypass` — her şey onaylı (dikkatli kullanın)

**Sandbox** (`sandbox`): `read-only` | `workspace-write` (varsayılan, çalışma
diziniyle sınırlı) | `danger-full-access`. `add_dirs` ile ek dizin açılır.
Bilinmeyen mod/mod/effort değeri sessizce düşmek yerine **başlatmayı durdurur**.

**web_fetch SSRF koruması**: loopback, özel (10/8, 172.16/12, 192.168/16),
link-yerel (169.254/16, fe80::/10), unique-local, CGNAT ve cloud-metadata
aralıkları engellenir; **her redirect adımı** yeniden denetlenir; fetch ayrı
OS thread'inde çalışır (async runtime panik riski yok).

**web panosu**: yabancı `Host` header'ı 400 (DNS-rebinding koruması), açık CORS
yok, token varsa constant-time karşılaştırma; token olmadan yalnızca loopback.

**Diğer**: shell meta-karakter filtresi + allowlist; arka plan görevlerinde
shell/yazma varsayılan kapalı (`--allow-background-tools` ile açılır); bilinmeyen
araç çağrısı modele açık hata döner; etkinlik günlüğü ve veriyolu lock'lu yazılır.

---

## 7. Provider katmanı

| | openai | anthropic | proxy |
|---|---|---|---|
| Uç | `openai_base_url` + key | `anthropic_base_url` + key | `proxy_url` + token |
| Streaming (SSE) | ✅ varsayılan | ✅ | ✅ (`stream = false` ile kapatılır) |
| Usage takibi | ✅ | ✅ | ✅ |
| Retry | 429/5xx'te 2 ek deneme, Retry-After destekli backoff | | |
| `effort` eşlemesi | `reasoning_effort` | extended thinking bütçesi (high/xhigh/max) | reasoning_effort |
| `list_models` | `/v1/models` | `/v1/models` (hata olursa yerleşik katalog) | `/v1/models` |

`max_tokens` ve `temperature` config'ten yönetilir. Ek parametre API'nin
reddettiği modelde otomatik düşüşle yeniden denenir.

---

## 8. MCP (Model Context Protocol)

`--mcp-config mcp.json` veya `VARYNTH_MCP_CONFIG`. **HTTP**, **streamable HTTP**
ve **stdio** transport desteklenir; stdio sunucular için istek başına deadline
var, takılan süreç öldürülür. MCP araçları `mcp__<sunucu>__<araç>` adıyla modele
açılır ve aynı izin kapısından geçer.

```json
{ "mcpServers": {
    "yerel-araclar": { "type": "http", "url": "http://127.0.0.1:9000/mcp",
                       "headers": { "Authorization": "Bearer <token>" } },
    "fs":            { "command": "npx", "args": ["-y", "@modelcontextprotocol/server-fs", "C:/projeler"] }
} }
```

---

## 9. Arayüz detayları

**TUI tuşları**: Enter gönder/kuyruğa al · Shift+Enter yeni satır · Alt+V panodaki
görseli ekle · Sol (boş draft) oturum geçmişi · Aşağı shell/alt ajan/görev listesi ·
Yukarı eski taslaklar · Shift+Tab izin modu · Esc/Ctrl+C çıkış.

**TUI slash komutları**: `/help /commands /goal /effort /status /skills /models
/model /compact /diff /review /cost /doctor /resume /agents /send /inbox /undo
/quit` — `/` yazınca tamamlama listesi açılır (kurulu skill'ler dahil).

**CLI**: `exec` · `resume [id]` · `sessions` · `models` · `doctor [--fix]`
(PATH'te `varynth-proxy`'yi bulup kaldırır; makineye özel yol yok) ·
`install [--startup]` · `serve [--host --port]` · `init` · `channels` ·
`task add/list/run/enable/disable/remove` (`--at` RFC3339, `--every-seconds`).

**Onboarding**: `varynth init` → `varynth doctor` → `varynth`.

---

## 10. Web panosu (REST + SSE)

`varynth serve` → `http://127.0.0.1:7420` (varsayılan loopback; uzak erişimde
`VARYNTH_DASHBOARD_TOKEN` zorunlu).

```
GET  /api/status                  model, izin, goal durumu, usage, araç listesi
GET  /api/models                  sağlayıcı kataloğu
GET  /api/sessions                oturumlar + unread sayaçları
POST /api/session/new             yeni oturum
GET  /api/session/{id}            oturum içeriği + unread
POST /api/session/{id}/messages   veriyoluna posta gönder
POST /api/chat                    senkron sohbet
POST /api/chat/stream             SSE sohbet
GET  /api/channels                Telegram vb. kanal durumu
GET  /api/doctor                  sağlık kontrolü
POST /v1/chat/completions         OpenAI uyumlu uç (araçlarıyla ajan olarak)
```

---

## 11. Config referansı (`~/.varynth/config.toml`)

```toml
model = "..."                        provider = "openai|anthropic|proxy"
openai_api_key / anthropic_api_key / proxy_token   (+ base_url'ler)
permission_mode = "acceptEdits"      sandbox = "workspace-write"
effort = "xhigh"                     max_tool_rounds = 24
max_tokens / temperature             (opsiyonel, provider'a iletilir)
goal_max_rounds = 25                 goal_max_minutes   (opsiyonel)
stream = true                        false → SSE kapalı (varsayılan: açık)
dashboard_host = "127.0.0.1"         dashboard_port = 7420
dashboard_token                      (uzak erişimde zorunlu)
telegram_bot_token + telegram_allow_from = ["123456789"]   (string!)
add_dirs  /  shell_allowlist  /  mcp_config
```

Env geçersiz kılmalar: `VARYNTH_MODEL · VARYNTH_PROVIDER · VARYNTH_PROXY_URL ·
VARYNTH_PROXY_TOKEN · VARYNTH_DASHBOARD_TOKEN · VARYNTH_DASHBOARD_PORT ·
VARYNTH_TELEGRAM_BOT_TOKEN · VARYNTH_TELEGRAM_ALLOW · VARYNTH_MCP_CONFIG ·
OPENAI_API_KEY · ANTHROPIC_API_KEY/AUTH_TOKEN`.

---

## 12. Kişilik, bellek, skill, görev

- **Agent dosyaları**: `SOUL.md` (kimlik), `USER.md` (kullanıcı notları),
  `MEMORY.md` (kalıcı hafıza — `memory_append` buraya yazar), `DREAM.md`,
  `HEART.md`. Global `~/.varynth/` + proje `.varynth/` katmanı; modele
  sınırlı boyutta, *güvenilmemiş bağlam* etiketiyle verilir.
- **Skill'ler**: `.varynth/skills/<ad>/SKILL.md` (veya `.claude/skills`,
  `~/.varynth/skills`); `/ad` ile çağrılır, gövde prompt'a sarılır.
- **Görevler**: `~/.varynth/tasks.json`; `serve` 15 sn'de bir due kontrol eder,
  lease mekanizmasıyla çift çalıştırma/lezih kirli yazma engellenir; her koşu
  kendi oturumunu açar.

---

## 13. Kalite durumu

| Ölçüt | Durum |
|---|---|
| Test | **198 geçti, 0 başarısız** (birim + entegrasyon) |
| Lint | `cargo fmt` temiz, `cargo check --all-targets` uyarısız |
| Release | `cargo build --release` doğrulandı (LTO + strip) |
| Kapsam | Ajan döngüsü (MockProvider ile), goal auditor, veriyolu, checkpoint/undo, onay akışı, sandbox, provider gövdeleri, MCP yapılandırma, dashboard middleware, oturum kalıcılığı |

**Bilinen sınırlar / sıradaki adaylar**: gizli anahtarlar hâlâ düz TOML'da
(keyring entegrasyonu bekliyor), shell allowlist `powershell/cmd/python` içerir
(keyfi kod çalıştırmaya açıktır — Docker sandbox fikri listede), Discord/
WhatsApp adaptörleri yok, `channels.rs` soyutlaması boş, git deposu/CI/dağıtım
(crates.io, npm) hâlâ kurulmadı.

---

*Bu rehber `docs/VARYNTH-REHBERI.md` olarak tutulur; büyük değişikliklerde
`PROGRESS.md` ile birlikte güncellenmelidir.*
