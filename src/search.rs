use crate::db::Db;
use crate::document::Document;
use crate::error::MksResult;

/// A single search result.
#[derive(Debug, Clone)]
pub struct SearchResult {
    pub doc_id: String,
    pub title: String,
    pub snippet: Option<String>,
    pub collection: Option<String>,
}

/// Query prefix variants (qmd-inspired).
pub enum QueryKind {
    /// BM25 keyword search (default, or explicit `lex:` prefix)
    Lex(String),
    /// Intent context — used for disambiguation but not searched
    Intent(String),
    /// Vector search placeholder — falls back to BM25 until embedding support is added
    Vec(String),
}

/// Parses a structured query prefix and returns the effective query + kind.
///
/// Supported prefixes:
///   `lex: <query>`    — BM25 keyword search (default)
///   `intent: <text>`  — context hint only, returns no results
///   `vec: <query>`    — semantic search (BM25 fallback until embeddings are wired up)
pub fn parse_query(raw: &str) -> QueryKind {
    let raw = raw.trim();
    if let Some(rest) = raw.strip_prefix("intent:") {
        return QueryKind::Intent(rest.trim().to_string());
    }
    if let Some(rest) = raw.strip_prefix("vec:") {
        return QueryKind::Vec(rest.trim().to_string());
    }
    let query = raw
        .strip_prefix("lex:")
        .map(|s| s.trim())
        .unwrap_or(raw);
    QueryKind::Lex(query.to_string())
}

/// Runs a full-text search against the FTS5 chunks index.
///
/// `query` uses FTS5 query syntax (porter-stemmed, unicode61).
/// Results are returned ordered by BM25 rank (best first).
pub fn fts_search(
    db: &Db,
    query: &str,
    limit: usize,
    collection: Option<&str>,
    with_snippets: bool,
) -> MksResult<Vec<SearchResult>> {
    db.fts_search(query, limit, collection, with_snippets)
}

/// Returns `true` if the document satisfies a `--where` filter expression.
///
/// Expression format: `<field> <op> <value>`
/// Operators: `=`, `!=`, `>`, `<`, `>=`, `<=`
/// Fields: `collection`, `path`, `title`, or any frontmatter key.
///
/// Examples: `collection=papers`, `date>2024-01`, `title!=Untitled`
pub fn where_matches(doc: &Document, expr: &str) -> bool {
    let expr = expr.trim();
    // Longest operators first to avoid ">=" being parsed as ">"
    for op in &[">=", "<=", "!=", ">", "<", "="] {
        if let Some(pos) = expr.find(op) {
            let field = expr[..pos].trim();
            let value = expr[pos + op.len()..].trim();
            let doc_val = field_value(doc, field);
            return compare_values(&doc_val, op, value);
        }
    }
    true // malformed expression — pass through rather than silently drop
}

fn field_value(doc: &Document, field: &str) -> String {
    match field {
        "collection" => doc.collection.clone(),
        "path"       => doc.path.clone(),
        "title"      => doc.title.clone(),
        _ => doc
            .frontmatter
            .get(field)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
    }
}

fn compare_values(actual: &str, op: &str, expected: &str) -> bool {
    // Try numeric comparison first
    if let (Ok(a), Ok(b)) = (actual.parse::<f64>(), expected.parse::<f64>()) {
        return match op {
            "="  => (a - b).abs() < 1e-10,
            "!=" => (a - b).abs() >= 1e-10,
            ">"  => a > b,
            "<"  => a < b,
            ">=" => a >= b,
            "<=" => a <= b,
            _    => false,
        };
    }
    // String comparison
    match op {
        "="  => actual == expected,
        "!=" => actual != expected,
        ">"  => actual > expected,
        "<"  => actual < expected,
        ">=" => actual >= expected,
        "<=" => actual <= expected,
        _    => false,
    }
}
