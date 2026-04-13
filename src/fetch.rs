/// URL ingestion: fetch a web page and convert it to plain markdown.
use chrono::Utc;

/// Fetches a URL and returns `(title, markdown_content_with_frontmatter)`.
pub fn fetch_url(url: &str) -> Result<(String, String), String> {
    let response = ureq::get(url)
        .set("User-Agent", "markstore/0.1 (markdown archiver; +https://github.com)")
        .timeout(std::time::Duration::from_secs(30))
        .call()
        .map_err(|e| e.to_string())?;

    let html = response.into_string().map_err(|e| e.to_string())?;
    let title = extract_title(&html).unwrap_or_else(|| url.to_string());
    let body = html_to_text(&html);

    // Escape YAML special characters in title
    let title_escaped = title.replace('"', "\\\"");

    let content = format!(
        "---\ntitle: \"{}\"\nsource: \"{}\"\nfetched_at: \"{}\"\n---\n\n# {}\n\n{}",
        title_escaped,
        url,
        Utc::now().to_rfc3339(),
        title,
        body,
    );

    Ok((title, content))
}

fn extract_title(html: &str) -> Option<String> {
    // Try <title> tag
    let start = html.to_lowercase().find("<title")?;
    let after_tag = html[start..].find('>')? + start + 1;
    let end = html[after_tag..].to_lowercase().find("</title>")? + after_tag;
    let raw = html[after_tag..end].trim();
    if raw.is_empty() {
        None
    } else {
        Some(decode_entities(raw))
    }
}

/// Strips HTML to readable plain text, preserving headings and paragraphs as
/// blank-line separated blocks.
pub fn html_to_text(html: &str) -> String {
    let mut text = html.to_string();

    // Remove entire blocks we don't want
    for tag in &["script", "style", "nav", "footer", "header", "aside", "noscript"] {
        text = remove_block_tag(&text, tag);
    }

    // Replace semantic block tags with newlines to preserve paragraph structure
    let block_openers: &[(&str, &str)] = &[
        ("<h1", "\n\n# "),
        ("<h2", "\n\n## "),
        ("<h3", "\n\n### "),
        ("<h4", "\n\n#### "),
        ("<h5", "\n\n##### "),
        ("<h6", "\n\n###### "),
        ("<p", "\n\n"),
        ("<br", "\n"),
        ("<li", "\n- "),
        ("<tr", "\n"),
        ("<td", "  "),
        ("<th", "  "),
        ("<blockquote", "\n\n> "),
        ("<hr", "\n\n---\n\n"),
        ("<div", "\n"),
    ];
    for (tag, replacement) in block_openers {
        // Case-insensitive replacement (tag may have attributes)
        let lower = text.to_lowercase();
        let mut out = String::with_capacity(text.len());
        let mut search_start = 0;
        while let Some(pos) = lower[search_start..].find(tag) {
            let abs = search_start + pos;
            out.push_str(&text[search_start..abs]);
            out.push_str(replacement);
            search_start = abs + tag.len();
        }
        out.push_str(&text[search_start..]);
        text = out;
    }

    // Strip all remaining tags
    let mut result = String::with_capacity(text.len());
    let mut in_tag = false;
    for ch in text.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => result.push(ch),
            _ => {}
        }
    }

    // Decode common HTML entities
    result = decode_entities(&result);

    // Normalise whitespace: collapse multiple blank lines, trim each line
    let lines: Vec<&str> = result.lines().map(|l| l.trim()).collect();
    let mut out = String::new();
    let mut blank_run = 0usize;
    for line in &lines {
        if line.is_empty() {
            blank_run += 1;
            if blank_run <= 2 {
                out.push('\n');
            }
        } else {
            blank_run = 0;
            out.push_str(line);
            out.push('\n');
        }
    }

    out.trim().to_string()
}

fn decode_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ")
        .replace("&mdash;", "—")
        .replace("&ndash;", "–")
        .replace("&hellip;", "…")
        .replace("&laquo;", "«")
        .replace("&raquo;", "»")
}

/// Removes everything between `<tag ...>` and `</tag>` (case-insensitive).
fn remove_block_tag(html: &str, tag: &str) -> String {
    let lower = html.to_lowercase();
    let open = format!("<{}", tag);
    let close = format!("</{}>", tag);
    let mut result = String::with_capacity(html.len());
    let mut pos = 0usize;

    while pos < html.len() {
        if let Some(start) = lower[pos..].find(&open).map(|i| i + pos) {
            result.push_str(&html[pos..start]);
            // Find the matching close tag
            if let Some(end) = lower[start..].find(&close).map(|i| i + start + close.len()) {
                pos = end;
            } else {
                pos = html.len();
            }
        } else {
            result.push_str(&html[pos..]);
            break;
        }
    }

    result
}
