#!/usr/bin/env bash
# Runs ON the Pi as root (piped over SSH by .github/workflows/deploy.yml):
# pull + rebuild as the service user, restart, verify health.
set -euo pipefail

sudo -u claude bash -lc 'cd ~/tools/repo-guardian && git pull --ff-only origin main && cargo build --release'
systemctl restart repo-guardian

for _ in $(seq 1 15); do
  sleep 2
  if curl -fsS http://127.0.0.1:8787/health >/dev/null; then
    echo "repo-guardian healthy on $(git -C /home/claude/tools/repo-guardian rev-parse --short HEAD)"
    exit 0
  fi
done

echo "repo-guardian failed its health check" >&2
systemctl status repo-guardian --no-pager >&2 || true
exit 1
