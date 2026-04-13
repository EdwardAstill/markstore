use std::path::Path;
use chrono::Utc;
use rusqlite::{Connection, params};

use crate::document::{Document, Chunk, content_id, sha256_hex, parse_frontmatter, extract_h1};
use crate::graph::{Node, Edge, NodeKind, GodNode, PathHop, GraphStats};
use crate::search::SearchResult;
use crate::error::{MksError, MksResult};

// ── Schema ───────────────────────────────────────────────────────────────────

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS documents (
    id               TEXT PRIMARY KEY,
    path             TEXT NOT NULL,
    collection       TEXT NOT NULL DEFAULT 'default',
    title            TEXT NOT NULL,
    content          TEXT NOT NULL,
    frontmatter_json TEXT NOT NULL DEFAULT '{}',
    added_at         TEXT NOT NULL,
    content_hash     TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_doc_collection ON documents(collection);
CREATE INDEX IF NOT EXISTS idx_doc_hash       ON documents(content_hash);

-- FTS5: porter stemming, unicode61 for Unicode normalisation.
-- doc_id is UNINDEXED (stored but not tokenised).
CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(
    doc_id  UNINDEXED,
    title,
    content,
    tokenize = 'porter unicode61'
);

CREATE TABLE IF NOT EXISTS nodes (
    id        TEXT PRIMARY KEY,
    label     TEXT NOT NULL,
    kind      TEXT NOT NULL,
    doc_id    TEXT,
    frequency INTEGER NOT NULL DEFAULT 1
);
CREATE INDEX IF NOT EXISTS idx_node_kind  ON nodes(kind);
CREATE INDEX IF NOT EXISTS idx_node_label ON nodes(label COLLATE NOCASE);

CREATE TABLE IF NOT EXISTS edges (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    source     TEXT NOT NULL,
    target     TEXT NOT NULL,
    relation   TEXT NOT NULL,
    confidence TEXT NOT NULL DEFAULT 'EXTRACTED',
    weight     REAL NOT NULL DEFAULT 1.0,
    context    TEXT,
    doc_id     TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_edge_source ON edges(source);
CREATE INDEX IF NOT EXISTS idx_edge_target ON edges(target);
CREATE INDEX IF NOT EXISTS idx_edge_doc    ON edges(doc_id);

-- Vector embeddings (one row per chunk, stored as JSON float array).
-- Vector search is done in Rust (cosine similarity); no sqlite-vec extension required.
CREATE TABLE IF NOT EXISTS embeddings (
    doc_id    TEXT NOT NULL,
    chunk_id  TEXT NOT NULL,
    vector    TEXT NOT NULL,  -- JSON array of f32
    PRIMARY KEY (doc_id, chunk_id)
);
CREATE INDEX IF NOT EXISTS idx_emb_doc ON embeddings(doc_id);
"#;

// ── StoreStats ────────────────────────────────────────────────────────────────

pub struct StoreStats {
    pub total_docs: usize,
    pub total_chunks: usize,
    pub total_nodes: usize,
    pub total_edges: usize,
    pub collections: Vec<(String, usize)>,
}

// ── Db ───────────────────────────────────────────────────────────────────────

pub struct Db {
    conn: Connection,
}

impl Db {
    // ── Lifecycle ─────────────────────────────────────────────────────────────

    /// Creates a new store at `db_path`, including any parent directories.
    pub fn init(db_path: &Path) -> MksResult<Self> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(db_path)?;
        conn.execute_batch(SCHEMA)?;
        conn.execute(
            "INSERT OR IGNORE INTO meta(key, value) VALUES ('version', '1')",
            [],
        )?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        Ok(Self { conn })
    }

    /// Opens an existing store. Returns `MksError::NotInitialized` if absent.
    pub fn open(db_path: &Path) -> MksResult<Self> {
        if !db_path.exists() {
            return Err(MksError::NotInitialized(db_path.display().to_string()));
        }
        let conn = Connection::open(db_path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        Ok(Self { conn })
    }

    // ── Documents ─────────────────────────────────────────────────────────────

    /// Returns the document ID if the content hash already exists in the store.
    pub fn find_by_hash(&self, hash: &str) -> MksResult<Option<String>> {
        let result = self.conn.query_row(
            "SELECT id FROM documents WHERE content_hash = ?1 LIMIT 1",
            params![hash],
            |row| row.get::<_, String>(0),
        );
        match result {
            Ok(id) => Ok(Some(id)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(MksError::Db(e)),
        }
    }

    /// Returns the document ID if a document at `path` already exists.
    pub fn find_by_path(&self, path: &str) -> MksResult<Option<String>> {
        let result = self.conn.query_row(
            "SELECT id FROM documents WHERE path = ?1 LIMIT 1",
            params![path],
            |row| row.get::<_, String>(0),
        );
        match result {
            Ok(id) => Ok(Some(id)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(MksError::Db(e)),
        }
    }

    /// Inserts or replaces a document (upsert by ID).
    pub fn upsert_document(&self, doc: &Document) -> MksResult<()> {
        let fm_json = serde_json::to_string(&doc.frontmatter)?;
        self.conn.execute(
            "INSERT OR REPLACE INTO documents(id, path, collection, title, content, frontmatter_json, added_at, content_hash)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                doc.id,
                doc.path,
                doc.collection,
                doc.title,
                doc.content,
                fm_json,
                doc.added_at.to_rfc3339(),
                doc.content_hash,
            ],
        )?;
        Ok(())
    }

    /// Retrieves a document by ID.
    pub fn get_document(&self, id: &str) -> MksResult<Document> {
        let result = self.conn.query_row(
            "SELECT id, path, collection, title, content, frontmatter_json, added_at, content_hash
             FROM documents WHERE id = ?1",
            params![id],
            row_to_document,
        );
        match result {
            Ok(doc) => Ok(doc),
            Err(rusqlite::Error::QueryReturnedNoRows) => Err(MksError::NotFound(id.to_string())),
            Err(e) => Err(MksError::Db(e)),
        }
    }

    /// Lists documents, optionally filtered by collection, newest first.
    pub fn list_documents(
        &self,
        collection: Option<&str>,
        limit: usize,
    ) -> MksResult<Vec<Document>> {
        let (sql, param): (&str, Option<String>) = match collection {
            Some(col) => (
                "SELECT id, path, collection, title, content, frontmatter_json, added_at, content_hash
                 FROM documents WHERE collection = ?1 ORDER BY added_at DESC LIMIT ?2",
                Some(col.to_string()),
            ),
            None => (
                "SELECT id, path, collection, title, content, frontmatter_json, added_at, content_hash
                 FROM documents ORDER BY added_at DESC LIMIT ?1",
                None,
            ),
        };

        if let Some(col) = param {
            let mut stmt = self.conn.prepare(sql)?;
            let docs = stmt
                .query_map(params![col, limit as i64], row_to_document)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(docs)
        } else {
            let mut stmt = self.conn.prepare(sql)?;
            let docs = stmt
                .query_map(params![limit as i64], row_to_document)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(docs)
        }
    }

    /// Deletes a document and all its associated chunks and graph data.
    pub fn delete_document(&self, id: &str) -> MksResult<()> {
        // Check it exists first
        self.get_document(id)?;
        self.delete_chunks_for_doc(id)?;
        self.delete_graph_for_doc(id)?;
        self.conn.execute("DELETE FROM documents WHERE id = ?1", params![id])?;
        Ok(())
    }

    // ── Chunks (FTS) ──────────────────────────────────────────────────────────

    /// Inserts chunks into the FTS5 table.
    pub fn insert_chunks(&self, chunks: &[Chunk]) -> MksResult<()> {
        for chunk in chunks {
            self.conn.execute(
                "INSERT INTO chunks_fts(doc_id, title, content) VALUES (?1, ?2, ?3)",
                params![chunk.doc_id, chunk.title, chunk.content],
            )?;
        }
        Ok(())
    }

    /// Removes all FTS entries for a given document.
    pub fn delete_chunks_for_doc(&self, doc_id: &str) -> MksResult<()> {
        self.conn.execute(
            "DELETE FROM chunks_fts WHERE doc_id = ?1",
            params![doc_id],
        )?;
        Ok(())
    }

    /// Full-text search over chunks using FTS5 BM25 ranking.
    pub fn fts_search(
        &self,
        query: &str,
        limit: usize,
        collection: Option<&str>,
        with_snippets: bool,
    ) -> MksResult<Vec<SearchResult>> {
        let snippet_expr = if with_snippets {
            "snippet(chunks_fts, 2, '**', '**', '…', 20)"
        } else {
            "NULL"
        };

        let sql = format!(
            "SELECT c.doc_id, c.title, {}, d.collection
             FROM chunks_fts c
             JOIN documents d ON d.id = c.doc_id
             WHERE chunks_fts MATCH ?1
             {}
             ORDER BY rank
             LIMIT ?2",
            snippet_expr,
            collection
                .map(|_| "AND d.collection = ?3")
                .unwrap_or(""),
        );

        let mut stmt = self.conn.prepare(&sql)?;
        let results = if let Some(col) = collection {
            stmt.query_map(params![query, limit as i64, col], row_to_search_result)?
                .collect::<rusqlite::Result<Vec<_>>>()?
        } else {
            stmt.query_map(params![query, limit as i64], row_to_search_result)?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };

        // Deduplicate by doc_id (multiple chunks may match; keep highest rank = first)
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let deduped = results
            .into_iter()
            .filter(|r| seen.insert(r.doc_id.clone()))
            .collect();
        Ok(deduped)
    }

    // ── Graph: nodes ──────────────────────────────────────────────────────────

    /// Inserts or increments frequency on a node.
    pub fn upsert_node(&self, node: &Node) -> MksResult<()> {
        self.conn.execute(
            "INSERT INTO nodes(id, label, kind, doc_id, frequency)
             VALUES (?1, ?2, ?3, ?4, 1)
             ON CONFLICT(id) DO UPDATE SET frequency = frequency + 1",
            params![
                node.id,
                node.label,
                node.kind.as_str(),
                node.doc_id,
            ],
        )?;
        Ok(())
    }

    /// Retrieves a node by ID.
    pub fn get_node(&self, id: &str) -> MksResult<Node> {
        let result = self.conn.query_row(
            "SELECT id, label, kind, doc_id, frequency FROM nodes WHERE id = ?1",
            params![id],
            row_to_node,
        );
        match result {
            Ok(n) => Ok(n),
            Err(rusqlite::Error::QueryReturnedNoRows) => Err(MksError::NodeNotFound(id.to_string())),
            Err(e) => Err(MksError::Db(e)),
        }
    }

    /// Finds a node ID by label — exact, then prefix, then contains (case-insensitive).
    pub fn find_node_by_label(&self, label: &str) -> MksResult<String> {
        // Exact
        let r = self.conn.query_row(
            "SELECT id FROM nodes WHERE lower(label) = lower(?1) LIMIT 1",
            params![label],
            |row| row.get::<_, String>(0),
        );
        if let Ok(id) = r {
            return Ok(id);
        }
        // Prefix
        let pattern = format!("{}%", label.to_lowercase());
        let r = self.conn.query_row(
            "SELECT id FROM nodes WHERE lower(label) LIKE ?1 LIMIT 1",
            params![pattern],
            |row| row.get::<_, String>(0),
        );
        if let Ok(id) = r {
            return Ok(id);
        }
        // Contains
        let pattern = format!("%{}%", label.to_lowercase());
        self.conn
            .query_row(
                "SELECT id FROM nodes WHERE lower(label) LIKE ?1 LIMIT 1",
                params![pattern],
                |row| row.get::<_, String>(0),
            )
            .map_err(|_| MksError::NodeNotFound(label.to_string()))
    }

    /// Returns all node IDs whose label contains `text` (case-insensitive).
    pub fn search_nodes_by_label(&self, text: &str) -> MksResult<Vec<String>> {
        let pattern = format!("%{}%", text.to_lowercase());
        let mut stmt = self.conn.prepare(
            "SELECT id FROM nodes WHERE lower(label) LIKE ?1 LIMIT 50",
        )?;
        let ids = stmt
            .query_map(params![pattern], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(ids)
    }

    /// Returns all nodes.
    pub fn all_nodes(&self) -> MksResult<Vec<Node>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, label, kind, doc_id, frequency FROM nodes",
        )?;
        let nodes = stmt
            .query_map([], row_to_node)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(nodes)
    }

    // ── Graph: edges ──────────────────────────────────────────────────────────

    pub fn insert_edge(&self, edge: &Edge) -> MksResult<()> {
        self.conn.execute(
            "INSERT INTO edges(source, target, relation, confidence, weight, context, doc_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                edge.source,
                edge.target,
                edge.relation,
                edge.confidence.as_str(),
                edge.weight,
                edge.context,
                edge.doc_id,
            ],
        )?;
        Ok(())
    }

    /// Returns all edges where `node_id` is the source: (target, relation, weight)
    pub fn edges_from(&self, node_id: &str) -> MksResult<Vec<(String, String, f64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT target, relation, weight FROM edges WHERE source = ?1",
        )?;
        let rows = stmt
            .query_map(params![node_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, f64>(2)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Returns all edges where `node_id` is the target: (source, relation, weight)
    pub fn edges_to(&self, node_id: &str) -> MksResult<Vec<(String, String, f64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT source, relation, weight FROM edges WHERE target = ?1",
        )?;
        let rows = stmt
            .query_map(params![node_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, f64>(2)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Returns total edge count for a node (in + out).
    pub fn node_degree(&self, node_id: &str) -> MksResult<usize> {
        let out: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM edges WHERE source = ?1",
            params![node_id],
            |row| row.get(0),
        )?;
        let inc: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM edges WHERE target = ?1",
            params![node_id],
            |row| row.get(0),
        )?;
        Ok((out + inc) as usize)
    }

    /// Deletes all nodes and edges associated with a document.
    pub fn delete_graph_for_doc(&self, doc_id: &str) -> MksResult<()> {
        let doc_node_id = format!("doc_{}", doc_id);
        self.conn.execute("DELETE FROM edges WHERE doc_id = ?1", params![doc_id])?;
        self.conn.execute("DELETE FROM nodes WHERE id = ?1", params![doc_node_id])?;
        // Remove orphaned wikilink/tag nodes (frequency drops to zero after decrement)
        self.conn.execute(
            "DELETE FROM nodes WHERE frequency <= 1 AND kind != 'document' AND doc_id IS NULL",
            [],
        )?;
        Ok(())
    }

    // ── Graph: stats ──────────────────────────────────────────────────────────

    pub fn graph_stats(&self) -> MksResult<GraphStats> {
        let total_nodes: i64 =
            self.conn.query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))?;
        let doc_nodes: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM nodes WHERE kind = 'document'",
            [], |r| r.get(0),
        )?;
        let wikilink_nodes: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM nodes WHERE kind = 'wikilink'",
            [], |r| r.get(0),
        )?;
        let tag_nodes: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM nodes WHERE kind = 'tag'",
            [], |r| r.get(0),
        )?;
        let total_edges: i64 =
            self.conn.query_row("SELECT COUNT(*) FROM edges", [], |r| r.get(0))?;
        let extracted: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM edges WHERE confidence = 'EXTRACTED'",
            [], |r| r.get(0),
        )?;
        let inferred: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM edges WHERE confidence = 'INFERRED'",
            [], |r| r.get(0),
        )?;
        let ambiguous: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM edges WHERE confidence = 'AMBIGUOUS'",
            [], |r| r.get(0),
        )?;
        let isolated: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM nodes WHERE kind = 'document'
             AND id NOT IN (SELECT DISTINCT source FROM edges)",
            [], |r| r.get(0),
        )?;

        Ok(GraphStats {
            total_nodes: total_nodes as usize,
            doc_nodes: doc_nodes as usize,
            wikilink_nodes: wikilink_nodes as usize,
            tag_nodes: tag_nodes as usize,
            total_edges: total_edges as usize,
            extracted_edges: extracted as usize,
            inferred_edges: inferred as usize,
            ambiguous_edges: ambiguous as usize,
            isolated_docs: isolated as usize,
        })
    }

    // ── Embeddings ────────────────────────────────────────────────────────────

    /// Stores a vector embedding for a chunk. Replaces any existing entry.
    pub fn upsert_embedding(&self, doc_id: &str, chunk_id: &str, vector: &[f32]) -> MksResult<()> {
        let json = crate::embed::vec_to_json(vector);
        self.conn.execute(
            "INSERT OR REPLACE INTO embeddings(doc_id, chunk_id, vector)
             VALUES (?1, ?2, ?3)",
            params![doc_id, chunk_id, json],
        )?;
        Ok(())
    }

    /// Returns doc IDs that have no embeddings yet (for incremental embed runs).
    pub fn find_unembedded_docs(&self) -> MksResult<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT id FROM documents WHERE id NOT IN (SELECT DISTINCT doc_id FROM embeddings)",
        )?;
        let ids = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(ids)
    }

    /// Returns all stored (doc_id, chunk_id, vector) for similarity scoring.
    /// For large corpora you'd want to paginate, but for typical personal stores
    /// loading all embeddings into RAM is acceptable.
    pub fn all_embeddings(&self) -> MksResult<Vec<(String, String, Vec<f32>)>> {
        let mut stmt = self.conn.prepare(
            "SELECT doc_id, chunk_id, vector FROM embeddings",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let parsed = rows
            .into_iter()
            .filter_map(|(doc, chunk, json)| {
                crate::embed::vec_from_json(&json).map(|v| (doc, chunk, v))
            })
            .collect();
        Ok(parsed)
    }

    /// Deletes all embeddings for a document.
    pub fn delete_embeddings_for_doc(&self, doc_id: &str) -> MksResult<()> {
        self.conn.execute("DELETE FROM embeddings WHERE doc_id = ?1", params![doc_id])?;
        Ok(())
    }

    /// Vector similarity search. Returns the top `limit` (doc_id, score) pairs
    /// sorted by cosine similarity to `query_vec`.
    pub fn vector_search(
        &self,
        query_vec: &[f32],
        limit: usize,
        collection: Option<&str>,
    ) -> MksResult<Vec<crate::search::SearchResult>> {
        let all = self.all_embeddings()?;
        if all.is_empty() {
            return Ok(Vec::new());
        }

        // Score all chunks
        let scores: Vec<(String, f32)> = all
            .iter()
            .map(|(doc_id, _chunk_id, vec)| {
                (doc_id.clone(), crate::embed::cosine_similarity(query_vec, vec))
            })
            .collect();

        // Keep highest score per doc_id
        let mut best: std::collections::HashMap<String, f32> = std::collections::HashMap::new();
        for (doc_id, score) in scores {
            let entry = best.entry(doc_id).or_insert(0.0);
            if score > *entry {
                *entry = score;
            }
        }

        let mut ranked: Vec<(String, f32)> = best.into_iter().collect();
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        ranked.truncate(limit * 2); // over-fetch to allow collection filtering

        let mut results = Vec::new();
        for (doc_id, _score) in ranked {
            match self.get_document(&doc_id) {
                Ok(doc) => {
                    if let Some(col) = collection {
                        if doc.collection != col {
                            continue;
                        }
                    }
                    results.push(crate::search::SearchResult {
                        doc_id: doc.id,
                        title: doc.title,
                        snippet: None,
                        collection: Some(doc.collection),
                    });
                    if results.len() >= limit {
                        break;
                    }
                }
                Err(_) => continue,
            }
        }

        Ok(results)
    }

    // ── Graph traversal ───────────────────────────────────────────────────────

    /// BFS from seed node IDs up to `max_depth` hops.
    /// Returns `(Node, degree)` pairs sorted by degree descending, capped at `token_budget`.
    pub fn bfs_neighbors(
        &self,
        seed_ids: &[String],
        max_depth: usize,
        token_budget: usize,
    ) -> MksResult<Vec<(Node, usize)>> {
        use std::collections::VecDeque;
        let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut queue: VecDeque<(String, usize)> = VecDeque::new();
        let mut results: Vec<(Node, usize)> = Vec::new();
        let mut token_count = 0usize;

        for id in seed_ids {
            queue.push_back((id.clone(), 0));
        }

        while let Some((node_id, depth)) = queue.pop_front() {
            if visited.contains(&node_id) || depth > max_depth {
                continue;
            }
            visited.insert(node_id.clone());

            if let Ok(node) = self.get_node(&node_id) {
                let degree = self.node_degree(&node_id).unwrap_or(0);
                token_count += node.label.len() / 4 + 5;
                results.push((node, degree));

                if token_count >= token_budget {
                    break;
                }

                let out = self.edges_from(&node_id).unwrap_or_default();
                let inc = self.edges_to(&node_id).unwrap_or_default();
                for (neighbor, _, _) in out.into_iter().chain(inc.into_iter()) {
                    if !visited.contains(&neighbor) {
                        queue.push_back((neighbor, depth + 1));
                    }
                }
            }
        }

        results.sort_by(|a, b| b.1.cmp(&a.1));
        Ok(results)
    }

    /// Dijkstra shortest path between nodes found by label (undirected traversal).
    pub fn shortest_path(&self, from_label: &str, to_label: &str) -> MksResult<Vec<PathHop>> {
        use std::collections::BinaryHeap;

        #[derive(Eq, PartialEq)]
        struct State { cost: u64, node_id: String }
        impl Ord for State {
            fn cmp(&self, other: &Self) -> std::cmp::Ordering {
                other.cost.cmp(&self.cost).then_with(|| self.node_id.cmp(&other.node_id))
            }
        }
        impl PartialOrd for State {
            fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> { Some(self.cmp(other)) }
        }

        let from_id = self.find_node_by_label(from_label)?;
        let to_id = self.find_node_by_label(to_label)?;

        if from_id == to_id {
            let node = self.get_node(&from_id)?;
            return Ok(vec![PathHop { node, relation: String::new() }]);
        }

        let mut dist: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
        let mut prev: std::collections::HashMap<String, (String, String)> = std::collections::HashMap::new();
        let mut heap: BinaryHeap<State> = BinaryHeap::new();

        dist.insert(from_id.clone(), 0);
        heap.push(State { cost: 0, node_id: from_id.clone() });

        while let Some(State { cost, node_id }) = heap.pop() {
            if node_id == to_id {
                let mut path: Vec<PathHop> = Vec::new();
                let mut cur = to_id.clone();
                while let Some((parent, rel)) = prev.get(&cur) {
                    let node = self.get_node(&cur).unwrap_or_else(|_| Node {
                        id: cur.clone(), label: cur.clone(),
                        kind: NodeKind::WikiLink, doc_id: None, frequency: 1,
                    });
                    path.push(PathHop { node, relation: rel.clone() });
                    cur = parent.clone();
                }
                if let Ok(node) = self.get_node(&cur) {
                    path.push(PathHop { node, relation: String::new() });
                }
                path.reverse();
                return Ok(path);
            }

            if cost > *dist.get(&node_id).unwrap_or(&u64::MAX) {
                continue;
            }

            let out = self.edges_from(&node_id).unwrap_or_default();
            let inc = self.edges_to(&node_id).unwrap_or_default();
            for (neighbor, relation, weight) in out.into_iter().chain(inc.into_iter()) {
                let next_cost = cost.saturating_add((weight * 1000.0) as u64);
                if next_cost < *dist.get(&neighbor).unwrap_or(&u64::MAX) {
                    dist.insert(neighbor.clone(), next_cost);
                    prev.insert(neighbor.clone(), (node_id.clone(), relation));
                    heap.push(State { cost: next_cost, node_id: neighbor });
                }
            }
        }

        Err(MksError::NoPath(from_label.to_string(), to_label.to_string()))
    }

    /// Returns top `limit` nodes by total degree (isolated nodes excluded).
    pub fn god_nodes(&self, limit: usize) -> MksResult<Vec<GodNode>> {
        let nodes = self.all_nodes()?;
        let mut ranked: Vec<GodNode> = nodes
            .into_iter()
            .filter_map(|node| {
                let degree = self.node_degree(&node.id).unwrap_or(0);
                if degree == 0 { None } else { Some(GodNode { node, degree }) }
            })
            .collect();
        ranked.sort_by(|a, b| b.degree.cmp(&a.degree));
        ranked.truncate(limit);
        Ok(ranked)
    }

    /// Generates a plain-text summary of the knowledge graph.
    pub fn graph_report(&self) -> MksResult<String> {
        let stats = self.graph_stats()?;
        let top = self.god_nodes(10)?;
        let mut out = String::new();
        out.push_str("# Knowledge Graph Report\n\n");
        out.push_str(&format!(
            "Nodes  : {}  (documents: {}, wikilinks: {}, tags: {})\n",
            stats.total_nodes, stats.doc_nodes, stats.wikilink_nodes, stats.tag_nodes
        ));
        out.push_str(&format!(
            "Edges  : {}  (extracted: {}, inferred: {}, ambiguous: {})\n\n",
            stats.total_edges, stats.extracted_edges, stats.inferred_edges, stats.ambiguous_edges
        ));
        if !top.is_empty() {
            out.push_str("## Most Connected Nodes\n\n");
            for gn in &top {
                out.push_str(&format!(
                    "  {:4}  {:12}  {}\n",
                    gn.degree, gn.node.kind.as_str(), gn.node.label
                ));
            }
            out.push('\n');
        }
        if stats.isolated_docs > 0 {
            out.push_str(&format!(
                "{} document(s) have no outgoing links.\n", stats.isolated_docs
            ));
        }
        Ok(out)
    }

    // ── Store stats ───────────────────────────────────────────────────────────

    pub fn stats(&self) -> MksResult<StoreStats> {
        let total_docs: i64 =
            self.conn.query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))?;

        // FTS5 row count via shadow table trick
        let total_chunks: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM chunks_fts", [], |r| r.get(0))
            .unwrap_or(0);

        let total_nodes: i64 =
            self.conn.query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))?;
        let total_edges: i64 =
            self.conn.query_row("SELECT COUNT(*) FROM edges", [], |r| r.get(0))?;

        let mut stmt = self.conn.prepare(
            "SELECT collection, COUNT(*) FROM documents GROUP BY collection ORDER BY collection",
        )?;
        let collections = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as usize))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        Ok(StoreStats {
            total_docs: total_docs as usize,
            total_chunks: total_chunks as usize,
            total_nodes: total_nodes as usize,
            total_edges: total_edges as usize,
            collections,
        })
    }
}

