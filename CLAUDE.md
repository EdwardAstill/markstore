# mks — markstore CLI

## Project purpose

`mks` stores markdown files in a local SQLite database and lets you retrieve them with full-text BM25 search, semantic vector search via Ollama, and knowledge graph traversal. Input can be local files, directories, globs, or `https://` URLs. Everything lives in a single portable `.db` file.

Pairs with `cnv` (convert2): convert PDF → markdown with `cnv`, ingest with `mks`, recall and analyse later.

Inspired by:
- **qmd** (`tobi/qmd`): SQLite+FTS5, scored chunking, `lex:`/`intent:`/`vec:` prefix DSL, content-hash IDs
- **graphify** (`safishamsi/graphify`): confidence tristate (EXTRACTED/INFERRED/AMBIGUOUS), paragraph-level edge context, token-budget BFS, god-node ranking

## Module map

| File | Responsibility |
|---|---|
| `src/cli.rs` | clap `Cli`, `Command`, `GraphCommand` enums |
| `src/error.rs` | `MksError` (thiserror), `MksResult<T>` |
| `src/document.rs` | `Document`, `Chunk`, hashing, frontmatter parsing, chunking — no DB calls |
| `src/graph.rs` | Pure types (`Node`, `Edge`, `NodeKind`, `Confidence`, etc.) + extraction (`extract_graph`, `extract_wikilinks`, `extract_tags`, `extract_concepts`) — no DB calls |
| `src/db.rs` | `Db` struct, all SQL (schema, CRUD, FTS5, graph traversal, embeddings, stats). Also `ingest_file()` and `ingest_content()` |
| `src/search.rs` | `SearchResult`, `parse_query()`, `fts_search()`, `where_matches()` |
| `src/embed.rs` | Ollama embedding API, `cosine_similarity()`, JSON serialisation |
| `src/fetch.rs` | `fetch_url()` — HTTP GET + HTML-to-markdown |
| `src/watch.rs` | `watch_dir()` — filesystem watcher via `notify` |
| `src/main.rs` | CLI dispatch, `resolve_inputs()` for file/dir/glob |

## Data flow

```
mks add <input>
  ├─ URL?  fetch::fetch_url()      → (title, markdown_with_frontmatter)
  │          db::ingest_content()  → same pipeline below
  └─ File  db::ingest_file()
             ├─ sha256_hex()            content hash for dedup
             ├─ content_id()            first 12 hex → doc_id
             ├─ parse_frontmatter()     (HashMap<String,Value>, body)
             ├─ Db::upsert_document()   documents table
             ├─ chunk_document()        ~900-token chunks, 15% overlap
             │    Db::insert_chunks()   chunks_fts (FTS5, porter unicode61)
             └─ extract_graph()
                  Db::upsert_node()  ×N  nodes table
                  Db::insert_edge()  ×N  edges table

mks search <query>
  ├─ lex: / default  Db::fts_search()     FTS5 MATCH + BM25 rank
  ├─ vec:            embed::embed()        Ollama /api/embeddings
  │                  Db::vector_search()   cosine similarity in Rust
  └─ --where-clause         search::where_matches()  post-filter on document fields

mks embed
  └─ Db::find_unembedded_docs() → doc IDs without embeddings
       embed::embed(body)       → Vec<f32> via Ollama
       Db::upsert_embedding()   → embeddings table

mks watch <dir>
  └─ notify::RecommendedWatcher
       Create/Modify → ingest_file()
       Remove        → Db::delete_document() by path lookup

mks graph *
  └─ Db::bfs_neighbors()    BFS, token-budget cap, sorted by degree
     Db::shortest_path()    Dijkstra, undirected
     Db::god_nodes()        all nodes, sorted by degree DESC
     Db::edges_to()         reverse edges (backlinks)
```

## Database schema

