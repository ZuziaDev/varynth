#!/usr/bin/env bash
# Sunucuda root olarak bir kez çalıştırılır. Sır yazdırmaz.
set -euo pipefail

if [[ $EUID -ne 0 ]]; then echo "root olarak çalıştırın" >&2; exit 1; fi

SRC=/opt/varynth/src
ETC=/etc/varynth
DB_NAME=varynth
DB_USER=varynth

apt-get update -y
apt-get install -y postgresql nginx certbot python3-certbot-nginx curl ca-certificates build-essential pkg-config libssl-dev git

# Node.js 20
if ! command -v node >/dev/null || [[ "$(node -v | cut -d. -f1 | tr -d v)" -lt 20 ]]; then
  curl -fsSL https://deb.nodesource.com/setup_20.x | bash -
  apt-get install -y nodejs
fi

# Rust (sistem geneli)
if ! command -v cargo >/dev/null; then
  curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
  # shellcheck disable=SC1091
  source "$HOME/.cargo/env"
fi

# Sistem kullanıcısı
id -u varynth >/dev/null 2>&1 || useradd --system --home /opt/varynth --shell /usr/sbin/nologin varynth
mkdir -p "$SRC" /var/lib/varynth "$ETC"
chown -R varynth:varynth /var/lib/varynth

# PostgreSQL rolü ve veritabanı (şifre yalnızca dosyaya yazılır)
systemctl enable --now postgresql
if [[ ! -f "$ETC/db.secret" ]]; then
  umask 077
  openssl rand -hex 24 > "$ETC/db.secret"
fi
DB_PASS="$(cat "$ETC/db.secret")"
sudo -u postgres psql -v ON_ERROR_STOP=1 -tc "SELECT 1 FROM pg_roles WHERE rolname='${DB_USER}'" | grep -q 1 \
  || sudo -u postgres psql -v ON_ERROR_STOP=1 -c "CREATE ROLE ${DB_USER} LOGIN PASSWORD '${DB_PASS}'"
sudo -u postgres psql -v ON_ERROR_STOP=1 -tc "SELECT 1 FROM pg_database WHERE datname='${DB_NAME}'" | grep -q 1 \
  || sudo -u postgres psql -v ON_ERROR_STOP=1 -c "CREATE DATABASE ${DB_NAME} OWNER ${DB_USER}"

# Ortam dosyaları: yalnızca yoksa şablondan oluşturulur, mevcut sırlara dokunulmaz.
umask 077
JWT_SECRET_VALUE="$(openssl rand -hex 32)"
[[ -f "$ETC/api.env" ]] || cat > "$ETC/api.env" <<EOF
DATABASE_URL=postgres://${DB_USER}:${DB_PASS}@127.0.0.1:5432/${DB_NAME}
JWT_SECRET=${JWT_SECRET_VALUE}
BIND_ADDR=127.0.0.1:8080
VARYNTH_API_PUBLIC_URL=https://varynth.zuzia.dev
VARYNTH_TRUSTED_PROXIES=127.0.0.1
VARYNTH_STUB_UPSTREAM=0
VARYNTH_BILLING_STUB=0
VARYNTH_ENABLE_PUBLIC_PLACEHOLDERS=0
EOF
[[ -f "$ETC/web.env" ]] || cat > "$ETC/web.env" <<EOF
API_URL=http://127.0.0.1:8080
NEXT_PUBLIC_SITE_URL=https://varynth.zuzia.dev
EOF
chmod 600 "$ETC"/*.env "$ETC/db.secret"
chown root:varynth "$ETC"/*.env
chmod 640 "$ETC"/*.env

# systemd birimleri deploy.sh tarafından kopyalanır; burada yalnızca yükleme.
systemctl daemon-reload || true

echo "Bootstrap tamam. JWT_SECRET ve DB şifresi otomatik üretildi. Sağlayıcı anahtarlarını $ETC/api.env içine ekleyip deploy.sh çalıştırın."
