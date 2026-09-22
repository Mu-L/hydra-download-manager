#!/usr/bin/env python3
# Copyright (C) 2026 Javad Rajabzadeh
# SPDX-License-Identifier: GPL-3.0-or-later
"""Regenerate the browser cookie-store fixtures in
crates/hydra-net/tests/fixtures.

The fixtures are written by SQLite itself, deliberately: hya-net reads the
file format with its own scanner (no C dependency, see cookies/sqlite.rs), so
a fixture built by that scanner's own idea of the format would be wrong in
exactly the places the scanner is. Running this needs Python's bundled
sqlite3; the committed output means the test suite does not.

    python3 scripts/make-cookie-fixtures.py
"""

import os
import shutil
import sqlite3
import sys

OUT = os.path.join(
    os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
    "crates", "hydra-net", "tests", "fixtures",
)
# Chromium stores time in the Windows FILETIME base whatever it runs on.
WINDOWS_EPOCH = 11_644_473_600


def fresh(path):
    for suffix in ("", "-wal", "-shm"):
        try:
            os.remove(path + suffix)
        except FileNotFoundError:
            pass
    return sqlite3.connect(path)


def firefox(path):
    """A Firefox store, with a 512-byte page size so the row count forces
    interior b-tree pages, and one 6000-byte value to force payload overflow."""
    con = fresh(path)
    con.execute("PRAGMA page_size=512")
    con.execute("PRAGMA journal_mode=DELETE")
    con.execute("""CREATE TABLE moz_cookies (
      id INTEGER PRIMARY KEY, originAttributes TEXT NOT NULL DEFAULT '',
      name TEXT, value TEXT, host TEXT, path TEXT, expiry INTEGER,
      lastAccessed INTEGER, creationTime INTEGER, isSecure INTEGER,
      isHttpOnly INTEGER, inBrowserElement INTEGER DEFAULT 0,
      sameSite INTEGER DEFAULT 0,
      CONSTRAINT moz_uniqueid UNIQUE (name, host, path, originAttributes))""")
    add = ("INSERT INTO moz_cookies (id,name,value,host,path,expiry,"
           "lastAccessed,creationTime,isSecure,isHttpOnly) "
           "VALUES (?,?,?,?,?,?,0,0,?,?)")
    for i, row in enumerate([
        ("sid", "abc123", ".example.org", "/", 2000000000, 1, 1),
        ("csrf", "def456", "www.example.org", "/files", 0, 1, 0),
        ("long", "L" * 6000, "big.example.org", "/", 2000000000, 0, 0),
        ("stale", "gone", "example.org", "/", 100, 0, 0),
    ]):
        con.execute(add, (i + 1,) + row)
    for i in range(400):
        con.execute(add, (1000 + i, f"filler{i}", "x" * 50,
                          f"h{i}.filler.test", "/", 2000000000, 0, 0))
    con.commit()
    con.close()


def chromium(path):
    """A Chromium store with the real 17-column schema. Values are left in the
    plaintext column: the decryption path is unit-tested against the cipher
    itself, and a fixture cannot carry a key from the machine that wrote it."""
    con = fresh(path)
    con.execute("PRAGMA page_size=4096")
    con.execute("""CREATE TABLE cookies (
      creation_utc INTEGER NOT NULL, host_key TEXT NOT NULL,
      top_frame_site_key TEXT NOT NULL, name TEXT NOT NULL, value TEXT NOT NULL,
      encrypted_value BLOB NOT NULL, path TEXT NOT NULL,
      expires_utc INTEGER NOT NULL, is_secure INTEGER NOT NULL,
      is_httponly INTEGER NOT NULL, last_access_utc INTEGER NOT NULL,
      has_expires INTEGER NOT NULL, is_persistent INTEGER NOT NULL,
      priority INTEGER NOT NULL, samesite INTEGER NOT NULL,
      source_scheme INTEGER NOT NULL, source_port INTEGER NOT NULL,
      UNIQUE (host_key, top_frame_site_key, name, path, source_scheme,
              source_port))""")
    for host, name, value, p, exp, sec, only in [
        (".example.org", "sid", "abc123", "/", 2000000000, 1, 1),
        ("www.example.org", "csrf", "def456", "/files", None, 0, 0),
        ("example.org", "stale", "gone", "/", 100, 0, 0),
    ]:
        utc = 0 if exp is None else (exp + WINDOWS_EPOCH) * 1_000_000
        con.execute(
            "INSERT INTO cookies VALUES (0,?,'',?,?,X'',?,?,?,?,0,1,1,1,0,2,443)",
            (host, name, value, p, utc, sec, only))
    con.commit()
    con.close()


def wal(path):
    """A store whose newest rows are in an uncheckpointed write-ahead log,
    which is what a RUNNING Firefox looks like on disk."""
    src = path + ".src"
    con = fresh(src)
    con.execute("PRAGMA page_size=4096")
    con.execute("""CREATE TABLE moz_cookies (id INTEGER PRIMARY KEY, name TEXT,
      value TEXT, host TEXT, path TEXT, expiry INTEGER, isSecure INTEGER,
      isHttpOnly INTEGER)""")
    con.execute("INSERT INTO moz_cookies VALUES "
                "(1,'sid','checkpointed','.example.org','/',2000000000,0,0)")
    con.commit()
    con.execute("PRAGMA journal_mode=WAL")
    con.execute("UPDATE moz_cookies SET value='from-the-wal' WHERE name='sid'")
    con.execute("INSERT INTO moz_cookies VALUES "
                "(2,'fresh','only-in-wal','.example.org','/',2000000000,0,0)")
    con.commit()
    # Copied while the connection is still open, so closing cannot checkpoint
    # the log away before the pair is captured.
    shutil.copy(src, path)
    shutil.copy(src + "-wal", path + "-wal")
    con.close()
    for suffix in ("", "-wal", "-shm"):
        try:
            os.remove(src + suffix)
        except FileNotFoundError:
            pass


def main():
    os.makedirs(OUT, exist_ok=True)
    firefox(os.path.join(OUT, "ff.sqlite"))
    chromium(os.path.join(OUT, "ch.sqlite"))
    wal(os.path.join(OUT, "wal.sqlite"))
    for name in sorted(os.listdir(OUT)):
        print(name, os.path.getsize(os.path.join(OUT, name)))


if __name__ == "__main__":
    sys.exit(main())
