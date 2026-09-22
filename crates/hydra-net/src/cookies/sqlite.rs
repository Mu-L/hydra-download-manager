// Copyright (C) 2026 Javad Rajabzadeh
// SPDX-License-Identifier: MIT OR Apache-2.0

//! A read-only SQLite table scanner, enough to read a browser's cookie store.
//!
//! Firefox and every Chromium keep their cookies in a SQLite database, so
//! importing them means reading one. The obvious way to do that is to link
//! SQLite, and this crate deliberately does not: `hya-net` has no C dependency
//! and the workspace picked `ring` over `aws-lc` for exactly that reason — a
//! build that needs no C toolchain. Trading that for `SELECT` against two
//! tables whose schema is fixed and public is the wrong way round, so the
//! scanner is here instead.
//!
//! What it does: open a file, walk `sqlite_master` for a named table's root
//! page and `CREATE TABLE` text, then walk that table's b-tree and decode every
//! leaf record, including payloads that overflow onto continuation pages. It
//! reads the write-ahead log too, because Firefox keeps `cookies.sqlite` in WAL
//! mode and a session's newest cookies live there until a checkpoint moves
//! them.
//!
//! What it deliberately does not do: no expressions, no indexes, no joins, no
//! writes of any kind. Column selection happens here, in Rust, after a full
//! scan of one small table. Cookie stores are hundreds of rows.
//!
//! The file format is documented at <https://sqlite.org/fileformat2.html>;
//! section numbers in the comments below refer to it.

use std::fmt;
use std::path::Path;

/// Largest database this will read into memory.
///
/// A cookie store is single-digit megabytes. The cap is not a tuning knob, it
/// is what stops a mistyped `--cookies-from-browser` path from reading a
/// hundred-gigabyte file into RAM before discovering it has no `cookies` table.
const MAX_DB_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    /// The file is not a SQLite database, or is one this scanner cannot read.
    Format(String),
    /// The database has no such table.
    NoTable(String),
    /// The table has no such column.
    NoColumn {
        table: String,
        column: String,
    },
    TooLarge(u64),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::Format(why) => write!(f, "not a readable SQLite database: {why}"),
            Error::NoTable(t) => write!(f, "no table {t:?} in this database"),
            Error::NoColumn { table, column } => {
                write!(f, "table {table:?} has no column {column:?}")
            }
            Error::TooLarge(n) => write!(f, "database is {n} bytes, refusing to read it"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

/// One column of one row.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl Value {
    pub fn as_str(&self) -> &str {
        match self {
            Value::Text(s) => s,
            _ => "",
        }
    }

    pub fn as_int(&self) -> i64 {
        match self {
            Value::Int(i) => *i,
            Value::Real(f) => *f as i64,
            Value::Text(s) => s.parse().unwrap_or(0),
            _ => 0,
        }
    }

    pub fn as_bytes(&self) -> &[u8] {
        match self {
            Value::Blob(b) => b,
            Value::Text(s) => s.as_bytes(),
            _ => &[],
        }
    }
}

/// A database file, read whole, with its write-ahead log applied.
pub struct Db {
    bytes: Vec<u8>,
    /// Page number -> offset into `wal` of that page's newest committed image.
    wal: Vec<u8>,
    wal_pages: std::collections::HashMap<u32, usize>,
    page_size: usize,
    /// Page size less the per-page reserved region (§1.2): the bytes a payload
    /// may actually occupy.
    usable: usize,
}

impl Db {
    /// Read `path` (and its `-wal` sidecar, when there is one).
    ///
    /// The caller is expected to have copied the file first: a running Chromium
    /// holds an exclusive lock on its own, and this reads bytes rather than
    /// negotiating for them.
    pub fn open(path: &Path) -> Result<Self, Error> {
        let len = std::fs::metadata(path)?.len();
        if len > MAX_DB_BYTES {
            return Err(Error::TooLarge(len));
        }
        let bytes = std::fs::read(path)?;
        // The log is bounded by the same cap for the same reason.
        let wal = match std::fs::metadata(wal_path(path)) {
            Ok(m) if m.len() > MAX_DB_BYTES => return Err(Error::TooLarge(m.len())),
            Ok(_) => std::fs::read(wal_path(path)).unwrap_or_default(),
            Err(_) => Vec::new(),
        };
        Self::from_bytes(bytes, wal)
    }

