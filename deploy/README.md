# Varynth dağıtımı (tek sunucu, Ubuntu/Debian)

Mimari:

- `varynth.zuzia.dev` → Nginx (TLS) → `/v1/*` ve `/v1` API'ye (`127.0.0.1:8080`), geri kalan her şey Next.js'e (`127.0.0.1:3000`).
- API `varynth-api.service` olarak çalışır, açılışta migration'ları kendisi uygular. Veritabanı yerel PostgreSQL.
- Web `varynth-web.service` olarak çalışır, sunucu tarafında API'ye `API_URL=http://127.0.0.1:8080` ile bağlanır.

Sunucuda hiçbir sır repoya yazılmaz. Gerçek değerler yalnızca `/etc/varynth/api.env` ve `/etc/varynth/web.env` dosyalarında, `root` tarafından okunabilir şekilde durur.

## 1. Sunucu hazırlığı (bir kez)

Sunucuda root olarak (önce SSH anahtarı ile giriş kurun, bkz. aşağı):

```bash
bash /opt/varynth/src/deploy/scripts/server-bootstrap.sh
```

Bu betik: PostgreSQL, Nginx, Rust (rustup), Node.js 20, `varynth` veritabanı/kullanıcısı (rastgele şifre üretir ve `/etc/varynth/db.secret` dosyasına yazar), `/etc/varynth/*.env` şablonlarını ve systemd birimlerini kurar. Sırları yazdırmaz.

## 2. Sırları doldurun

`DATABASE_URL` ve `JWT_SECRET` bootstrap tarafından otomatik üretilir; elle doldurmanız gerekmez. `/etc/varynth/api.env` içinde ayrıca şunları doğrulayın/ekleyin:

- `VARYNTH_API_PUBLIC_URL=https://varynth.zuzia.dev`
- `VARYNTH_TRUSTED_PROXIES=127.0.0.1`
- `VARYNTH_STUB_UPSTREAM=0`, `VARYNTH_BILLING_STUB=0`
- Kullanacağınız sağlayıcı anahtarları (`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `OPENROUTER_API_KEY` …)

`/etc/varynth/web.env` içinde:

- `API_URL=http://127.0.0.1:8080`
- `NEXT_PUBLIC_SITE_URL=https://varynth.zuzia.dev`

## 3. Kod yükleme ve derleme

Yerel makinenizden (Git Bash):

```bash
SSH_TARGET=root@<sunucu-ip> bash deploy/scripts/deploy.sh
```

Betik çalışma ağacını `tar` ile gönderir (`target/`, `node_modules/`, `.next/`, `.env*` hariç), sunucuda `cargo build --release` ve `npm ci && npm run build` çalıştırır, sonra iki servisi yeniden başlatır.

## 4. TLS ve Nginx

Alan adı A kaydı sunucuya işaret ettikten sonra:

```bash
cp /opt/varynth/src/deploy/nginx/varynth.conf /etc/nginx/sites-available/varynth
ln -sf /etc/nginx/sites-available/varynth /etc/nginx/sites-enabled/varynth
nginx -t && systemctl reload nginx
certbot --nginx -d varynth.zuzia.dev
```

## 5. Doğrulama

```bash
systemctl status varynth-api varynth-web --no-pager
curl -sS http://127.0.0.1:8080/v1/models/public | head -c 300
curl -sSI https://varynth.zuzia.dev/ | head -5
```

## SSH anahtarı (şifre yerine)

Yerel makinenizde (bir kez, şifre gerektirmeyen giriş için):

```bash
ssh-keygen -t ed25519 -C "varynth-deploy" -f ~/.ssh/varynth_deploy
ssh-copy-id -i ~/.ssh/varynth_deploy.pub root@<sunucu-ip>
```

`ssh-copy-id` sizden sunucu şifresini ister; bunu kendiniz girin. Sonrasında betikler anahtarla çalışır. Sunucuda şifre ile SSH girişini kapatmak için `/etc/ssh/sshd_config` içinde `PasswordAuthentication no` ayarlayıp `systemctl reload ssh` yapın (önce anahtarla giriş çalıştığını doğrulayın).

**Not:** Sohbette paylaşılan sunucu şifresini değiştirmenizi öneririm.