// ── Row mappers ───────────────────────────────────────────────────────────────

fn row_to_document(row: &rusqlite::Row<'_>) -> rusqlite::Result<Document> {
    use chrono::DateTime;
    let added_str: String = row.get(6)?;
    let fm_json: String = row.get(5)?;
    Ok(Document {
        id: row.get(0)?,
        path: row.get(1)?,
        collection: row.get(2)?,
        title: row.get(3)?,
        content: row.get(4)?,
        frontmatter: serde_json::from_str(&fm_json).unwrap_or_default(),
        added_at: DateTime::parse_from_rfc3339(&added_str)
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now()),
        content_hash: row.get(7)?,
    })
}

fn row_to_node(row: &rusqlite::Row<'_>) -> rusqlite::Result<Node> {
    Ok(Node {
        id: row.get(0)?,
        label: row.get(1)?,
        kind: NodeKind::from_str(&row.get::<_, String>(2)?),
        doc_id: row.get(3)?,
        frequency: row.get::<_, i64>(4)? as u32,
    })
}

fn row_to_search_result(row: &rusqlite::Row<'_>) -> rusqlite::Result<SearchResult> {
    Ok(SearchResult {
        doc_id: row.get(0)?,
        title: row.get(1)?,
        snippet: row.get(2)?,
        collection: row.get(3)?,
    })
}