    fn from_bytes(bytes: Vec<u8>, wal: Vec<u8>) -> Result<Self, Error> {
        if bytes.len() < 100 || !bytes.starts_with(b"SQLite format 3\0") {
            return Err(Error::Format("bad magic".into()));
        }
        // §1.3: a page size of 1 encodes 65536, which does not fit the u16.
        let page_size = match be16(&bytes, 16) {
            1 => 65536,
            n if n >= 512 && n.is_power_of_two() => n as usize,
            n => return Err(Error::Format(format!("page size {n}"))),
        };
        let reserved = bytes[20] as usize;
        if reserved >= page_size {
            return Err(Error::Format("reserved region larger than a page".into()));
        }
        let mut db = Db {
            bytes,
            wal_pages: std::collections::HashMap::new(),
            wal,
            page_size,
            usable: page_size - reserved,
        };
        db.index_wal();
        Ok(db)
    }

    /// Map each page to its newest COMMITTED image in the log.
    ///
    /// §4.2: a frame is a 24-byte header plus one page. The header's third
    /// field is non-zero on the frame that commits a transaction, so frames
    /// after the last commit belong to a transaction that never landed and must
    /// not be read. Salt values that differ from the log header's mark frames
    /// left over from a previous log that was reset in place.
    ///
    /// Checksums are not verified. They guard against a torn write by a
    /// different process; this reads a private copy of a file that SQLite is
    /// not writing, and a wrong page here costs a cookie rather than a database.
    fn index_wal(&mut self) {
        if self.wal.len() < 32 {
            return;
        }
        let magic = be32(&self.wal, 0);
        if magic != 0x377f_0682 && magic != 0x377f_0683 {
            return;
        }
        if be32(&self.wal, 8) as usize != self.page_size {
            return;
        }
        let (salt1, salt2) = (be32(&self.wal, 16), be32(&self.wal, 20));
        let frame = 24 + self.page_size;
        let mut pending: Vec<(u32, usize)> = Vec::new();
        let mut at = 32;
        while at + frame <= self.wal.len() {
            let page_no = be32(&self.wal, at);
            let commit = be32(&self.wal, at + 4);
            if be32(&self.wal, at + 8) != salt1 || be32(&self.wal, at + 12) != salt2 {
                break;
            }
            pending.push((page_no, at + 24));
            if commit != 0 {
                for (p, off) in pending.drain(..) {
                    self.wal_pages.insert(p, off);
                }
            }
            at += frame;
        }
    }

    /// Bytes of page `n` (1-based), from the log when it holds a newer image.
    fn page(&self, n: u32) -> Option<&[u8]> {
        if n == 0 {
            return None;
        }
        if let Some(&off) = self.wal_pages.get(&n) {
            return self.wal.get(off..off + self.page_size);
        }
        let start = (n as usize - 1) * self.page_size;
        self.bytes.get(start..start + self.page_size)
    }

    /// Every row of `table`, projected onto `columns` in that order.
    ///
    /// # Errors
    ///
    /// [`Error::NoTable`] when the database has no such table, and
    /// [`Error::NoColumn`] naming the first requested column the table lacks —
    /// which is how a browser that renamed a column across versions reports
    /// itself, rather than as empty output.
    pub fn rows(&self, table: &str, columns: &[&str]) -> Result<Vec<Vec<Value>>, Error> {
        let (root, sql) = self.table_def(table)?;
        let names = column_names(&sql);
        let idx: Vec<usize> = columns
            .iter()
            .map(|want| {
                names
                    .iter()
                    .position(|n| n.eq_ignore_ascii_case(want))
                    .ok_or_else(|| Error::NoColumn {
                        table: table.to_string(),
                        column: (*want).to_string(),
                    })
            })
            .collect::<Result<_, _>>()?;

        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::new();
        self.walk(root, &mut seen, &mut |record: Vec<Value>| {
            out.push(
                idx.iter()
                    .map(|&i| record.get(i).cloned().unwrap_or(Value::Null))
                    .collect(),
            );
        });
        Ok(out)
    }

    /// Root page and `CREATE TABLE` text for a named table.
    fn table_def(&self, table: &str) -> Result<(u32, String), Error> {
        let mut found = None;
        let mut seen = std::collections::HashSet::new();
        // sqlite_master is always rooted at page 1 (§2.6), and its five columns
        // are fixed: type, name, tbl_name, rootpage, sql.
        self.walk(1, &mut seen, &mut |r: Vec<Value>| {
            if found.is_some() || r.len() < 5 {
                return;
            }
            if r[0].as_str() == "table" && r[1].as_str().eq_ignore_ascii_case(table) {
                found = Some((r[3].as_int() as u32, r[4].as_str().to_string()));
            }
        });
        found.ok_or_else(|| Error::NoTable(table.to_string()))
    }

