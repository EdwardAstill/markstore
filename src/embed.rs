/// Ollama embedding API client and cosine similarity.

pub const DEFAULT_MODEL: &str = "nomic-embed-text";
pub const DEFAULT_BASE_URL: &str = "http://localhost:11434";

/// Compute the embedding of `text` by calling the Ollama `/api/embeddings` endpoint.
/// Returns `None` if Ollama is not running or the request fails.
pub fn embed(text: &str, model: &str, base_url: &str) -> Option<Vec<f32>> {
    let response = ureq::post(&format!("{}/api/embeddings", base_url))
        .set("Content-Type", "application/json")
        .timeout(std::time::Duration::from_secs(60))
        .send_json(serde_json::json!({
            "model": model,
            "prompt": text,
        }))
        .ok()?;

    let body: serde_json::Value = response.into_json().ok()?;
    let vec: Vec<f32> = body["embedding"]
        .as_array()?
        .iter()
        .filter_map(|v| v.as_f64().map(|f| f as f32))
        .collect();

    if vec.is_empty() { None } else { Some(vec) }
}

/// Cosine similarity between two equal-length vectors. Returns 0.0 on length mismatch or zero norms.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        0.0
    } else {
        dot / (norm_a * norm_b)
    }
}

/// Serialises a vector to a compact JSON string for storage.
pub fn vec_to_json(v: &[f32]) -> String {
    let parts: Vec<String> = v.iter().map(|f| format!("{:.6}", f)).collect();
    format!("[{}]", parts.join(","))
}

/// Deserialises a vector from the stored JSON string.
pub fn vec_from_json(s: &str) -> Option<Vec<f32>> {
    let v: serde_json::Value = serde_json::from_str(s).ok()?;
    Some(
        v.as_array()?
            .iter()
            .filter_map(|x| x.as_f64().map(|f| f as f32))
            .collect(),
    )
}