// ── Public helpers (called from main.rs) ─────────────────────────────────────

/// Ingests raw markdown content (e.g., fetched from a URL).
/// `path` is stored as the document's path field (typically the source URL).
/// Returns `(doc_id, was_skipped)`.
pub fn ingest_content(
    db: &Db,
    raw: &str,
    path: &str,
    collection: &str,
    force: bool,
) -> MksResult<(String, bool)> {
    let hash = sha256_hex(raw);

    if !force {
        if let Some(existing_id) = db.find_by_hash(&hash)? {
            return Ok((existing_id, true));
        }
    }

    if let Some(old_id) = db.find_by_path(path)? {
        db.delete_document(&old_id)?;
    }

    let id = content_id(raw);
    let (fm, body) = parse_frontmatter(raw);
    let title = fm
        .get("title")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| extract_h1(raw))
        .unwrap_or_else(|| path.to_string());

    let doc = Document {
        id: id.clone(),
        path: path.to_string(),
        collection: collection.to_string(),
        title: title.clone(),
        content: raw.to_string(),
        frontmatter: fm,
        added_at: Utc::now(),
        content_hash: hash,
    };

    db.upsert_document(&doc)?;

    let chunks = crate::document::chunk_document(&id, &title, body);
    db.insert_chunks(&chunks)?;

    let (nodes, edges) = crate::graph::extract_graph(&id, &title, body);
    for node in &nodes { db.upsert_node(node)?; }
    for edge in &edges { db.insert_edge(edge)?; }

    Ok((id, false))
}