    /// Depth-first walk of a table b-tree, calling `f` with each leaf record.
    ///
    /// `seen` makes a corrupt file terminate: a page that points at an ancestor
    /// would otherwise be an infinite descent, and this reads files written by
    /// other processes that may have been copied mid-write.
    fn walk(
        &self,
        page_no: u32,
        seen: &mut std::collections::HashSet<u32>,
        f: &mut impl FnMut(Vec<Value>),
    ) {
        if !seen.insert(page_no) {
            return;
        }
        let Some(page) = self.page(page_no) else {
            return;
        };
        // §1.6: page 1 carries the 100-byte file header before its b-tree header.
        let hdr = if page_no == 1 { 100 } else { 0 };
        let Some(&kind) = page.get(hdr) else { return };
        let cells = be16(page, hdr + 3) as usize;
        let ptrs = hdr + if kind == 0x05 || kind == 0x02 { 12 } else { 8 };

        if kind == 0x05 {
            for i in 0..cells {
                let Some(at) = cell_offset(page, ptrs, i) else {
                    continue;
                };
                if at + 4 <= page.len() {
                    self.walk(be32(page, at), seen, f);
                }
            }
            // §1.6: the rightmost child hangs off the header, not the cell array.
            self.walk(be32(page, hdr + 8), seen, f);
            return;
        }
        if kind != 0x0d {
            return;
        }
        for i in 0..cells {
            let Some(at) = cell_offset(page, ptrs, i) else {
                continue;
            };
            let Some((size, n1)) = varint(page, at) else {
                continue;
            };
            let Some((_rowid, n2)) = varint(page, at + n1) else {
                continue;
            };
            let body = at + n1 + n2;
            if let Some(payload) = self.payload(page, body, size as usize) {
                f(decode_record(&payload));
            }
        }
    }

    /// A cell's payload, following overflow pages when it does not fit.
    ///
    /// §1.6 gives the spill arithmetic. `X` is how much of a table-leaf payload
    /// may live on the page; past that, `M` bytes stay and the rest goes to a
    /// chain of overflow pages named by a 4-byte pointer after the local part.
    fn payload(&self, page: &[u8], at: usize, size: usize) -> Option<Vec<u8>> {
        let x = self.usable - 35;
        if size <= x {
            return page.get(at..at + size).map(<[u8]>::to_vec);
        }
        let m = ((self.usable - 12) * 32 / 255).saturating_sub(23);
        let k = m + (size - m) % (self.usable - 4);
        let local = if k <= x { k } else { m };
        let mut out = page.get(at..at + local)?.to_vec();
        let mut next = be32(page, at + local);
        // The chain gets the same cycle guard as the tree: an overflow page
        // that names itself would otherwise be followed until `out` reached
        // `size`, and `size` is whatever the varint said.
        let mut seen = std::collections::HashSet::new();
        while next != 0 && out.len() < size {
            if !seen.insert(next) {
                return None;
            }
            let p = self.page(next)?;
            let take = (size - out.len()).min(self.usable - 4);
            out.extend_from_slice(p.get(4..4 + take)?);
            next = be32(p, 0);
        }
        (out.len() == size).then_some(out)
    }
}

/// The `-wal` sidecar beside a database file.
fn wal_path(db: &Path) -> std::path::PathBuf {
    let mut s = db.as_os_str().to_os_string();
    s.push("-wal");
    std::path::PathBuf::from(s)
}

fn cell_offset(page: &[u8], ptrs: usize, i: usize) -> Option<usize> {
    let at = be16(page, ptrs + i * 2) as usize;
    (at > 0 && at < page.len()).then_some(at)
}

fn be16(b: &[u8], at: usize) -> u16 {
    match b.get(at..at + 2) {
        Some(s) => u16::from_be_bytes([s[0], s[1]]),
        None => 0,
    }
}

fn be32(b: &[u8], at: usize) -> u32 {
    match b.get(at..at + 4) {
        Some(s) => u32::from_be_bytes([s[0], s[1], s[2], s[3]]),
        None => 0,
    }
}

/// §2 huffman-ish varint: up to nine bytes, seven bits each, the ninth
/// contributing all eight. Returns the value and how many bytes it took.
fn varint(b: &[u8], at: usize) -> Option<(u64, usize)> {
    let mut v: u64 = 0;
    for i in 0..9 {
        let byte = *b.get(at + i)?;
        if i == 8 {
            return Some((v << 8 | byte as u64, 9));
        }
        v = v << 7 | (byte & 0x7f) as u64;
        if byte & 0x80 == 0 {
            return Some((v, i + 1));
        }
    }
    None
}

