#!/usr/bin/env bash
# Start the wbmc-dev GreenMail container and seed alice's inbox.
# Only touches the compose project named wbmc-dev.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

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
