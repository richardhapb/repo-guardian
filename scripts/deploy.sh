#!/usr/bin/env bash
# Runs on the deploy host as the service user (piped over SSH by
# .github/workflows/deploy.yml): pull + rebuild, restart, verify health.
# The restart is the only step that needs sudo.
set -euo pipefail

export PATH="$HOME/.cargo/bin:$PATH"

cd "$HOME/tools/repo-guardian"
git pull --ff-only origin main
cargo build --release

sudo -n systemctl restart repo-guardian

for _ in $(seq 1 15); do
  sleep 2
  if curl -fsS http://127.0.0.1:8787/health >/dev/null; then
    echo "repo-guardian healthy on $(git rev-parse --short HEAD)"
    exit 0
  fi
done

echo "repo-guardian failed its health check" >&2
systemctl status repo-guardian --no-pager >&2 || true
exit 1
