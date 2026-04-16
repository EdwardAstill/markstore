mod cli;
mod db;
mod document;
mod embed;
mod error;
mod fetch;
mod graph;
mod search;
mod watch;

use std::path::{Path, PathBuf};
use anyhow::Context;
use clap::Parser;

use cli::{Cli, Command, GraphCommand};
use db::Db;
use search::{parse_query, QueryKind};

fn default_db_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".markstore")
        .join("store.db")
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let db_path = cli.store.unwrap_or_else(default_db_path);

    match cli.command {
        // ── Init ─────────────────────────────────────────────────────────────
        Command::Init => {
            Db::init(&db_path)
                .with_context(|| format!("Failed to initialise store at {}", db_path.display()))?;
            println!("Store initialised at {}", db_path.display());
        }

        // ── Add ───────────────────────────────────────────────────────────────
        Command::Add { input, collection, force } => {
            let db = open_store(&db_path)?;

            // URL ingestion
            if input.starts_with("https://") || input.starts_with("http://") {
                match fetch::fetch_url(&input) {
                    Ok((title, content)) => {
                        match db::ingest_content(&db, &content, &input, &collection, force) {
                            Ok((id, skipped)) => {
                                if skipped {
                                    println!("  skipped  {} (unchanged)", input);
                                } else {
                                    println!("  added  {} ({})", title, id);
                                }
                            }
                            Err(e) => eprintln!("  error  {}: {}", input, e),
                        }
                    }
                    Err(e) => eprintln!("  error fetching {}: {}", input, e),
                }
                return Ok(());
            }

            // File / directory / glob ingestion
            let paths = resolve_inputs(&input)
                .with_context(|| format!("Failed to resolve input '{}'", input))?;

            if paths.is_empty() {
                eprintln!("No markdown files found for '{}'", input);
            }

            let mut added = 0usize;
            let mut skipped = 0usize;
            let mut errors = 0usize;

            for path in &paths {
                match db::ingest_file(&db, path, &collection, force) {
                    Ok((id, was_skipped)) => {
                        if was_skipped {
                            skipped += 1;
                        } else {
                            println!("  added  {} ({})", path.display(), id);
                            added += 1;
                        }
                    }
                    Err(e) => {
                        eprintln!("  error  {}: {}", path.display(), e);
                        errors += 1;
                    }
                }
            }

            println!(
                "{} added, {} skipped (unchanged), {} errors",
                added, skipped, errors
            );
        }

        // ── Search ────────────────────────────────────────────────────────────
        Command::Search { query, limit, offset, collection, snippets, where_clause, sort, format } => {
            let db = open_store(&db_path)?;

            let results = match parse_query(&query) {
                QueryKind::Intent(ctx) => {
                    println!("[intent context — not searched]\n{}", ctx);
                    return Ok(());
                }
                QueryKind::Vec(q) => {
                    // Attempt vector search via Ollama; fall back to BM25 if unavailable.
                    match embed::embed(&q, embed::DEFAULT_MODEL, embed::DEFAULT_BASE_URL) {
                        Some(qvec) => db
                            .vector_search(&qvec, limit, collection.as_deref())
                            .with_context(|| "Vector search failed")?,
                        None => {
                            eprintln!("Warning: Ollama unavailable, falling back to BM25.");
                            search::fts_search(&db, &q, limit, offset, collection.as_deref(), snippets, &sort)
                                .with_context(|| "Search failed")?
                        }
                    }
                }
                QueryKind::Lex(q) => {
                    search::fts_search(&db, &q, limit, offset, collection.as_deref(), snippets, &sort)
                        .with_context(|| "Search failed")?
                }
            };

            // Apply --where post-filter
            let results: Vec<_> = if let Some(expr) = &where_clause {
                results
                    .into_iter()
                    .filter(|r| {
                        db.get_document(&r.doc_id)
                            .map(|doc| search::where_matches(&doc, expr))
                            .unwrap_or(false)
                    })
                    .collect()
            } else {
                results
            };

            if format == "json" {
                let arr: Vec<serde_json::Value> = results.iter().map(|r| {
                    let mut obj = serde_json::json!({
                        "id": r.doc_id,
                        "title": r.title,
                        "collection": r.collection,
                    });
                    if let Some(snip) = &r.snippet {
                        obj["snippet"] = serde_json::Value::String(snip.clone());
                    }
                    obj
                }).collect();
                println!("{}", serde_json::to_string_pretty(&arr)?);
            } else if results.is_empty() {
                println!("No results.");
            } else {
                for r in &results {
                    let col = r.collection.as_deref().unwrap_or("default");
                    println!("[{}] {} ({})", col, r.title, r.doc_id);
                    if let Some(snip) = &r.snippet {
                        println!("    {}", snip);
                    }
                }
            }
        }

        // ── Get ───────────────────────────────────────────────────────────────
        Command::Get { id } => {
            let db = open_store(&db_path)?;
            let doc = db.get_document(&id)
                .with_context(|| format!("Document '{}' not found", id))?;
            println!("{}", doc.content);
        }

        // ── List ──────────────────────────────────────────────────────────────
        Command::List { collection, limit, offset, format } => {
            let db = open_store(&db_path)?;
            // fetch limit+offset then skip
            let fetch = limit + offset;
            let docs = db
                .list_documents(collection.as_deref(), fetch)
                .with_context(|| "Failed to list documents")?;
            let docs: Vec<_> = docs.into_iter().skip(offset).collect();

            if format == "json" {
                let arr: Vec<serde_json::Value> = docs.iter().map(|doc| {
                    serde_json::json!({
                        "id": doc.id,
                        "title": doc.title,
                        "collection": doc.collection,
                        "path": doc.path,
                        "added_at": doc.added_at.to_rfc3339(),
                    })
                }).collect();
                println!("{}", serde_json::to_string_pretty(&arr)?);
            } else if docs.is_empty() {
                println!("No documents.");
            } else {
                for doc in &docs {
                    println!(
                        "[{}] {} — {} ({})",
                        doc.collection,
                        doc.title,
                        doc.added_at.format("%Y-%m-%d"),
                        doc.id,
                    );
                }
            }
        }

        // ── Optimize ──────────────────────────────────────────────────────────
        Command::Optimize => {
            let db = open_store(&db_path)?;
            db.optimize().with_context(|| "Optimize failed")?;
            println!("Store optimized.");
        }

        // ── Remove ────────────────────────────────────────────────────────────
        Command::Remove { id } => {
            let db = open_store(&db_path)?;
            db.delete_document(&id)
                .with_context(|| format!("Failed to remove '{}'", id))?;
            println!("Removed {}", id);
        }

        // ── Stats ─────────────────────────────────────────────────────────────
        Command::Stats => {
            let db = open_store(&db_path)?;
            let s = db.stats().with_context(|| "Failed to get stats")?;
            println!("Documents : {}", s.total_docs);
            println!("Chunks    : {}", s.total_chunks);
            println!("Nodes     : {}", s.total_nodes);
            println!("Edges     : {}", s.total_edges);
            if !s.collections.is_empty() {
                println!("\nBy collection:");
                for (col, count) in &s.collections {
                    println!("  {:4}  {}", count, col);
                }
            }
        }

        // ── Watch ─────────────────────────────────────────────────────────────
        Command::Watch { dir, collection } => {
            let db = open_store(&db_path)?;
            watch::watch_dir(&db, Path::new(&dir), &collection)
                .with_context(|| format!("Watch failed for '{}'", dir))?;
        }

        // ── Embed ─────────────────────────────────────────────────────────────
        Command::Embed { model, base_url, force } => {
            let db = open_store(&db_path)?;
            let doc_ids: Vec<String> = if force {
                db.list_documents(None, usize::MAX)?
                    .into_iter()
                    .map(|d| d.id)
                    .collect()
            } else {
                db.find_unembedded_docs()?
            };

            if doc_ids.is_empty() {
                println!("All documents already embedded. Use --force to re-embed.");
                return Ok(());
            }

            println!("Embedding {} document(s) using {}...", doc_ids.len(), model);
            let mut done = 0usize;
            let mut failed = 0usize;

            for doc_id in &doc_ids {
                match db.get_document(doc_id) {
                    Ok(doc) => {
                        let (_, body) = document::parse_frontmatter(&doc.content);
                        match embed::embed(body, &model, &base_url) {
                            Some(vec) => {
                                if force {
                                    db.delete_embeddings_for_doc(doc_id).ok();
                                }
                                db.upsert_embedding(doc_id, "full", &vec)
                                    .with_context(|| format!("Failed to store embedding for {}", doc_id))?;
                                done += 1;
                                println!("  embedded  {} ({})", doc.title, doc_id);
                            }
                            None => {
                                eprintln!("  failed    {} ({})", doc.title, doc_id);
                                failed += 1;
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("  error     {}: {}", doc_id, e);
                        failed += 1;
                    }
                }
            }
            println!("{} embedded, {} failed.", done, failed);
        }

        // ── Graph ─────────────────────────────────────────────────────────────
        Command::Graph(graph_cmd) => {
            let db = open_store(&db_path)?;
            handle_graph(&db, graph_cmd)?;
        }
    }

    Ok(())
}

// ── Graph subcommands ─────────────────────────────────────────────────────────

fn handle_graph(db: &Db, cmd: GraphCommand) -> anyhow::Result<()> {
    use graph::extract_graph;

    match cmd {
        GraphCommand::Build { force } => {
            let docs = db
                .list_documents(None, usize::MAX)
                .context("Failed to list documents")?;

            let mut rebuilt = 0usize;
            for doc in &docs {
                let doc_node_id = format!("doc_{}", doc.id);
                if !force && db.get_node(&doc_node_id).is_ok() {
                    continue;
                }
                db.delete_graph_for_doc(&doc.id).ok();
                let (_, body) = document::parse_frontmatter(&doc.content);
                let (nodes, edges) = extract_graph(&doc.id, &doc.title, body);
                for node in &nodes { db.upsert_node(node).ok(); }
                for edge in &edges { db.insert_edge(edge).ok(); }
                rebuilt += 1;
            }
            println!("Graph built/updated for {} document(s).", rebuilt);
        }

        GraphCommand::Query { text, depth, budget } => {
            let seeds = db.search_nodes_by_label(&text).context("Node search failed")?;
            if seeds.is_empty() {
                println!("No nodes found matching '{}'.", text);
                return Ok(());
            }
            let nodes = db.bfs_neighbors(&seeds, depth, budget)
                .context("Graph traversal failed")?;
            if nodes.is_empty() {
                println!("No results.");
            } else {
                println!("Subgraph ({} nodes, depth ≤{}):\n", nodes.len(), depth);
                for (node, degree) in &nodes {
                    println!("  [{:3} conn] {:12} — {}", degree, node.kind.as_str(), node.label);
                }
            }
        }

        GraphCommand::Path { from, to } => {
            let hops = db.shortest_path(&from, &to).context("Path search failed")?;
            if hops.is_empty() {
                println!("No path found.");
            } else {
                println!("Path ({} hops):", hops.len().saturating_sub(1));
                for (i, hop) in hops.iter().enumerate() {
                    if i == 0 {
                        println!("  {} [{}]", hop.node.label, hop.node.kind.as_str());
                    } else {
                        println!("  ──[{}]──> {} [{}]", hop.relation, hop.node.label, hop.node.kind.as_str());
                    }
                }
            }
        }

        GraphCommand::Neighbors { node, depth } => {
            let node_id = db.find_node_by_label(&node)
                .with_context(|| format!("Node '{}' not found", node))?;
            let nodes = db.bfs_neighbors(&[node_id], depth, 5000)
                .context("Neighbor traversal failed")?;
            if nodes.is_empty() {
                println!("No neighbors found.");
            } else {
                for (n, degree) in &nodes {
                    println!("[{:3} conn] {:12} — {}", degree, n.kind.as_str(), n.label);
                }
            }
        }

        GraphCommand::GodNodes { limit } => {
            let nodes = db.god_nodes(limit).context("Failed to get god nodes")?;
            if nodes.is_empty() {
                println!("No graph nodes found. Run `mks graph build` first.");
            } else {
                println!("{} most connected nodes:\n", nodes.len());
                for gn in &nodes {
                    println!("  {:4} connections  {:12}  {}", gn.degree, gn.node.kind.as_str(), gn.node.label);
                }
            }
        }

        GraphCommand::Backlinks { node } => {
            let node_id = db.find_node_by_label(&node)
                .with_context(|| format!("Node '{}' not found", node))?;
            let edges = db.edges_to(&node_id).context("Backlink lookup failed")?;
            if edges.is_empty() {
                println!("No backlinks to '{}'.", node);
            } else {
                println!("Backlinks to '{}' ({}):\n", node, edges.len());
                for (source_id, relation, _weight) in &edges {
                    if let Ok(source) = db.get_node(source_id) {
                        println!("  {:12}  {} ──[{}]──>", source.kind.as_str(), source.label, relation);
                    }
                }
            }
        }

        GraphCommand::Report => {
            let report = db.graph_report().context("Failed to generate graph report")?;
            print!("{}", report);
        }
    }

    Ok(())
}

// ── Utilities ─────────────────────────────────────────────────────────────────

fn open_store(db_path: &Path) -> anyhow::Result<Db> {
    Db::open(db_path)
        .with_context(|| format!("Store not found at {} — run `mks init` first", db_path.display()))
}

/// Resolves a file path, directory (all `.md` files), or glob pattern to a list of paths.
fn resolve_inputs(input: &str) -> anyhow::Result<Vec<PathBuf>> {
    let path = Path::new(input);

    if path.is_file() {
        return Ok(vec![path.to_path_buf()]);
    }

    if path.is_dir() {
        let canonical = path.canonicalize()?;
        let pattern = format!("{}/**/*.md", canonical.display());
        let mut files: Vec<PathBuf> = glob::glob(&pattern)?
            .filter_map(|e| e.ok())
            .collect();
        files.sort();
        return Ok(files);
    }

    // Glob
    let mut files: Vec<PathBuf> = glob::glob(input)?
        .filter_map(|e| e.ok())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("md"))
        .collect();
    files.sort();
    Ok(files)
}