```sql
documents    -- full content + title, path, collection, frontmatter_json, content_hash
chunks_fts   -- FTS5 virtual table (porter unicode61), doc_id UNINDEXED
nodes        -- id, label, kind (document/wikilink/tag/concept), frequency
edges        -- source, target, relation, confidence, weight, context, doc_id
embeddings   -- doc_id, chunk_id, vector (JSON float array)
meta         -- key/value store version
```

## Key design decisions

**Content-addressed IDs.** `content_id()` = first 12 hex of SHA-256(raw). Same content always gets the same ID. Re-adding an unchanged file is a no-op. When content changes, old record is found by path and deleted; new record gets a new ID.

**Chunking is two-pass.** Pass 1: count tokens line-by-line, find highest-scored break point in a ±10-line window at each 900-token boundary (H1=100, H2=90, H3=80, code fence=80, blank=20, list item=5; score=0 inside a fence). Pass 2: extend each chunk start backward by 135 tokens (15% overlap). Never cuts inside a code fence.

**FTS5 deletion by UNINDEXED column.** `DELETE FROM chunks_fts WHERE doc_id = ?` does a full scan — FTS5 doesn't index `UNINDEXED` columns. Correct and acceptable at personal-store scale (thousands of documents).

**Graph: three extraction layers.**
- WikiLinks (`[[Target]]`, `[[Target|Display]]`): `links_to` edges, EXTRACTED, weight 1.0. Includes enclosing paragraph as edge context (up to 300 chars).
- Hashtags (`#tag`): `tagged` edges, EXTRACTED, weight 0.8.
- Concept phrases (multi-word Title Case, ≥2 occurrences, stopword-filtered): `contains` edges, INFERRED, weight 0.6. Pure regex, no LLM needed.

**Confidence tristate on edges.** EXTRACTED = directly observed. INFERRED = derived (concept phrases). AMBIGUOUS = reserved for future uncertain relationships. Stored in `edges.confidence`.

**BFS sorted by degree.** `bfs_neighbors()` accumulates until a token budget is reached, then sorts all results by degree descending. If the budget forces truncation, the most-connected nodes survive the cut.

**Dijkstra is undirected.** `shortest_path()` follows both `edges_from` and `edges_to` at each hop so path-finding works regardless of link direction. `backlinks` (`edges_to` only) gives the reverse-link view.

**Vector search in Rust.** All embeddings loaded from `embeddings` table, cosine similarity computed in Rust, top-k returned. No sqlite-vec or any native extension required. One embedding per document stored under `chunk_id = "full"` (document body text).

**URL ingestion uses the URL as path.** `ingest_content()` stores the source URL in `documents.path`. Content deduplication works the same way as for files.

**`--where-clause` is a post-filter.** `where_matches()` runs after FTS5 or vector search. Each result triggers `get_document()` to access frontmatter fields. Fine for personal-scale stores.

## Build

```bash
cargo build --release   # target/release/mks  (first build ~1 min — bundled SQLite)
cargo test
cargo clippy
```

## Common commands

```bash
mks init
mks add notes.md
mks add ./papers/ --collection research
mks add https://example.com/post --collection web
mks add ./vault/ --force
mks watch ./notes/
mks search "attention mechanism"
mks search "lex: transformer" --snippets
mks search "vec: how does attention work"
mks search "training" --where-clause "collection=papers"
mks search "paper" --where-clause "date>2024-01"
mks search "topic" --sort date --limit 20 --offset 20   # page 2
mks search "topic" --format json | jq '.[].id'
mks embed
mks embed --model mxbai-embed-large --force
mks list --collection research
mks list --format json
mks get a1b2c3d4e5f6
mks remove a1b2c3d4e5f6
mks stats
mks optimize
mks graph build
mks graph query "attention" --depth 2
mks graph path "Introduction" "Conclusion"
mks graph neighbors "Attention" --depth 1
mks graph god-nodes --limit 10
mks graph backlinks "Attention"
mks graph report
```

## Extending the graph

Add a new edge type by editing `extract_graph()` in `src/graph.rs`. Use `Confidence::Extracted` for directly-observed relationships, `Confidence::Inferred` for derived ones. No schema changes needed — `edges.relation` is a free string.
