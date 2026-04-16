# mks (markstore)

Store markdown files in a local SQLite database and retrieve them with full-text search, semantic vector search, and knowledge graph queries.

Point `mks` at markdown files, a directory, a glob, or an `https://` URL. Everything gets indexed immediately and stays in a single portable file you can back up with `cp`.

Pairs well with `cnv` (convert2): convert a PDF to markdown with `cnv`, then store and query it with `mks`.

---

## Install

```sh
cargo build --release
cp target/release/mks ~/.local/bin/
```

First build takes a minute — `rusqlite` compiles SQLite from source to guarantee FTS5 support.

---

## Quick start

```sh
mks init
mks add notes.md
mks add ./research/ --collection papers
mks search "attention mechanism"
mks list
mks get a1b2c3d4e5f6
```

---

## Storing documents

### Files, directories, and globs

```sh
mks add note.md
mks add ./docs/
mks add "papers/*.md" --collection research
mks add ./vault/ --collection obsidian --force   # re-ingest even if unchanged
```

Every document belongs to a **collection** (default: `"default"`). Collections are just labels — no setup required.

Documents are identified by the first 12 hex characters of their SHA-256 content hash, so:
- Re-adding the same file is a no-op (content unchanged → same hash → skipped)
- Renaming a file without changing its content does not create a duplicate
- When content changes, the old record is replaced and a new ID is assigned

### Web pages

```sh
mks add https://example.com/article
mks add https://arxiv.org/abs/1706.03762 --collection papers
```

The page is fetched, converted from HTML to markdown, and stored like any other document. The URL is stored as the document's path and used for deduplication — re-fetching an unchanged page is a no-op.

### Watch mode

```sh
mks watch ./notes/
mks watch ./vault/ --collection daily
```

Watches a directory recursively. `.md` files are re-ingested when created or modified and removed from the store when deleted. Blocks until Ctrl-C.

---

## Searching

### Full-text search (BM25)

```sh
mks search "attention mechanism"
mks search "transformer" --snippets
mks search "neural network" --limit 20
mks search "learning" --collection papers
```

Uses FTS5 with porter stemming. Supports standard FTS5 syntax:

| Query | Meaning |
|---|---|
| `attention` | Matches "attention", "attentive", etc. (stemmed) |
| `"attention mechanism"` | Exact phrase |
| `attention -distraction` | Must contain "attention", must not contain "distraction" |
| `transf*` | Prefix match |
| `lex: query` | Equivalent to the default — explicit BM25 keyword search |

### Semantic / vector search

```sh
# First, generate embeddings (requires Ollama running locally)
mks embed

# Then search semantically
mks search "vec: how does self-attention work"
mks search "vec: papers about gradient descent" --collection research
```

`vec:` queries embed the query text via Ollama and score all stored documents by cosine similarity. Falls back to BM25 automatically if Ollama is unavailable.

### Filter with `--where-clause`

```sh
mks search "learning" --where-clause "collection=papers"
mks search "training" --where-clause "date>2024-01"
mks search "model" --where-clause "title!=Untitled"
```

Post-filters results by document fields or YAML frontmatter values. Supported fields: `collection`, `path`, `title`, or any frontmatter key. Operators: `=` `!=` `>` `<` `>=` `<=`. Numeric and string comparisons both work.

### Intent context

```sh
mks search "intent: I am looking for papers about transformers, not the electrical component"
```

The `intent:` prefix is shown back to the caller as context but not used as a search term. Useful for disambiguation when building pipelines.

---

## Retrieving documents

```sh
mks get a1b2c3d4e5f6          # print full content
mks list                       # newest first
mks list --collection papers   # filter by collection
mks list --limit 50
mks remove a1b2c3d4e5f6        # delete a document
```

---

## Vector embeddings

```sh
mks embed                                    # embed all unembedded documents
mks embed --model mxbai-embed-large          # use a different Ollama model
mks embed --base-url http://localhost:11434  # custom Ollama URL
mks embed --force                            # re-embed everything
```

Requires [Ollama](https://ollama.ai) running locally with an embedding model pulled:

```sh
ollama pull nomic-embed-text   # default model
```

Embeddings are stored in the SQLite file as compact JSON float arrays and scored in Rust — no native extensions required.

---

## Knowledge graph

The graph is extracted automatically during `mks add`. Three things create graph edges:

| Source | Edge type | Confidence |
|---|---|---|
| `[[WikiLink]]` / `[[WikiLink\|Display]]` | `links_to` | EXTRACTED |
| `#hashtag` | `tagged` | EXTRACTED |
| Multi-word capitalised phrases appearing ≥2× in the document | `contains` | INFERRED |

Node kinds: `document`, `wikilink`, `tag`, `concept`.

```sh
# Rebuild graph for all stored documents
mks graph build
mks graph build --force        # re-extract even if already done

# BFS traversal from nodes matching a keyword
mks graph query "attention"
mks graph query "transformer" --depth 3 --budget 4000

# Shortest path between two nodes (by label)
mks graph path "Introduction" "Conclusion"

# Neighbors of a node
mks graph neighbors "Attention" --depth 2

# Most connected nodes
mks graph god-nodes
mks graph god-nodes --limit 5

# What links TO a given node (reverse lookup)
mks graph backlinks "Attention"

# Summary statistics
mks graph report
```

---

## Store statistics

```sh
mks stats
```

Shows document count, chunk count, graph node/edge counts, and a breakdown by collection.

---

## Store location

```sh
mks init                          # creates ~/.markstore/store.db
mks --store ./local.db init       # custom location
mks --store ./local.db add *.md   # --store works on every command
```

The store is a single SQLite file. Back it up with `cp`.

---

## Collections

Collections are string labels assigned at ingest time. There is no separate creation step.

```sh
mks add ./papers/ --collection research
mks add ./notes/ --collection personal
mks search "learning" --collection research
mks list --collection personal
```

---

## Pairing with cnv

```sh
# Convert a PDF to markdown
cnv paper.pdf -f raw -o /tmp/out/

# Store it
mks add /tmp/out/paper.md --collection papers

# Search and graph
mks search "methodology" --collection papers
mks embed
mks search "vec: experimental results" --collection papers
mks graph build
mks graph query "related work"
```
