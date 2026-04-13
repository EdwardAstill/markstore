use std::path::{Path, PathBuf};
use std::fs;
use chrono::Utc;
use tantivy::{Index, IndexWriter, TantivyDocument, Term};
use tantivy::query::QueryParser;
use tantivy::schema::*;
use tantivy::collector::TopDocs;

use crate::document::{StoredDocument, parse_frontmatter, extract_h1, make_id};
use crate::error::{MksError, MksResult};

const DOCS_DIR: &str = "docs";
const INDEX_DIR: &str = "index";
const META_FILE: &str = "meta.json";

pub struct Store {
    root: PathBuf,
    schema: Schema,
    index: Index,
}

impl Store {
    /// Open an existing store. Fails if not initialized.
    pub fn open(root: &Path) -> MksResult<Self> {
        let meta = root.join(META_FILE);
        if !meta.exists() {
            return Err(MksError::NotInitialized(root.display().to_string()));
        }
        let (schema, index) = open_index(&root.join(INDEX_DIR))?;
        Ok(Self { root: root.to_path_buf(), schema, index })
    }

    /// Create a new store at root.
    pub fn init(root: &Path) -> MksResult<Self> {
        fs::create_dir_all(root.join(DOCS_DIR))?;
        fs::create_dir_all(root.join(INDEX_DIR))?;
        fs::write(root.join(META_FILE), r#"{"version":1}"#)?;
        let (schema, index) = create_index(&root.join(INDEX_DIR))?;
        Ok(Self { root: root.to_path_buf(), schema, index })
    }

    /// Add a markdown document to the store.
    pub fn add(&self, content: &str, source_path: &str, collection: &str) -> MksResult<StoredDocument> {
        let (fm, body) = parse_frontmatter(content);
        let title = fm.title
            .or_else(|| extract_h1(body))
            .unwrap_or_else(|| {
                Path::new(source_path)
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string()
            });

        let id = make_id(&title, source_path);
        let doc = StoredDocument {
            id: id.clone(),
            collection: collection.to_string(),
            title: title.clone(),
            content: content.to_string(),
            source_path: source_path.to_string(),
            added_at: Utc::now(),
            frontmatter: fm.fields,
        };

        // Persist JSON sidecar
        let doc_path = self.root.join(DOCS_DIR).join(format!("{}.json", id));
        fs::write(&doc_path, serde_json::to_string_pretty(&doc)?)?;

        // Index for full-text search
        let mut writer: IndexWriter = self.index.writer(50_000_000)
            .map_err(MksError::Index)?;
        let id_field = self.schema.get_field("id").unwrap();
        let title_field = self.schema.get_field("title").unwrap();
        let body_field = self.schema.get_field("body").unwrap();
        let collection_field = self.schema.get_field("collection").unwrap();

        let mut tantivy_doc = TantivyDocument::default();
        tantivy_doc.add_text(id_field, &id);
        tantivy_doc.add_text(title_field, &title);
        tantivy_doc.add_text(body_field, body);
        tantivy_doc.add_text(collection_field, collection);
        writer.add_document(tantivy_doc).map_err(MksError::Index)?;
        writer.commit().map_err(MksError::Index)?;

        Ok(doc)
    }

    /// Full-text search. Returns matching documents ordered by relevance.
    pub fn search(&self, query: &str, limit: usize, collection: Option<&str>) -> MksResult<Vec<StoredDocument>> {
        let reader = self.index.reader().map_err(MksError::Index)?;
        let searcher = reader.searcher();

        let title_field = self.schema.get_field("title").unwrap();
        let body_field = self.schema.get_field("body").unwrap();
        let id_field = self.schema.get_field("id").unwrap();

        let query_parser = QueryParser::for_index(&self.index, vec![title_field, body_field]);
        let parsed = query_parser.parse_query(query).map_err(|e| MksError::Other(e.to_string()))?;

        let top_docs = searcher.search(&parsed, &TopDocs::with_limit(limit * 2))
            .map_err(MksError::Index)?;

        let mut results = Vec::new();
        for (_score, addr) in top_docs {
            let retrieved = searcher.doc::<TantivyDocument>(addr).map_err(MksError::Index)?;
            let id_val = retrieved.get_first(id_field)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let doc = self.load_by_id(&id_val)?;
            if let Some(col) = collection {
                if doc.collection != col {
                    continue;
                }
            }
            results.push(doc);
            if results.len() >= limit {
                break;
            }
        }

        Ok(results)
    }

    /// Retrieve a document by ID.
    pub fn get(&self, id: &str) -> MksResult<StoredDocument> {
        self.load_by_id(id)
    }

    /// List all documents, optionally filtered by collection.
    pub fn list(&self, collection: Option<&str>, limit: usize) -> MksResult<Vec<StoredDocument>> {
        let docs_dir = self.root.join(DOCS_DIR);
        let mut docs = Vec::new();

        for entry in fs::read_dir(&docs_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let content = fs::read_to_string(&path)?;
            let doc: StoredDocument = serde_json::from_str(&content)?;
            if let Some(col) = collection {
                if doc.collection != col {
                    continue;
                }
            }
            docs.push(doc);
            if docs.len() >= limit {
                break;
            }
        }

        docs.sort_by(|a, b| b.added_at.cmp(&a.added_at));
        Ok(docs)
    }

    /// Remove a document by ID.
    pub fn remove(&self, id: &str) -> MksResult<()> {
        let doc_path = self.root.join(DOCS_DIR).join(format!("{}.json", id));
        if !doc_path.exists() {
            return Err(MksError::NotFound(id.to_string()));
        }
        fs::remove_file(&doc_path)?;

        let id_field = self.schema.get_field("id").unwrap();
        let mut writer: IndexWriter = self.index.writer(50_000_000).map_err(MksError::Index)?;
        writer.delete_term(Term::from_field_text(id_field, id));
        writer.commit().map_err(MksError::Index)?;

        Ok(())
    }

    /// Count total documents and by collection.
    pub fn stats(&self) -> MksResult<Stats> {
        let docs_dir = self.root.join(DOCS_DIR);
        let mut total = 0usize;
        let mut by_collection: std::collections::HashMap<String, usize> = Default::default();

        for entry in fs::read_dir(&docs_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let content = fs::read_to_string(&path)?;
            if let Ok(doc) = serde_json::from_str::<StoredDocument>(&content) {
                total += 1;
                *by_collection.entry(doc.collection).or_insert(0) += 1;
            }
        }

        Ok(Stats { total, by_collection })
    }

    fn load_by_id(&self, id: &str) -> MksResult<StoredDocument> {
        let path = self.root.join(DOCS_DIR).join(format!("{}.json", id));
        if !path.exists() {
            return Err(MksError::NotFound(id.to_string()));
        }
        let content = fs::read_to_string(&path)?;
        Ok(serde_json::from_str(&content)?)
    }
}

pub struct Stats {
    pub total: usize,
    pub by_collection: std::collections::HashMap<String, usize>,
}

fn schema_builder() -> Schema {
    let mut builder = Schema::builder();
    builder.add_text_field("id", STRING | STORED);
    builder.add_text_field("title", TEXT | STORED);
    builder.add_text_field("body", TEXT);
    builder.add_text_field("collection", STRING | STORED);
    builder.build()
}

fn create_index(index_dir: &Path) -> MksResult<(Schema, Index)> {
    let schema = schema_builder();
    let index = Index::create_in_dir(index_dir, schema.clone()).map_err(MksError::Index)?;
    Ok((schema, index))
}

fn open_index(index_dir: &Path) -> MksResult<(Schema, Index)> {
    let schema = schema_builder();
    let index = Index::open_in_dir(index_dir).map_err(MksError::Index)?;
    Ok((schema, index))
}
