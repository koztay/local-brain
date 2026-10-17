//! The per-project knowledge store: a single SQLite database (`.local-brain/brain.db`).
//!
//! Tables: files, symbols, edges (code graph) · chunks + chunks_fts (embedding and lexical
//! index) · summaries · docs · memory · metrics · denials · meta.

use crate::paths::Project;
use crate::util;
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

pub const SCHEMA_VERSION: i64 = 1;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);

CREATE TABLE IF NOT EXISTS files (
    id INTEGER PRIMARY KEY,
    path TEXT NOT NULL UNIQUE,
    lang TEXT,
    kind TEXT NOT NULL,            -- code | text | doc
    hash TEXT NOT NULL,
    size INTEGER NOT NULL,
    lines INTEGER NOT NULL,
    mtime INTEGER NOT NULL,
    stage INTEGER NOT NULL DEFAULT 0,  -- 1 = ast/chunks, 2 = embedded, 3 = summarized
    updated INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS symbols (
    id INTEGER PRIMARY KEY,
    file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    kind TEXT NOT NULL,
    parent TEXT,
    start_line INTEGER NOT NULL,
    end_line INTEGER NOT NULL,
    signature TEXT NOT NULL,
    doc TEXT
);
CREATE INDEX IF NOT EXISTS symbols_name ON symbols(name);
CREATE INDEX IF NOT EXISTS symbols_lname ON symbols(lower(name));
CREATE INDEX IF NOT EXISTS symbols_file ON symbols(file_id);

CREATE TABLE IF NOT EXISTS edges (
    id INTEGER PRIMARY KEY,
    file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    kind TEXT NOT NULL,            -- import | call | inherit
    source TEXT,                   -- enclosing symbol (NULL = file scope)
    target TEXT NOT NULL,
    line INTEGER NOT NULL,
    resolved_file_id INTEGER
);
CREATE INDEX IF NOT EXISTS edges_target ON edges(target);
CREATE INDEX IF NOT EXISTS edges_file ON edges(file_id);
CREATE INDEX IF NOT EXISTS edges_resolved ON edges(resolved_file_id);

CREATE TABLE IF NOT EXISTS chunks (
    id INTEGER PRIMARY KEY,
    file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    symbol TEXT,
    start_line INTEGER NOT NULL,
    end_line INTEGER NOT NULL,
    text TEXT NOT NULL,
    embedding BLOB,
    embed_model TEXT
);
CREATE INDEX IF NOT EXISTS chunks_file ON chunks(file_id);

CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(text, path, symbol, tokenize = 'porter unicode61');

CREATE TABLE IF NOT EXISTS summaries (
    scope TEXT NOT NULL,           -- file | module | project | architecture
    key TEXT NOT NULL,
    text TEXT NOT NULL,
    source_hash TEXT NOT NULL,
    stale INTEGER NOT NULL DEFAULT 0,
    flagged INTEGER NOT NULL DEFAULT 0,
    model TEXT NOT NULL,
    updated INTEGER NOT NULL,
    PRIMARY KEY (scope, key)
);

CREATE TABLE IF NOT EXISTS docs (
    path TEXT PRIMARY KEY,
    hash TEXT NOT NULL,
    kind TEXT NOT NULL,            -- image | pdf
    text TEXT NOT NULL,
    model TEXT NOT NULL,
    updated INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS memory (
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL,            -- decision | change | todo | note | session
    text TEXT NOT NULL,
    why TEXT,
    files TEXT,
    session_id TEXT,
    source TEXT NOT NULL,          -- auto | agent | user
    created INTEGER NOT NULL,
    archived INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS metrics (
    id INTEGER PRIMARY KEY,
    ts INTEGER NOT NULL,
    source TEXT NOT NULL,          -- mcp | hook | cli
    kind TEXT NOT NULL,            -- tool or hook name
    baseline_tokens INTEGER NOT NULL,
    actual_tokens INTEGER NOT NULL,
    session_id TEXT
);
CREATE INDEX IF NOT EXISTS metrics_ts ON metrics(ts);

CREATE TABLE IF NOT EXISTS denials (
    session_id TEXT NOT NULL,
    key TEXT NOT NULL,
    ts INTEGER NOT NULL,
    PRIMARY KEY (session_id, key)
);
"#;

pub struct Store {
    pub conn: Connection,
}

/// Embeddings are stored as int8 with a per-vector scale: `b'Q'`, f32 scale, then one i8 per
/// dimension. That is 4× smaller than f32 with no practical effect on cosine ranking.
pub fn encode_embedding(v: &[f32]) -> Vec<u8> {
    let max = v.iter().fold(0f32, |m, x| m.max(x.abs())).max(1e-12);
    let scale = max / 127.0;
    let mut out = Vec::with_capacity(5 + v.len());
    out.push(b'Q');
    out.extend_from_slice(&scale.to_le_bytes());
    out.extend(
        v.iter()
            .map(|x| ((x / scale).round().clamp(-127.0, 127.0) as i8) as u8),
    );
    out
}

pub fn decode_embedding(b: &[u8]) -> Vec<f32> {
    // Quantized blobs have length 5 + n (never a multiple of 4 for the usual dimensions);
    // older indexes stored raw little-endian f32.
    if b.len() % 4 == 1 && b.first() == Some(&b'Q') {
        let scale = f32::from_le_bytes([b[1], b[2], b[3], b[4]]);
        b[5..].iter().map(|x| (*x as i8) as f32 * scale).collect()
    } else {
        b.chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect()
    }
}

#[derive(Clone, Debug)]
pub struct FileRow {
    pub id: i64,
    pub path: String,
    pub lang: Option<String>,
    pub kind: String,
    pub hash: String,
    pub size: i64,
    pub lines: i64,
    pub stage: i64,
}

/// A symbol row; fields mirror the table even when a caller only needs some of them.
#[allow(dead_code)]
#[derive(Clone, Debug)]
pub struct SymbolRow {
    pub id: i64,
    pub file_id: i64,
    pub path: String,
    pub name: String,
    pub kind: String,
    pub parent: Option<String>,
    pub start_line: i64,
    pub end_line: i64,
    pub signature: String,
    pub doc: Option<String>,
}

#[derive(Clone, Debug)]
pub struct EdgeRow {
    pub path: String,
    pub kind: String,
    pub source: Option<String>,
    pub target: String,
    pub line: i64,
    pub resolved_path: Option<String>,
}

#[cfg(test)]
mod tests {
    #[test]
    fn embedding_roundtrip() {
        let v: Vec<f32> = (0..512).map(|i| ((i as f32) * 0.37).sin()).collect();
        let d = super::decode_embedding(&super::encode_embedding(&v));
        assert_eq!(d.len(), v.len());
        assert!(crate::embed::cosine(&v, &d) / crate::embed::cosine(&v, &v) > 0.999);
    }
}

#[allow(dead_code)]
#[derive(Clone, Debug)]
pub struct ChunkRow {
    pub id: i64,
    pub file_id: i64,
    pub path: String,
    pub symbol: Option<String>,
    pub start_line: i64,
    pub end_line: i64,
    pub text: String,
}

impl Store {
    pub fn open(project: &Project) -> Result<Store> {
        std::fs::create_dir_all(project.dir())?;
        let conn = Connection::open(project.db())
            .with_context(|| format!("opening {}", project.db().display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL; PRAGMA foreign_keys = ON;",
        )?;
        conn.execute_batch(SCHEMA)?;
        let s = Store { conn };
        s.migrate()?;
        if s.meta("schema_version")?.is_none() {
            s.set_meta("schema_version", &SCHEMA_VERSION.to_string())?;
            s.set_meta("created", &util::now().to_string())?;
        }
        Ok(s)
    }

    /// Rebuild the FTS table when an older tokenizer configuration is found.
    fn migrate(&self) -> Result<()> {
        let sql: String = self
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name = 'chunks_fts'",
                [],
                |r| r.get(0),
            )
            .unwrap_or_default();
        if !sql.is_empty() && !sql.contains("porter") {
            self.conn.execute_batch("DROP TABLE chunks_fts;")?;
            self.conn.execute_batch(SCHEMA)?;
            let rows: Vec<(i64, String, String, Option<String>)> = {
                let mut st = self.conn.prepare("SELECT c.id, c.text, f.path, c.symbol FROM chunks c JOIN files f ON f.id = c.file_id")?;
                let r = st
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
                    .flatten()
                    .collect();
                r
            };
            let tx = self.conn.unchecked_transaction()?;
            for (id, text, path, symbol) in rows {
                Self::fts_insert(&self.conn, id, &text, &path, symbol.as_deref())?;
            }
            tx.commit()?;
        }
        Ok(())
    }

    fn fts_insert(
        conn: &Connection,
        id: i64,
        text: &str,
        path: &str,
        symbol: Option<&str>,
    ) -> Result<()> {
        // Path words are indexed too, so "auth middleware" matches src/auth/middleware.ts.
        let path_words = util::split_ident(path).join(" ");
        let sym_words = symbol
            .map(|s| format!("{} {}", s, util::split_ident(s).join(" ")))
            .unwrap_or_default();
        conn.execute(
            "INSERT INTO chunks_fts(rowid, text, path, symbol) VALUES (?, ?, ?, ?)",
            params![id, text, format!("{} {}", path, path_words), sym_words],
        )?;
        Ok(())
    }

    // ---- meta -------------------------------------------------------------------------

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?", [key], |r| r.get(0))
            .optional()?)
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta(key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    // ---- files ------------------------------------------------------------------------

    fn file_from_row(r: &rusqlite::Row) -> rusqlite::Result<FileRow> {
        Ok(FileRow {
            id: r.get(0)?,
            path: r.get(1)?,
            lang: r.get(2)?,
            kind: r.get(3)?,
            hash: r.get(4)?,
            size: r.get(5)?,
            lines: r.get(6)?,
            stage: r.get(7)?,
        })
    }

    pub fn file(&self, path: &str) -> Result<Option<FileRow>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, path, lang, kind, hash, size, lines, stage FROM files WHERE path = ?",
                [path],
                Self::file_from_row,
            )
            .optional()?)
    }

    /// (size, mtime, stage) of an indexed file.
    pub fn file_stat(&self, path: &str) -> Result<Option<(i64, i64, i64)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT size, mtime, stage FROM files WHERE path = ?",
                [path],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?)
    }

    pub fn files(&self) -> Result<Vec<FileRow>> {
        let mut st = self.conn.prepare(
            "SELECT id, path, lang, kind, hash, size, lines, stage FROM files ORDER BY path",
        )?;
        let rows = st.query_map([], Self::file_from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Insert or update a file row; returns its id. Resets its stage when the hash changed.
    #[allow(clippy::too_many_arguments)]
    pub fn upsert_file(
        &self,
        path: &str,
        lang: Option<&str>,
        kind: &str,
        hash: &str,
        size: i64,
        lines: i64,
        mtime: i64,
    ) -> Result<i64> {
        let now = util::now();
        self.conn.execute(
            "INSERT INTO files(path, lang, kind, hash, size, lines, mtime, stage, updated)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, ?8)
             ON CONFLICT(path) DO UPDATE SET
               lang = excluded.lang, kind = excluded.kind, size = excluded.size,
               lines = excluded.lines, mtime = excluded.mtime, updated = excluded.updated,
               stage = CASE WHEN files.hash = excluded.hash THEN files.stage ELSE 0 END,
               hash = excluded.hash",
            params![path, lang, kind, hash, size, lines, mtime, now],
        )?;
        let id: i64 = self
            .conn
            .query_row("SELECT id FROM files WHERE path = ?", [path], |r| r.get(0))?;
        Ok(id)
    }

    pub fn touch_file(&self, file_id: i64, mtime: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE files SET mtime = ? WHERE id = ?",
            params![mtime, file_id],
        )?;
        Ok(())
    }

    pub fn set_stage(&self, file_id: i64, stage: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE files SET stage = ? WHERE id = ?",
            params![stage, file_id],
        )?;
        Ok(())
    }

    pub fn delete_file(&self, path: &str) -> Result<bool> {
        if let Some(f) = self.file(path)? {
            self.clear_file_index(f.id)?;
            self.conn
                .execute("DELETE FROM files WHERE id = ?", [f.id])?;
            self.conn.execute(
                "DELETE FROM summaries WHERE scope = 'file' AND key = ?",
                [path],
            )?;
            self.conn
                .execute("DELETE FROM docs WHERE path = ?", [path])?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Remove symbols, edges and chunks of a file (before re-indexing it).
    pub fn clear_file_index(&self, file_id: i64) -> Result<()> {
        self.conn.execute(
            "DELETE FROM chunks_fts WHERE rowid IN (SELECT id FROM chunks WHERE file_id = ?)",
            [file_id],
        )?;
        self.conn
            .execute("DELETE FROM chunks WHERE file_id = ?", [file_id])?;
        self.conn
            .execute("DELETE FROM symbols WHERE file_id = ?", [file_id])?;
        self.conn
            .execute("DELETE FROM edges WHERE file_id = ?", [file_id])?;
        Ok(())
    }

    // ---- symbols & edges --------------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    pub fn insert_symbol(
        &self,
        file_id: i64,
        name: &str,
        kind: &str,
        parent: Option<&str>,
        start: i64,
        end: i64,
        signature: &str,
        doc: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO symbols(file_id, name, kind, parent, start_line, end_line, signature, doc)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            params![file_id, name, kind, parent, start, end, signature, doc],
        )?;
        Ok(())
    }

    pub fn insert_edge(
        &self,
        file_id: i64,
        kind: &str,
        source: Option<&str>,
        target: &str,
        line: i64,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO edges(file_id, kind, source, target, line) VALUES (?, ?, ?, ?, ?)",
            params![file_id, kind, source, target, line],
        )?;
        Ok(())
    }

    fn sym_from_row(r: &rusqlite::Row) -> rusqlite::Result<SymbolRow> {
        Ok(SymbolRow {
            id: r.get(0)?,
            file_id: r.get(1)?,
            path: r.get(2)?,
            name: r.get(3)?,
            kind: r.get(4)?,
            parent: r.get(5)?,
            start_line: r.get(6)?,
            end_line: r.get(7)?,
            signature: r.get(8)?,
            doc: r.get(9)?,
        })
    }

    const SYM_SELECT: &'static str = "SELECT s.id, s.file_id, f.path, s.name, s.kind, s.parent, s.start_line, s.end_line, s.signature, s.doc FROM symbols s JOIN files f ON f.id = s.file_id";

    pub fn symbols_named(&self, name: &str) -> Result<Vec<SymbolRow>> {
        let sql = format!(
            "{} WHERE s.name = ?1 OR lower(s.name) = lower(?1) ORDER BY (s.name = ?1) DESC, f.path LIMIT 50",
            Self::SYM_SELECT
        );
        let mut st = self.conn.prepare(&sql)?;
        let rows = st.query_map([name], Self::sym_from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Symbols named exactly `name` (case-sensitive) if any exist, else case-insensitive matches.
    pub fn symbols_best(&self, name: &str) -> Result<Vec<SymbolRow>> {
        let all = self.symbols_named(name)?;
        if all.iter().any(|s| s.name == name) {
            Ok(all.into_iter().filter(|s| s.name == name).collect())
        } else {
            Ok(all)
        }
    }

    pub fn symbols_in_file(&self, file_id: i64) -> Result<Vec<SymbolRow>> {
        let sql = format!(
            "{} WHERE s.file_id = ? ORDER BY s.start_line",
            Self::SYM_SELECT
        );
        let mut st = self.conn.prepare(&sql)?;
        let rows = st.query_map([file_id], Self::sym_from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn symbol_names(&self) -> Result<std::collections::HashSet<String>> {
        let mut st = self.conn.prepare("SELECT DISTINCT name FROM symbols")?;
        let rows = st.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    fn edge_query(&self, where_sql: &str, p: &[&dyn rusqlite::ToSql]) -> Result<Vec<EdgeRow>> {
        let sql = format!(
            "SELECT f.path, e.kind, e.source, e.target, e.line, rf.path FROM edges e
             JOIN files f ON f.id = e.file_id LEFT JOIN files rf ON rf.id = e.resolved_file_id
             WHERE {} ORDER BY f.path, e.line LIMIT 500",
            where_sql
        );
        let mut st = self.conn.prepare(&sql)?;
        let rows = st.query_map(p, |r| {
            Ok(EdgeRow {
                path: r.get(0)?,
                kind: r.get(1)?,
                source: r.get(2)?,
                target: r.get(3)?,
                line: r.get(4)?,
                resolved_path: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Call / inherit edges whose target is `name`.
    pub fn edges_to(&self, name: &str, kind: &str) -> Result<Vec<EdgeRow>> {
        self.edge_query("e.target = ?1 AND e.kind = ?2", &[&name, &kind])
    }

    /// Edges originating from a given symbol in a given file.
    pub fn edges_from_symbol(&self, file_id: i64, symbol: &str) -> Result<Vec<EdgeRow>> {
        self.edge_query("e.file_id = ?1 AND e.source = ?2", &[&file_id, &symbol])
    }

    pub fn edges_of_file(&self, file_id: i64) -> Result<Vec<EdgeRow>> {
        self.edge_query("e.file_id = ?1", &[&file_id])
    }

    /// Files whose imports resolve to `file_id`.
    pub fn importers_of(&self, file_id: i64) -> Result<Vec<EdgeRow>> {
        self.edge_query("e.kind = 'import' AND e.resolved_file_id = ?1", &[&file_id])
    }

    // ---- chunks -----------------------------------------------------------------------

    pub fn insert_chunk(
        &self,
        file_id: i64,
        path: &str,
        symbol: Option<&str>,
        start: i64,
        end: i64,
        text: &str,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO chunks(file_id, symbol, start_line, end_line, text) VALUES (?, ?, ?, ?, ?)",
            params![file_id, symbol, start, end, text],
        )?;
        let id = self.conn.last_insert_rowid();
        Self::fts_insert(&self.conn, id, text, path, symbol)?;
        Ok(id)
    }

    pub fn set_chunk_embedding(&self, id: i64, emb: &[f32], model: &str) -> Result<()> {
        let bytes = encode_embedding(emb);
        self.conn.execute(
            "UPDATE chunks SET embedding = ?, embed_model = ? WHERE id = ?",
            params![bytes, model, id],
        )?;
        Ok(())
    }

    fn chunk_from_row(r: &rusqlite::Row) -> rusqlite::Result<ChunkRow> {
        Ok(ChunkRow {
            id: r.get(0)?,
            file_id: r.get(1)?,
            path: r.get(2)?,
            symbol: r.get(3)?,
            start_line: r.get(4)?,
            end_line: r.get(5)?,
            text: r.get(6)?,
        })
    }

    pub fn chunk(&self, id: i64) -> Result<Option<ChunkRow>> {
        Ok(self
            .conn
            .query_row(
                "SELECT c.id, c.file_id, f.path, c.symbol, c.start_line, c.end_line, c.text
                 FROM chunks c JOIN files f ON f.id = c.file_id WHERE c.id = ?",
                [id],
                Self::chunk_from_row,
            )
            .optional()?)
    }

    pub fn chunks_of_file(&self, file_id: i64) -> Result<Vec<ChunkRow>> {
        let mut st = self.conn.prepare(
            "SELECT c.id, c.file_id, f.path, c.symbol, c.start_line, c.end_line, c.text
             FROM chunks c JOIN files f ON f.id = c.file_id WHERE c.file_id = ? ORDER BY c.start_line",
        )?;
        let rows = st.query_map([file_id], Self::chunk_from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Chunks that still need an embedding for `model`.
    pub fn chunks_needing_embedding(&self, model: &str, limit: i64) -> Result<Vec<ChunkRow>> {
        let mut st = self.conn.prepare(
            "SELECT c.id, c.file_id, f.path, c.symbol, c.start_line, c.end_line, c.text
             FROM chunks c JOIN files f ON f.id = c.file_id
             WHERE c.embedding IS NULL OR c.embed_model IS NOT ?1 LIMIT ?2",
        )?;
        let rows = st.query_map(params![model, limit], Self::chunk_from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Full-text search; returns (chunk id, bm25 score — lower is better).
    pub fn fts(&self, query: &str, limit: i64) -> Result<Vec<(i64, f64)>> {
        if query.trim().is_empty() {
            return Ok(vec![]);
        }
        let mut st = self.conn.prepare(
            "SELECT rowid, bm25(chunks_fts, 1.0, 2.0, 3.0) FROM chunks_fts WHERE chunks_fts MATCH ? ORDER BY 2 LIMIT ?",
        )?;
        let rows = st.query_map(params![query, limit], |r| Ok((r.get(0)?, r.get(1)?)));
        match rows {
            Ok(r) => Ok(r.filter_map(|x| x.ok()).collect()),
            Err(_) => Ok(vec![]),
        }
    }

    /// Iterate over all stored embeddings for `model`: (chunk id, file id, vector).
    pub fn embeddings(&self, model: &str) -> Result<Vec<(i64, i64, Vec<f32>)>> {
        let mut st = self.conn.prepare(
            "SELECT id, file_id, embedding FROM chunks WHERE embedding IS NOT NULL AND embed_model = ?",
        )?;
        let rows = st.query_map([model], |r| {
            let b: Vec<u8> = r.get(2)?;
            Ok((r.get(0)?, r.get(1)?, decode_embedding(&b)))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    // ---- summaries --------------------------------------------------------------------

    pub fn summary(&self, scope: &str, key: &str) -> Result<Option<(String, bool, bool, String)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT text, stale, flagged, source_hash FROM summaries WHERE scope = ? AND key = ?",
                params![scope, key],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, i64>(1)? != 0,
                        r.get::<_, i64>(2)? != 0,
                        r.get::<_, String>(3)?,
                    ))
                },
            )
            .optional()?)
    }

    pub fn summary_model(&self, scope: &str, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT model FROM summaries WHERE scope = ? AND key = ?",
                params![scope, key],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn put_summary(
        &self,
        scope: &str,
        key: &str,
        text: &str,
        source_hash: &str,
        flagged: bool,
        model: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO summaries(scope, key, text, source_hash, stale, flagged, model, updated)
             VALUES (?, ?, ?, ?, 0, ?, ?, ?)
             ON CONFLICT(scope, key) DO UPDATE SET text = excluded.text, source_hash = excluded.source_hash,
               stale = 0, flagged = excluded.flagged, model = excluded.model, updated = excluded.updated",
            params![scope, key, text, source_hash, flagged as i64, model, util::now()],
        )?;
        Ok(())
    }

    pub fn mark_stale(&self, scope: &str, key: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE summaries SET stale = 1 WHERE scope = ? AND key = ?",
            params![scope, key],
        )?;
        Ok(())
    }

    // ---- docs -------------------------------------------------------------------------

    pub fn doc(&self, path: &str) -> Result<Option<(String, String, String)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT hash, text, model FROM docs WHERE path = ?",
                [path],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?)
    }

    pub fn put_doc(
        &self,
        path: &str,
        hash: &str,
        kind: &str,
        text: &str,
        model: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO docs(path, hash, kind, text, model, updated) VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT(path) DO UPDATE SET hash = excluded.hash, kind = excluded.kind,
               text = excluded.text, model = excluded.model, updated = excluded.updated",
            params![path, hash, kind, text, model, util::now()],
        )?;
        Ok(())
    }

    // ---- metrics ----------------------------------------------------------------------

    pub fn record_metric(
        &self,
        source: &str,
        kind: &str,
        baseline: i64,
        actual: i64,
        session: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO metrics(ts, source, kind, baseline_tokens, actual_tokens, session_id) VALUES (?, ?, ?, ?, ?, ?)",
            params![util::now(), source, kind, baseline, actual, session],
        )?;
        Ok(())
    }

    /// (calls, baseline tokens, actual tokens) since `since`.
    pub fn metric_totals(&self, since: i64) -> Result<(i64, i64, i64)> {
        Ok(self.conn.query_row(
            "SELECT count(*), coalesce(sum(baseline_tokens), 0), coalesce(sum(actual_tokens), 0) FROM metrics WHERE ts >= ?",
            [since],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?)
    }

    /// (calls, baseline tokens, actual tokens) recorded for one Claude Code session.
    pub fn metric_totals_for_session(&self, session: &str) -> Result<(i64, i64, i64)> {
        Ok(self.conn.query_row(
            "SELECT count(*), coalesce(sum(baseline_tokens), 0), coalesce(sum(actual_tokens), 0) FROM metrics WHERE session_id = ?",
            [session],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?)
    }

    pub fn metric_breakdown(&self, since: i64) -> Result<Vec<(String, i64, i64, i64)>> {
        let mut st = self.conn.prepare(
            "SELECT kind, count(*), sum(baseline_tokens), sum(actual_tokens) FROM metrics
             WHERE ts >= ? GROUP BY kind ORDER BY sum(baseline_tokens) - sum(actual_tokens) DESC",
        )?;
        let rows = st.query_map([since], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    // ---- denials (hook escape hatch) --------------------------------------------------

    /// Returns true if `key` was already denied in this session within `window` seconds.
    pub fn recently_denied(&self, session: &str, key: &str, window: i64) -> Result<bool> {
        let ts: Option<i64> = self
            .conn
            .query_row(
                "SELECT ts FROM denials WHERE session_id = ? AND key = ?",
                params![session, key],
                |r| r.get(0),
            )
            .optional()?;
        Ok(matches!(ts, Some(t) if util::now() - t <= window))
    }

    pub fn record_denial(&self, session: &str, key: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO denials(session_id, key, ts) VALUES (?, ?, ?)
             ON CONFLICT(session_id, key) DO UPDATE SET ts = excluded.ts",
            params![session, key, util::now()],
        )?;
        self.conn
            .execute("DELETE FROM denials WHERE ts < ?", [util::now() - 86_400])?;
        Ok(())
    }

    pub fn clear_denial(&self, session: &str, key: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM denials WHERE session_id = ? AND key = ?",
            params![session, key],
        )?;
        Ok(())
    }

    // ---- stats ------------------------------------------------------------------------

    /// Reclaim space after large rewrites (used after a first full index).
    pub fn compact(&self) -> Result<()> {
        self.conn
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE); VACUUM;")?;
        Ok(())
    }

    pub fn count(&self, sql: &str) -> i64 {
        self.conn.query_row(sql, [], |r| r.get(0)).unwrap_or(0)
    }
}
