/// Pure types and extraction logic — no database dependency.
/// Traversal functions (BFS, Dijkstra, god-nodes) live in db.rs as Db methods.
use std::collections::HashSet;
use std::sync::OnceLock;
use serde::{Deserialize, Serialize};

fn wikilink_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"\[\[([^\]|]+)(?:\|[^\]]+)?\]\]").unwrap())
}

fn hashtag_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"(?:^|\s)#([a-zA-Z][a-zA-Z0-9_-]*)").unwrap())
}

fn concept_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"\b([A-Z][a-z]{2,}(?:\s+[A-Z][a-z]{2,}){1,2})\b").unwrap())
}

// ── Node / Edge types ────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeKind {
    Document,
    WikiLink,
    Tag,
    /// Multi-word capitalised phrase appearing ≥2× in a document (INFERRED confidence).
    Concept,
}

impl NodeKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            NodeKind::Document => "document",
            NodeKind::WikiLink => "wikilink",
            NodeKind::Tag => "tag",
            NodeKind::Concept => "concept",
        }
    }
    pub fn from_str(s: &str) -> Self {
        match s {
            "document" => NodeKind::Document,
            "tag" => NodeKind::Tag,
            "concept" => NodeKind::Concept,
            _ => NodeKind::WikiLink,
        }
    }
}

/// How a relationship was established.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Confidence {
    /// Directly observed (WikiLink, tag, heading reference)
    Extracted,
    /// Derived from co-occurrence or structural analysis
    Inferred,
    /// Uncertain; flagged for human review
    Ambiguous,
}

