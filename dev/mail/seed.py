#!/usr/bin/env python3
"""Seed the local GreenMail server with the corpus and generated messages.

Uses only the Python standard library. Idempotent: if alice's INBOX already
contains "Seed message 0499", the script prints a note and exits unless
--force is passed.

``--dovecot-maildir DIR`` writes the 50,000-message mailbox straight into a
Maildir tree (plus the corpus) for the Dovecot service. It does not talk to
IMAP. It skips when the last generated file is already present, unless
--force is passed.
"""

from __future__ import annotations

import argparse
import calendar
import os
import smtplib
import time
from email.message import EmailMessage
from email.utils import formatdate
from pathlib import Path

import imaplib

HOST = os.environ.get("WBMC_IMAP_HOST", "127.0.0.1")
SMTP_HOST = os.environ.get("WBMC_SMTP_HOST", HOST)
IMAP_PORT = int(os.environ.get("WBMC_IMAP_PORT", "3143"))
SMTP_PORT = int(os.environ.get("WBMC_SMTP_PORT", "3025"))
USER = "alice@wbmc.test"
PASSWORD = "password"
FROM_ADDR = "alice@wbmc.test"
TO_ADDR = "alice@wbmc.test"
MARKER = "Seed message 0499"
# Corpus fixtures are dated 19 Oct 2026. Seed dates start later so message
# 0499 is the newest by Date as well as by UID.
SEED_DATE_BASE = calendar.timegm(time.strptime("2026-11-01 00:00:00", "%Y-%m-%d %H:%M:%S"))
READY_TIMEOUT_S = 60
CORPUS_DIR = Path(__file__).resolve().parent / "corpus"


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--big",
        action="store_true",
        help="send 50,000 generated messages instead of 500",
    )
    parser.add_argument(
        "--force",
        action="store_true",
        help="send even when INBOX already contains the seed marker",
    )
    parser.add_argument(
        "--dovecot-maildir",
        metavar="DIR",
        help="write 50,000 Maildir files (and the corpus) under DIR instead of SMTP",
    )
    return parser.parse_args()


def connect_imap() -> imaplib.IMAP4:
    deadline = time.time() + READY_TIMEOUT_S
    last: Exception | None = None
    while time.time() < deadline:
        try:
            imap = imaplib.IMAP4(HOST, IMAP_PORT)
            imap.login(USER, PASSWORD)
            return imap
        except (OSError, imaplib.IMAP4.error) as exc:
            last = exc
            time.sleep(0.5)
    raise SystemExit(f"IMAP not ready at {HOST}:{IMAP_PORT}: {last}")


def inbox_has_marker() -> bool:
    imap = connect_imap()
    try:
        status, _ = imap.select("INBOX", readonly=True)
        if status != "OK":
            return False
        status, data = imap.search(None, "SUBJECT", f'"{MARKER}"')
        if status != "OK" or not data or not data[0]:
            return False
        return bool(data[0].strip())
    finally:
        try:
            imap.logout()
        except imaplib.IMAP4.error:
            pass


def connect_smtp() -> smtplib.SMTP:
    deadline = time.time() + READY_TIMEOUT_S
    last: Exception | None = None
    while time.time() < deadline:
        try:
            smtp = smtplib.SMTP(SMTP_HOST, SMTP_PORT, timeout=30)
            try:
                smtp.login(USER, PASSWORD)
            except smtplib.SMTPException as exc:
                print(f"SMTP auth not used: {exc}")
            return smtp
        except OSError as exc:
            last = exc
            time.sleep(0.5)
    raise SystemExit(f"SMTP not ready at {SMTP_HOST}:{SMTP_PORT}: {last}")


def to_crlf(raw: bytes) -> bytes:
    text = raw.replace(b"\r\n", b"\n").replace(b"\r", b"\n")
    if not text.endswith(b"\n"):
        text += b"\n"
    return text.replace(b"\n", b"\r\n")