/// §2.1 record format: a header of serial types, then the bodies in order.
fn decode_record(rec: &[u8]) -> Vec<Value> {
    let Some((hdr_len, n)) = varint(rec, 0) else {
        return Vec::new();
    };
    let hdr_len = hdr_len as usize;
    if hdr_len > rec.len() {
        return Vec::new();
    }
    let mut types = Vec::new();
    let mut at = n;
    while at < hdr_len {
        let Some((t, used)) = varint(rec, at) else {
            break;
        };
        types.push(t);
        at += used;
    }
    let mut out = Vec::with_capacity(types.len());
    let mut body = hdr_len;
    for t in types {
        let (v, used) = read_value(rec, body, t);
        out.push(v);
        body += used;
    }
    out
}

fn read_value(rec: &[u8], at: usize, serial: u64) -> (Value, usize) {
    let int = |n: usize| -> i64 {
        match rec.get(at..at + n) {
            // Sign-extend from n bytes: SQLite stores integers in the
            // narrowest form that holds them, two's complement.
            Some(s) => s
                .iter()
                .fold(if s[0] & 0x80 != 0 { -1i64 } else { 0 }, |acc, &b| {
                    acc << 8 | b as i64
                }),
            None => 0,
        }
    };
    match serial {
        0 => (Value::Null, 0),
        1 => (Value::Int(int(1)), 1),
        2 => (Value::Int(int(2)), 2),
        3 => (Value::Int(int(3)), 3),
        4 => (Value::Int(int(4)), 4),
        5 => (Value::Int(int(6)), 6),
        6 => (Value::Int(int(8)), 8),
        7 => (
            Value::Real(match rec.get(at..at + 8) {
                Some(s) => f64::from_be_bytes(s.try_into().unwrap_or([0; 8])),
                None => 0.0,
            }),
            8,
        ),
        8 => (Value::Int(0), 0),
        9 => (Value::Int(1), 0),
        n if n >= 12 && n % 2 == 0 => {
            let len = (n as usize - 12) / 2;
            (
                Value::Blob(rec.get(at..at + len).unwrap_or(&[]).to_vec()),
                len,
            )
        }
        n if n >= 13 => {
            let len = (n as usize - 13) / 2;
            (
                Value::Text(String::from_utf8_lossy(rec.get(at..at + len).unwrap_or(&[])).into()),
                len,
            )
        }
        // 10 and 11 are reserved and appear in no released file format.
        _ => (Value::Null, 0),
    }
}