impl Confidence {
    pub fn as_str(&self) -> &'static str {
        match self {
            Confidence::Extracted => "EXTRACTED",
            Confidence::Inferred => "INFERRED",
            Confidence::Ambiguous => "AMBIGUOUS",
        }
    }
    pub fn from_str(s: &str) -> Self {
        match s {
            "INFERRED" => Confidence::Inferred,
            "AMBIGUOUS" => Confidence::Ambiguous,
            _ => Confidence::Extracted,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub id: String,
    pub label: String,
    pub kind: NodeKind,
    pub doc_id: Option<String>,
    pub frequency: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Edge {
    pub source: String,
    pub target: String,
    pub relation: String,
    pub confidence: Confidence,
    pub weight: f64,
    /// The enclosing paragraph where this link appeared — for hop-level explanations.
    pub context: Option<String>,
    pub doc_id: String,
}

/// A node with its total degree — returned by god_nodes().
#[derive(Debug, Clone)]
pub struct GodNode {
    pub node: Node,
    pub degree: usize,
}

/// One hop in a shortest-path result.
#[derive(Debug, Clone)]
pub struct PathHop {
    pub node: Node,
    /// Relation that led to this node (empty for the start node).
    pub relation: String,
}

/// Graph statistics returned by Db::graph_stats().
pub struct GraphStats {
    pub total_nodes: usize,
    pub doc_nodes: usize,
    pub wikilink_nodes: usize,
    pub tag_nodes: usize,
    pub total_edges: usize,
    pub extracted_edges: usize,
    pub inferred_edges: usize,
    pub ambiguous_edges: usize,
    pub isolated_docs: usize,
}

// ── Extraction ────────────────────────────────────────────────────────────────

/// URL-safe slug derived from a label string.
pub fn slug(label: &str) -> String {
    label
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect::<String>()
        .split('_')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("_")
}

/// Returns the paragraph (blank-line delimited block) enclosing `pos` in `text`,
/// truncated to 300 chars.
fn enclosing_paragraph(text: &str, pos: usize) -> Option<String> {
    let start = text[..pos].rfind("\n\n").map(|i| i + 2).unwrap_or(0);
    let end = text[pos..]
        .find("\n\n")
        .map(|i| pos + i)
        .unwrap_or(text.len());
    let para = text[start..end].trim();
    if para.is_empty() {
        None
    } else {
        Some(para.chars().take(300).collect())
    }
}

/// Extracts WikiLink targets with optional enclosing-paragraph context.
/// Handles `[[Target]]` and `[[Target|Display Text]]`.
/// Lines inside code fences are skipped.
pub fn extract_wikilinks(content: &str) -> Vec<(String, Option<String>)> {
    let re = wikilink_re();
    let mut results = Vec::new();
    let mut in_fence = false;
    let mut byte_offset = 0usize;

    for line in content.lines() {
        if line.starts_with("```") {
            in_fence = !in_fence;
        }
        if !in_fence {
            for cap in re.captures_iter(line) {
                let target = cap[1].trim().to_string();
                if !target.is_empty() {
                    let ctx = content
                        .find(&cap[0])
                        .and_then(|pos| enclosing_paragraph(content, pos));
                    results.push((target, ctx));
                }
            }
        }
        byte_offset += line.len() + 1;
    }
    let _ = byte_offset; // suppress unused warning
    results
}

/// Extracts hashtag labels from content (not inside code fences).
pub fn extract_tags(content: &str) -> Vec<String> {
    let re = hashtag_re();
    let mut tags = Vec::new();
    let mut in_fence = false;
    for line in content.lines() {
        if line.starts_with("```") {
            in_fence = !in_fence;
        }
        if !in_fence {
            for cap in re.captures_iter(line) {
                tags.push(cap[1].to_string());
            }
        }
    }
    tags
}

/// Words that are commonly capitalised in English prose but are not domain concepts.
const CONCEPT_STOPWORDS: &[&str] = &[
    "The", "This", "These", "That", "Those", "A", "An",
    "In", "On", "At", "By", "For", "From", "To", "Of", "Into", "Over",
    "And", "Or", "But", "So", "Yet", "Nor",
    "We", "They", "You", "It", "He", "She", "Our", "Their", "Your", "Its",
    "Is", "Are", "Was", "Were", "Be", "Been", "Has", "Have", "Had",
    "Will", "Would", "Could", "Should", "May", "Might", "Must",
    "Not", "No", "All", "Some", "Any", "Each", "Every", "Both", "Either",
];

/// Extracts multi-word capitalised phrases (2–3 words, each 3+ chars) that
/// appear at least twice within `body`. These become `Concept` nodes with
/// `INFERRED` confidence.
pub fn extract_concepts(body: &str) -> Vec<String> {
    use std::collections::HashMap;
    let re = concept_re();
    let mut counts: HashMap<String, usize> = HashMap::new();
    let mut in_fence = false;

    for line in body.lines() {
        if line.starts_with("```") {
            in_fence = !in_fence;
        }
        if in_fence {
            continue;
        }
        for cap in re.captures_iter(line) {
            let phrase = cap[1].to_string();
            // Skip if any word is a stopword
            let is_stop = phrase
                .split_whitespace()
                .any(|w| CONCEPT_STOPWORDS.contains(&w));
            if !is_stop {
                *counts.entry(phrase).or_insert(0) += 1;
            }
        }
    }

    counts
        .into_iter()
        .filter(|(_, n)| *n >= 2)
        .map(|(phrase, _)| phrase)
        .collect()
}

/// Builds the Node and Edge lists for one document.
/// Called during `mks add` and `mks graph build`.
pub fn extract_graph(doc_id: &str, title: &str, body: &str) -> (Vec<Node>, Vec<Edge>) {
    let mut nodes: Vec<Node> = Vec::new();
    let mut edges: Vec<Edge> = Vec::new();

    let doc_node_id = format!("doc_{}", doc_id);
    nodes.push(Node {
        id: doc_node_id.clone(),
        label: title.to_string(),
        kind: NodeKind::Document,
        doc_id: Some(doc_id.to_string()),
        frequency: 1,
    });

    // WikiLinks
    for (target, ctx) in extract_wikilinks(body) {
        let target_id = format!("wikilink_{}", slug(&target));
        nodes.push(Node {
            id: target_id.clone(),
            label: target.clone(),
            kind: NodeKind::WikiLink,
            doc_id: None,
            frequency: 1,
        });
        edges.push(Edge {
            source: doc_node_id.clone(),
            target: target_id,
            relation: "links_to".to_string(),
            confidence: Confidence::Extracted,
            weight: 1.0,
            context: ctx,
            doc_id: doc_id.to_string(),
        });
    }

    // Hashtags
    let mut seen_tags: HashSet<String> = HashSet::new();
    for tag in extract_tags(body) {
        let tag_lower = tag.to_lowercase();
        let tag_id = format!("tag_{}", slug(&tag_lower));
        if seen_tags.insert(tag_id.clone()) {
            nodes.push(Node {
                id: tag_id.clone(),
                label: format!("#{}", tag_lower),
                kind: NodeKind::Tag,
                doc_id: None,
                frequency: 1,
            });
        }
        edges.push(Edge {
            source: doc_node_id.clone(),
            target: tag_id,
            relation: "tagged".to_string(),
            confidence: Confidence::Extracted,
            weight: 0.8,
            context: None,
            doc_id: doc_id.to_string(),
        });
    }

    // Concepts (multi-word capitalised phrases appearing ≥2× in doc)
    let title_lower = title.to_lowercase();
    let mut seen_concepts: HashSet<String> = HashSet::new();
    for concept in extract_concepts(body) {
        // Skip if concept duplicates the document title
        if concept.to_lowercase() == title_lower {
            continue;
        }
        let concept_id = format!("concept_{}", slug(&concept));
        if seen_concepts.insert(concept_id.clone()) {
            nodes.push(Node {
                id: concept_id.clone(),
                label: concept.clone(),
                kind: NodeKind::Concept,
                doc_id: None,
                frequency: 1,
            });
        }
        edges.push(Edge {
            source: doc_node_id.clone(),
            target: concept_id,
            relation: "contains".to_string(),
            confidence: Confidence::Inferred,
            weight: 0.6,
            context: None,
            doc_id: doc_id.to_string(),
        });
    }

    (nodes, edges)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_normalises_label() {
        assert_eq!(slug("Hello World"), "hello_world");
        assert_eq!(slug("Multi--Word"), "multi_word");
    }

    #[test]
    fn extract_wikilinks_basic() {
        let md = "See [[Attention]] and [[Multi-Head Attention|MHA]].";
        let links: Vec<String> = extract_wikilinks(md).into_iter().map(|(t, _)| t).collect();
        assert!(links.contains(&"Attention".to_string()));
        assert!(links.contains(&"Multi-Head Attention".to_string()));
    }

    #[test]
    fn extract_wikilinks_skips_code_fences() {
        let md = "```\n[[NotALink]]\n```\n[[RealLink]]";
        let links: Vec<String> = extract_wikilinks(md).into_iter().map(|(t, _)| t).collect();
        assert!(!links.contains(&"NotALink".to_string()));
        assert!(links.contains(&"RealLink".to_string()));
    }

    #[test]
    fn extract_tags_basic() {
        let md = "This is #rust and #machine-learning content.";
        let tags = extract_tags(md);
        assert!(tags.contains(&"rust".to_string()));
        assert!(tags.contains(&"machine-learning".to_string()));
    }
}
