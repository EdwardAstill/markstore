use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "mks",
    about = "Store, search, and build knowledge graphs from markdown documents",
    version
)]
pub struct Cli {
    /// Path to the store database (default: ~/.markstore/store.db)
    #[arg(short, long, global = true)]
    pub store: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Initialise a new store
    Init,

    /// Add markdown files to the store (file, directory, glob, or https:// URL)
    Add {
        /// File path, directory, glob (e.g. "docs/*.md"), or https:// URL
        input: String,

        /// Collection label (default: "default")
        #[arg(short, long, default_value = "default")]
        collection: String,

        /// Re-ingest even if content hash is unchanged
        #[arg(long)]
        force: bool,
    },

    /// Full-text search. Supports lex:/vec:/intent: prefixes and --where filters
    Search {
        query: String,

        /// Maximum results to return
        #[arg(short, long, default_value = "10")]
        limit: usize,

        /// Skip the first N results (for pagination)
        #[arg(long, default_value = "0")]
        offset: usize,

        /// Filter by collection
        #[arg(short, long)]
        collection: Option<String>,

        /// Show snippet context in results
        #[arg(long)]
        snippets: bool,

        /// Filter on document or frontmatter fields, e.g. --where-clause "date>2024-01"
        /// Supports: =  !=  >  <  >=  <=  for collection, path, title, or any frontmatter key
        #[arg(long, value_name = "EXPR")]
        where_clause: Option<String>,

        /// Sort results: relevance (default), date, title
        #[arg(long, default_value = "relevance", value_name = "FIELD")]
        sort: String,

        /// Output format: text (default), json
        #[arg(long, default_value = "text", value_name = "FMT")]
        format: String,
    },

    /// Retrieve a document by ID and print its full content
    Get {
        /// Document ID (12-char hex, shown by add/search/list)
        id: String,
    },

    /// List stored documents, newest first
    List {
        /// Filter by collection
        #[arg(short, long)]
        collection: Option<String>,

        /// Maximum results to return
        #[arg(short, long, default_value = "20")]
        limit: usize,

        /// Skip the first N results (for pagination)
        #[arg(long, default_value = "0")]
        offset: usize,

        /// Output format: text (default), json
        #[arg(long, default_value = "text", value_name = "FMT")]
        format: String,
    },

    /// Run PRAGMA optimize and VACUUM to reclaim space and improve query performance
    Optimize,

    /// Remove a document and all its graph nodes/edges from the store
    Remove {
        /// Document ID
        id: String,
    },

    /// Show store statistics
    Stats,

    /// Watch a directory and auto-reingest .md files as they change
    Watch {
        /// Directory to watch
        dir: String,

        /// Collection to assign newly ingested files
        #[arg(short, long, default_value = "default")]
        collection: String,
    },

    /// Generate or update vector embeddings for all stored documents (requires Ollama)
    Embed {
        /// Ollama model to use for embeddings
        #[arg(long, default_value = "nomic-embed-text")]
        model: String,

        /// Ollama base URL
        #[arg(long, default_value = "http://localhost:11434")]
        base_url: String,

        /// Re-embed even if embeddings already exist
        #[arg(long)]
        force: bool,
    },

    /// Knowledge graph operations
    #[command(subcommand)]
    Graph(GraphCommand),
}

#[derive(Subcommand, Debug)]
pub enum GraphCommand {
    /// Extract (or re-extract) the knowledge graph from all stored documents
    Build {
        /// Re-extract even if graph already exists
        #[arg(long)]
        force: bool,
    },

    /// BFS graph traversal from nodes matching a keyword
    Query {
        text: String,

        /// Maximum BFS depth
        #[arg(short, long, default_value = "2")]
        depth: usize,

        /// Approximate token budget for output
        #[arg(long, default_value = "2000")]
        budget: usize,
    },

    /// Find the shortest path between two nodes (by label)
    Path {
        from: String,
        to: String,
    },

    /// List neighbors of a node (by label or ID)
    Neighbors {
        node: String,

        /// BFS depth
        #[arg(short, long, default_value = "1")]
        depth: usize,
    },

    /// List top nodes by degree (most connected)
    GodNodes {
        #[arg(short, long, default_value = "10")]
        limit: usize,
    },

    /// Show which documents link TO a given node (reverse link lookup)
    Backlinks {
        /// Node label or ID to look up
        node: String,
    },

    /// Print a text summary of the graph
    Report,
}

