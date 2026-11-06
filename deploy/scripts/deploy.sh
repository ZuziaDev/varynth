#!/usr/bin/env bash
# Yerel makineden çalıştırın (Git Bash). Gerekli: SSH_TARGET (örn. deploy@1.2.3.4)
# ve anahtar tabanlı SSH girişi. Şifre bu betikte hiçbir yerde kullanılmaz.
set -euo pipefail

: "${SSH_TARGET:?SSH_TARGET ayarlayın, örn. SSH_TARGET=root@1.2.3.4}"
SSH_OPTS=(-o BatchMode=yes -o StrictHostKeyChecking=accept-new)
[[ -n "${SSH_KEY:-}" ]] && SSH_OPTS+=(-i "$SSH_KEY")

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
REMOTE_SRC=/opt/varynth/src

echo "==> Kaynak gönderiliyor (target/, node_modules/, .next/, .env* hariç)"
tar -C "$ROOT" \
  --exclude='./target' --exclude='**/target' \
  --exclude='node_modules' --exclude='**/node_modules' \
  --exclude='.next' --exclude='**/.next' \
  --exclude='.env' --exclude='.env.*' \
  --exclude='./.git' --exclude='./.claude' --exclude='./cli' \
  -czf - . \
  | ssh "${SSH_OPTS[@]}" "$SSH_TARGET" "mkdir -p $REMOTE_SRC && tar -xzf - -C $REMOTE_SRC && chown -R varynth:varynth $REMOTE_SRC"

uid="$(ssh "${SSH_OPTS[@]}" "$SSH_TARGET" id -u)"
if [[ "$uid" != "0" ]]; then
  echo "Uzak kullanıcı root olmalı (systemd ve /etc için). SSH_TARGET'ı kontrol edin." >&2
  exit 1
fi

echo "==> Derleniyor ve servisler kuruluyor"
ssh "${SSH_OPTS[@]}" "$SSH_TARGET" bash -s <<'REMOTE'
set -euo pipefail
export PATH="$HOME/.cargo/bin:$PATH"
SRC=/opt/varynth/src

cd "$SRC/api"
cargo build --release --locked

cd "$SRC/web"
npm ci
npm run build
chown -R varynth:varynth "$SRC/web/.next"

cp "$SRC/deploy/systemd/varynth-api.service" /etc/systemd/system/
cp "$SRC/deploy/systemd/varynth-web.service" /etc/systemd/system/
systemctl daemon-reload
systemctl enable varynth-api varynth-web
systemctl restart varynth-api
systemctl restart varynth-web
sleep 3
systemctl is-active varynth-api varynth-web
REMOTE

echo "==> Bitti. Doğrulama: curl -sS https://<alan-adı>/v1/models/public"