def generated_message(index: int) -> bytes:
    message = EmailMessage()
    message["From"] = f"Seeder <{FROM_ADDR}>"
    message["To"] = f"Alice <{TO_ADDR}>"
    message["Subject"] = f"Seed message {index:04d}"
    message["Date"] = formatdate(SEED_DATE_BASE + index, localtime=False, usegmt=True)
    message["Message-ID"] = f"<seed-{index:04d}@wbmc.test>"
    message.set_content(f"Seed body {index:04d}.\n")
    return message.as_bytes()


def send_raw(smtp: smtplib.SMTP, raw: bytes) -> smtplib.SMTP:
    try:
        smtp.sendmail(FROM_ADDR, [TO_ADDR], raw)
        return smtp
    except smtplib.SMTPServerDisconnected:
        smtp = connect_smtp()
        smtp.sendmail(FROM_ADDR, [TO_ADDR], raw)
        return smtp


def inbox_count() -> int:
    imap = connect_imap()
    try:
        status, data = imap.select("INBOX", readonly=True)
        if status != "OK" or not data or not data[0]:
            raise SystemExit(f"SELECT INBOX failed: {status} {data}")
        return int(data[0])
    finally:
        try:
            imap.logout()
        except imaplib.IMAP4.error:
            pass


def maildir_name(index: int) -> str:
    return f"{index:06d}.M000000P1.wbmc.test"


def write_dovecot_maildir(root: Path, force: bool) -> None:
    count = 50_000
    home = root / "alice@wbmc.test"
    maildir = home / "Maildir"
    new = maildir / "new"
    for directory in (new, maildir / "cur", maildir / "tmp"):
        directory.mkdir(parents=True, exist_ok=True)
        os.chmod(directory, 0o777)
    for directory in (root, home, maildir):
        os.chmod(directory, 0o777)

    marker = new / maildir_name(count - 1)
    if marker.exists() and not force:
        existing = sum(1 for path in new.iterdir() if path.is_file())
        print(
            f"maildir already contains {marker.name}; skipping "
            f"(pass --force to rewrite). files in new/={existing}"
        )
        return

    corpus = sorted(CORPUS_DIR.glob("*.eml"))
    if not corpus:
        raise SystemExit(f"no corpus messages in {CORPUS_DIR}")

    for index, path in enumerate(corpus):
        dest = new / f"c{index:02d}.M000000P1.wbmc.test"
        dest.write_bytes(to_crlf(path.read_bytes()))
        os.chmod(dest, 0o666)
    for index in range(count):
        dest = new / maildir_name(index)
        dest.write_bytes(generated_message(index))
        os.chmod(dest, 0o666)
        if (index + 1) % 5000 == 0:
            print(f"wrote {index + 1}")

    print(
        f"wrote {len(corpus)} corpus message(s) and {count} generated "
        f"message(s) under {new}"
    )


def main() -> None:
    args = parse_args()
    if args.dovecot_maildir:
        write_dovecot_maildir(Path(args.dovecot_maildir), args.force)
        return

    if not args.force and inbox_has_marker():
        print(
            f"INBOX already contains {MARKER!r}; skipping "
            f"(pass --force to send again). INBOX count={inbox_count()}"
        )
        return

    count = 50_000 if args.big else 500
    corpus = sorted(CORPUS_DIR.glob("*.eml"))
    if not corpus:
        raise SystemExit(f"no corpus messages in {CORPUS_DIR}")

    smtp = connect_smtp()
    sent = 0
    try:
        for path in corpus:
            smtp = send_raw(smtp, to_crlf(path.read_bytes()))
            sent += 1
        for index in range(count):
            smtp = send_raw(smtp, generated_message(index))
            sent += 1
            if sent % 100 == 0:
                print(f"sent {sent}")
    finally:
        try:
            smtp.quit()
        except smtplib.SMTPException:
            pass

    print(
        f"seeded {len(corpus)} corpus message(s) and {count} generated "
        f"message(s); INBOX count={inbox_count()}"
    )


if __name__ == "__main__":
    main()
