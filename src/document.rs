use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

// ── Constants ────────────────────────────────────────────────────────────────

const CHUNK_TOKENS: usize = 900;
const OVERLAP_TOKENS: usize = 135; // ~15 %
const BREAK_WINDOW: usize = 10; // lines either side of target cut

// ── Types ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Document {
    /// First 12 hex chars of SHA-256(content) — stable across renames
    pub id: String,
    pub path: String,
    pub collection: String,
    pub title: String,
    pub content: String,
    pub frontmatter: HashMap<String, serde_json::Value>,
    pub added_at: DateTime<Utc>,
    /// Full SHA-256 hex — used to detect unchanged files
    pub content_hash: String,
}

#[derive(Debug, Clone)]
pub struct Chunk {
    /// doc_id + "_" + chunk_index
    pub id: String,
    pub doc_id: String,
    pub chunk_index: usize,
    /// Document title (indexed in FTS for title-boosted queries)
    pub title: String,
    pub content: String,
}

// ── Hashing ──────────────────────────────────────────────────────────────────

/// Returns the full SHA-256 hex of `content`.
pub fn sha256_hex(content: &str) -> String {
    let mut h = Sha256::new();
    h.update(content.as_bytes());
    hex::encode(h.finalize())
}

/// Returns the first 12 hex chars of SHA-256(content) as the document ID.
pub fn content_id(content: &str) -> String {
    sha256_hex(content)[..12].to_string()
}

// ── Frontmatter ──────────────────────────────────────────────────────────────

/// Parses YAML frontmatter from a markdown string.
/// Returns `(fields, body)` where `body` is the markdown without the `---` block.
pub fn parse_frontmatter(content: &str) -> (HashMap<String, serde_json::Value>, &str) {
    if !content.starts_with("---") {
        return (HashMap::new(), content);
    }
    let rest = &content[3..];
    // Find closing ---
    let end = match rest.find("\n---") {
        Some(i) => i,
        None => return (HashMap::new(), content),
    };
    let yaml = &rest[..end];
    let body = &rest[end + 4..];
    let body = body.strip_prefix('\n').unwrap_or(body);
    let fields: HashMap<String, serde_json::Value> =
        serde_yaml::from_str(yaml).unwrap_or_default();
    (fields, body)
}

/// Extracts the text of the first H1 heading from a markdown string.
pub fn extract_h1(content: &str) -> Option<String> {
    content
        .lines()
        .find(|l| l.starts_with("# "))
        .map(|l| l[2..].trim().to_string())
}

// ── Chunking ─────────────────────────────────────────────────────────────────

fn estimate_tokens(s: &str) -> usize {
    (s.len() / 4).max(1)
}

/// Assigns a break-point score to a line.
/// Higher = better place to cut.  Returns 0 inside code fences (except on the
/// fence marker itself, which scores 80 to allow cutting *at* a fence boundary).
fn break_score(line: &str, in_fence: bool) -> u32 {
    if in_fence {
        return if line.starts_with("```") { 80 } else { 0 };
    }
    if line.starts_with("# ") {
        return 100;
    }
    if line.starts_with("## ") {
        return 90;
    }
    if line.starts_with("### ") {
        return 80;
    }
    if line.starts_with("#### ") {
        return 70;
    }
    if line.starts_with("```") {
        return 80;
    }
    if line.trim() == "---" || line.trim() == "***" {
        return 60;
    }
    if line.trim().is_empty() {
        return 20;
    }
    if line.starts_with("- ")
        || line.starts_with("* ")
        || line.starts_with("+ ")
        || (line.len() > 2 && line.chars().next().map_or(false, |c| c.is_ascii_digit()))
    {
        return 5;
    }
    1
}

fn make_chunk(doc_id: &str, idx: usize, title: &str, content: String) -> Chunk {
    Chunk {
        id: format!("{}_{}", doc_id, idx),
        doc_id: doc_id.to_string(),
        chunk_index: idx,
        title: title.to_string(),
        content,
    }
}

/// Splits a document body into overlapping chunks using scored line break points.
///
/// Algorithm (two-pass):
/// 1. Accumulate token counts line-by-line; when the target chunk size is reached,
///    find the highest-scored break point within ±BREAK_WINDOW lines and record it.
/// 2. Build chunks from the recorded cut points, extending each chunk's start
///    backward by OVERLAP_TOKENS for context continuity.
pub fn chunk_document(doc_id: &str, title: &str, body: &str) -> Vec<Chunk> {
    let lines: Vec<&str> = body.lines().collect();
    let n = lines.len();
    if n == 0 {
        return Vec::new();
    }

    let total_tokens: usize = lines.iter().map(|l| estimate_tokens(l)).sum();
    if total_tokens <= CHUNK_TOKENS {
        return vec![make_chunk(doc_id, 0, title, body.to_string())];
    }

    // Score every line (track code-fence state)
    let mut scores = vec![1u32; n];
    let mut in_fence = false;
    for (i, line) in lines.iter().enumerate() {
        scores[i] = break_score(line, in_fence);
        if line.starts_with("```") {
            in_fence = !in_fence;
        }
    }

    // ── Pass 1: find chunk start positions ───────────────────────────────────
    let mut starts: Vec<usize> = vec![0];
    let mut acc = 0usize;
    let mut prev_cut = 0usize;

    for i in 0..n {
        acc += estimate_tokens(lines[i]);
        if acc >= CHUNK_TOKENS && i + 1 < n {
            let lo = i.saturating_sub(BREAK_WINDOW).max(prev_cut + 1);
            let hi = (i + BREAK_WINDOW).min(n - 1);
            // Best break line in [lo, hi]; cut starts *after* that line
            let best = (lo..=hi).max_by_key(|&j| scores[j]).unwrap_or(i);
            let next = best + 1;
            if next > prev_cut {
                starts.push(next);
                prev_cut = next;
                acc = 0;
            }
        }
    }

    // ── Pass 2: build chunks with backward overlap ───────────────────────────
    let mut chunks = Vec::new();
    for (chunk_idx, &start) in starts.iter().enumerate() {
        let end = starts.get(chunk_idx + 1).copied().unwrap_or(n);
        if start >= end {
            continue;
        }

        // Extend start backward for overlap continuity (skip for first chunk)
        let content_start = if chunk_idx == 0 {
            start
        } else {
            let mut back = start;
            let mut ot = 0usize;
            while back > 0 && ot < OVERLAP_TOKENS {
                back -= 1;
                ot += estimate_tokens(lines[back]);
            }
            back
        };

        let content = lines[content_start..end].join("\n");
        if !content.trim().is_empty() {
            chunks.push(make_chunk(doc_id, chunk_idx, title, content));
        }
    }

    if chunks.is_empty() {
        chunks.push(make_chunk(doc_id, 0, title, body.to_string()));
    }

    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_frontmatter_extracts_title() {
        let md = "---\ntitle: Hello\n---\n# Body";
        let (fm, body) = parse_frontmatter(md);
        assert_eq!(fm.get("title").and_then(|v| v.as_str()), Some("Hello"));
        assert_eq!(body, "# Body");
    }

    #[test]
    fn content_id_is_stable() {
        let id1 = content_id("hello world");
        let id2 = content_id("hello world");
        assert_eq!(id1, id2);
        assert_eq!(id1.len(), 12);
    }

    #[test]
    fn chunk_document_single_chunk_for_short_doc() {
        let body = "Short document.";
        let chunks = chunk_document("abc123", "Title", body);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].content, body);
    }
}