/// Column names, in order, from a `CREATE TABLE` statement.
///
/// Only the leading identifier of each top-level comma-separated item, with
/// table constraints skipped. That is all a browser's cookie schema contains,
/// and anything more would be a SQL parser.
fn column_names(sql: &str) -> Vec<String> {
    let Some(open) = sql.find('(') else {
        return Vec::new();
    };
    let inner = &sql[open + 1..];
    let mut items = Vec::new();
    let mut depth = 0i32;
    let mut cur = String::new();
    for ch in inner.chars() {
        match ch {
            '(' => {
                depth += 1;
                cur.push(ch);
            }
            ')' if depth == 0 => break,
            ')' => {
                depth -= 1;
                cur.push(ch);
            }
            ',' if depth == 0 => items.push(std::mem::take(&mut cur)),
            _ => cur.push(ch),
        }
    }
    items.push(cur);
    items
        .iter()
        .filter_map(|item| {
            let first = item.trim().split([' ', '\t', '\n', '(']).next()?.trim();
            let name = first.trim_matches(['"', '`', '[', ']', '\'']);
            let constraint = matches!(
                name.to_ascii_uppercase().as_str(),
                "CONSTRAINT" | "PRIMARY" | "UNIQUE" | "CHECK" | "FOREIGN" | "KEY"
            );
            (!name.is_empty() && !constraint).then(|| name.to_string())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints_decode_at_every_width() {
        for (bytes, want, used) in [
            (vec![0x00], 0u64, 1),
            (vec![0x7f], 127, 1),
            (vec![0x81, 0x00], 128, 2),
            (vec![0x82, 0x2c], 300, 2),
            (
                vec![0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
                u64::MAX,
                9,
            ),
        ] {
            assert_eq!(varint(&bytes, 0), Some((want, used)), "{bytes:?}");
        }
        assert_eq!(varint(&[0x81], 0), None);
    }

    #[test]
    fn record_decodes_every_serial_type() {
        // A header counts its own length byte: 1 + six serial types = 7.
        let mut rec = vec![7u8, 0, 1, 2, 3 * 2 + 13, 2 * 2 + 12, 9];
        rec.extend_from_slice(&[0xff]); // int8 = -1
        rec.extend_from_slice(&[0x01, 0x00]); // int16 = 256
        rec.extend_from_slice(b"abc");
        rec.extend_from_slice(&[0xde, 0xad]);
        assert_eq!(
            decode_record(&rec),
            vec![
                Value::Null,
                Value::Int(-1),
                Value::Int(256),
                Value::Text("abc".into()),
                Value::Blob(vec![0xde, 0xad]),
                Value::Int(1),
            ]
        );
    }

    #[test]
    fn column_names_skip_table_constraints() {
        let sql = "CREATE TABLE cookies (creation_utc INTEGER NOT NULL, host_key TEXT NOT NULL, \
                   name TEXT NOT NULL, encrypted_value BLOB DEFAULT '', \
                   UNIQUE (host_key, name, path), PRIMARY KEY (creation_utc))";
        assert_eq!(
            column_names(sql),
            ["creation_utc", "host_key", "name", "encrypted_value"]
        );
    }

    #[test]
    fn quoted_and_bracketed_column_names_are_unwrapped() {
        let sql = "CREATE TABLE t (\"id\" INTEGER, [value] TEXT, `name` TEXT)";
        assert_eq!(column_names(sql), ["id", "value", "name"]);
    }

    /// A database written by SQLite itself, not by this test.
    ///
    /// The generator is `scripts/make-cookie-fixtures.py`; the point of
    /// committing the output is that the scanner is checked against the real
    /// file format rather than against a second implementation of it that
    /// would be wrong in the same places.
    fn fixture(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    #[test]
    fn scans_a_table_that_spans_interior_pages_and_overflows_a_payload() {
        let db = Db::open(&fixture("ff.sqlite")).unwrap();
        let rows = db.rows("moz_cookies", &["name", "value", "host"]).unwrap();
        // 404 rows in 512-byte pages cannot fit one leaf, so the walk went
        // through an interior node to find them.
        assert_eq!(rows.len(), 404);
        let long = rows.iter().find(|r| r[0].as_str() == "long").unwrap();
        // 6000 bytes is far past what one 512-byte page holds: the payload
        // followed a chain of overflow pages and came back whole.
        assert_eq!(long[1].as_str().len(), 6000);
        assert!(long[1].as_str().bytes().all(|b| b == b'L'));
        assert!(rows.iter().any(|r| r[2].as_str() == ".example.org"));
    }

    #[test]
    fn the_write_ahead_log_wins_over_the_checkpointed_page() {
        let db = Db::open(&fixture("wal.sqlite")).unwrap();
        let rows = db.rows("moz_cookies", &["name", "value"]).unwrap();
        let get = |n: &str| {
            rows.iter()
                .find(|r| r[0].as_str() == n)
                .map(|r| r[1].as_str().to_string())
        };
        assert_eq!(get("sid").as_deref(), Some("from-the-wal"));
        assert_eq!(get("fresh").as_deref(), Some("only-in-wal"));
    }

    /// The module promises that a corrupt file terminates. The b-tree walk
    /// always did; the overflow chain is the walk that had no guard, and a
    /// page naming itself as its own continuation was followed until `out`
    /// reached whatever size the record claimed.
    #[test]
    fn an_overflow_page_that_names_itself_ends_the_read() {
        let page_size = 512;
        let mut bytes = vec![0u8; page_size * 2];
        bytes[page_size..page_size + 4].copy_from_slice(&2u32.to_be_bytes());
        let db = Db {
            bytes,
            wal: Vec::new(),
            wal_pages: Default::default(),
            page_size,
            usable: page_size,
        };
        // A 1 000 000-byte payload keeps 256 bytes on the page (§1.6's `K`
        // for this page size), so the overflow pointer sits right after them.
        let mut cell = vec![b'x'; 300];
        cell[256..260].copy_from_slice(&2u32.to_be_bytes());
        assert_eq!(db.payload(&cell, 0, 1_000_000), None);
    }

    #[test]
    fn a_missing_table_or_column_is_named_in_the_error() {
        let db = Db::open(&fixture("ff.sqlite")).unwrap();
        let e = db.rows("cookies", &["name"]).unwrap_err().to_string();
        assert!(e.contains("cookies"), "{e}");
        let e = db
            .rows("moz_cookies", &["name", "encrypted_value"])
            .unwrap_err()
            .to_string();
        assert!(e.contains("encrypted_value"), "{e}");
    }

    #[test]
    fn a_file_that_is_not_a_database_is_refused_by_name() {
        let Err(e) = Db::from_bytes(vec![0u8; 200], Vec::new()) else {
            panic!("a file of zeroes was accepted as a database");
        };
        assert!(e.to_string().contains("bad magic"), "{e}");
    }
}
