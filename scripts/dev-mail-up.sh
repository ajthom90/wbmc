#!/usr/bin/env bash
# Start the wbmc-dev mail containers and seed alice's inbox.
# Only touches the compose project named wbmc-dev.
#
#   scripts/dev-mail-up.sh         GreenMail on 127.0.0.1:3143, 500-message seed
#   scripts/dev-mail-up.sh --big   Dovecot (profile big) on 127.0.0.1:4143,
#                                  50,000 Maildir files written on disk
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

if [[ "${1:-}" == "--big" ]]; then
  if [[ $# -ne 1 ]]; then
    echo "usage: scripts/dev-mail-up.sh [--big]" >&2
    exit 2
  fi
  python3 dev/mail/seed.py --dovecot-maildir "$root/dev/mail/dovecot-data"
  docker compose -p wbmc-dev -f dev/mail/docker-compose.yml --profile big up -d dovecot
  python3 - <<'PY'
import socket
import sys
import time

host = "127.0.0.1"
port = 4143
deadline = time.time() + 60
while time.time() < deadline:
    try:
        with socket.create_connection((host, port), timeout=2):
            sys.exit(0)
    except OSError:
        time.sleep(0.5)
print(f"timed out waiting for {host}:{port}", file=sys.stderr)
sys.exit(1)
PY
  exit 0
fi

if [[ $# -ne 0 ]]; then
  echo "usage: scripts/dev-mail-up.sh [--big]" >&2
  exit 2
fi

docker compose -p wbmc-dev -f dev/mail/docker-compose.yml up -d

python3 - <<'PY'
import socket
import sys
import time

host = "127.0.0.1"
port = 3143
deadline = time.time() + 60
while time.time() < deadline:
    try:
        with socket.create_connection((host, port), timeout=2):
            sys.exit(0)
    except OSError:
        time.sleep(0.5)
print(f"timed out waiting for {host}:{port}", file=sys.stderr)
sys.exit(1)
PY

python3 dev/mail/seed.py