/// Ingests a single markdown file into the store.
/// Returns `(doc_id, was_skipped)`.
pub fn ingest_file(
    db: &Db,
    path: &std::path::Path,
    collection: &str,
    force: bool,
) -> MksResult<(String, bool)> {
    let raw = std::fs::read_to_string(path)?;
    let hash = sha256_hex(&raw);

    // Skip if unchanged (unless --force)
    if !force {
        if let Some(existing_id) = db.find_by_hash(&hash)? {
            return Ok((existing_id, true));
        }
    }

    // Remove old record for this path (changed content)
    if let Some(old_id) = db.find_by_path(&path.display().to_string())? {
        db.delete_document(&old_id)?;
    }

    let id = content_id(&raw);
    let (fm, body) = parse_frontmatter(&raw);
    let title = fm
        .get("title")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| extract_h1(&raw))
        .unwrap_or_else(|| {
            path.file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string()
        });

    let doc = Document {
        id: id.clone(),
        path: path.display().to_string(),
        collection: collection.to_string(),
        title: title.clone(),
        content: raw.clone(),
        frontmatter: fm,
        added_at: Utc::now(),
        content_hash: hash,
    };

    db.upsert_document(&doc)?;

    // Chunk and index for FTS
    let chunks = crate::document::chunk_document(&id, &title, body);
    db.insert_chunks(&chunks)?;

    // Extract and store knowledge graph
    let (nodes, edges) = crate::graph::extract_graph(&id, &title, body);
    for node in &nodes {
        db.upsert_node(node)?;
    }
    for edge in &edges {
        db.insert_edge(edge)?;
    }

    Ok((id, false))
}
