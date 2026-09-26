//! Knowledge search over the seed corpus (`seed/principles`, embedded) and the
//! farm's stored lessons, with tantivy. The index lives in
//! `<data_dir>/knowledge-index` and is rebuilt when missing or when the seed
//! or lessons change. Seed files are split into lessons by `## ` heading, like
//! the kit's `LessonExtractor`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use op_core::{Ctx, time};
use rust_embed::RustEmbed;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::Row;
use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::{Field, IndexRecordOption, STORED, STRING, Schema, TextFieldIndexing, TextOptions, Value as _};
use tantivy::{Index, IndexReader, TantivyDocument, doc};

#[derive(RustEmbed)]
#[folder = "$CARGO_MANIFEST_DIR/../../seed"]
struct SeedFiles;

const INDEX_DIR: &str = "knowledge-index";
const FINGERPRINT_FILE: &str = "fingerprint";
const SCHEMA_VERSION: &str = "1";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    pub title: String,
    /// principle | technique | signal | mistake (seed), lesson | outcome | farmer (farm lessons)
    pub kind: String,
    pub body: String,
    pub source: String,
}

fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_owned()
}

fn title_case(stem: &str) -> String {
    stem.split('_')
        .map(|w| {
            let mut c = w.chars();
            c.next().map(|f| f.to_uppercase().chain(c.flat_map(char::to_lowercase)).collect()).unwrap_or_default()
        })
        .collect::<Vec<String>>()
        .join(" ")
}

fn classify(heading: &str, content: &str) -> &'static str {
    let s = format!("{heading} {content}").to_lowercase();
    let any = |words: &[&str]| words.iter().any(|w| s.contains(w));
    if any(&["mistake", "avoid", "don't", "do not", "never"]) {
        "mistake"
    } else if any(&["signal", "look for", "watch", "behavior", "indicates"]) {
        "signal"
    } else if any(&["rule", "principle", "bias", "matters", "philosophy"]) {
        "principle"
    } else {
        "technique"
    }
}

/// Split one seed markdown file into lessons (kit `LessonExtractor.extract`).
pub fn extract(text: &str, source_title: &str) -> Vec<Entry> {
    let mut sections: Vec<(String, Vec<String>)> = Vec::new();
    let mut heading = "Overview".to_owned();
    let mut lines: Vec<String> = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        if let Some(h) = t.strip_prefix("## ") {
            if !lines.is_empty() {
                sections.push((heading.clone(), std::mem::take(&mut lines)));
            }
            heading = h.trim().to_owned();
            continue;
        }
        if t.starts_with("# ") {
            continue;
        }
        lines.push(t.to_owned());
    }
    if !lines.is_empty() {
        sections.push((heading, lines));
    }
    let mut out = Vec::new();
    for (i, (heading, body)) in sections.iter().filter(|(_, b)| b.iter().any(|l| !l.is_empty())).enumerate() {
        let content = body.iter().filter(|l| !l.is_empty()).cloned().collect::<Vec<_>>().join(" ");
        let normalized = content.split_whitespace().collect::<Vec<_>>().join(" ");
        if normalized.chars().count() < 24 {
            continue;
        }
        let hslug: String = slug(heading).chars().take(32).collect();
        out.push(Entry {
            id: format!("knowledge_{}_{:02}_{}", slug(source_title), i + 1, hslug),
            title: heading.clone(),
            kind: classify(heading, &normalized).to_owned(),
            body: normalized,
            source: source_title.to_owned(),
        });
    }
    out
}

/// Every lesson in the embedded seed corpus.
pub fn seed_entries() -> &'static [Entry] {
    static SEED: OnceLock<Vec<Entry>> = OnceLock::new();
    SEED.get_or_init(|| {
        let mut files: Vec<String> = SeedFiles::iter().map(|f| f.to_string()).filter(|f| f.starts_with("principles/") && f.ends_with(".md")).collect();
        files.sort();
        files
            .iter()
            .flat_map(|f| {
                let text = SeedFiles::get(f).map(|e| String::from_utf8_lossy(&e.data).into_owned()).unwrap_or_default();
                let stem = Path::new(f).file_stem().and_then(|s| s.to_str()).unwrap_or(f);
                extract(&text, &title_case(stem))
            })
            .collect()
    })
}

fn seed_hash() -> &'static str {
    static H: OnceLock<String> = OnceLock::new();
    H.get_or_init(|| {
        let mut h = Sha256::new();
        for e in seed_entries() {
            h.update(e.id.as_bytes());
            h.update(e.body.as_bytes());
        }
        h.finalize().iter().take(8).map(|b| format!("{b:02x}")).collect()
    })
}

pub async fn lessons(ctx: &Ctx) -> anyhow::Result<Vec<Entry>> {
    let rows = sqlx::query("SELECT id, title, body, kind, source FROM lessons ORDER BY created_at DESC").fetch_all(ctx.db()).await?;
    Ok(rows.iter().map(|r| Entry { id: r.get("id"), title: r.get("title"), body: r.get("body"), kind: r.get("kind"), source: r.get("source") }).collect())
}

/// Record a lesson learned on this farm. It shows up in search right away.
pub async fn add_lesson(
    ctx: &Ctx,
    title: &str,
    body: &str,
    kind: &str,
    source: &str,
    decision_id: Option<&str>,
    paddock_id: Option<&str>,
) -> anyhow::Result<String> {
    let id = op_core::id::new_id("les");
    sqlx::query("INSERT INTO lessons (id, title, body, kind, source, decision_id, paddock_id, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)")
        .bind(&id)
        .bind(title)
        .bind(body)
        .bind(kind)
        .bind(source)
        .bind(decision_id)
        .bind(paddock_id)
        .bind(time::to_db(&time::now()))
        .execute(ctx.db())
        .await?;
    Ok(id)
}

struct Fields {
    id: Field,
    title: Field,
    body: Field,
    source: Field,
}

fn schema() -> (Schema, Fields) {
    let text = TextOptions::default()
        .set_indexing_options(TextFieldIndexing::default().set_tokenizer("en_stem").set_index_option(IndexRecordOption::WithFreqsAndPositions));
    let mut b = Schema::builder();
    let id = b.add_text_field("id", STRING | STORED);
    let title = b.add_text_field("title", text.clone());
    let body = b.add_text_field("body", text.clone());
    let source = b.add_text_field("source", text);
    (b.build(), Fields { id, title, body, source })
}

struct Built {
    fingerprint: String,
    index: Index,
    reader: IndexReader,
    fields: Fields,
    entries: HashMap<String, Entry>,
}

type Cache = Mutex<HashMap<PathBuf, Arc<Built>>>;

fn cache() -> &'static Cache {
    static C: OnceLock<Cache> = OnceLock::new();
    C.get_or_init(Default::default)
}

async fn fingerprint(ctx: &Ctx) -> anyhow::Result<String> {
    let row = sqlx::query("SELECT COUNT(*), COALESCE(MAX(created_at), ''), COALESCE(MAX(id), '') FROM lessons").fetch_one(ctx.db()).await?;
    let (n, at, id): (i64, String, String) = (row.get(0), row.get(1), row.get(2));
    Ok(format!("v{SCHEMA_VERSION}:{}:{n}:{at}:{id}", seed_hash()))
}

fn build(dir: &Path, fingerprint: &str, entries: Vec<Entry>) -> anyhow::Result<Built> {
    let (schema, fields) = schema();
    let fp_path = dir.join(FINGERPRINT_FILE);
    let on_disk = std::fs::read_to_string(&fp_path).ok();
    let index = if on_disk.as_deref() == Some(fingerprint) { Index::open_in_dir(dir).ok() } else { None };
    let index = match index {
        Some(i) => i,
        None => {
            let _ = std::fs::remove_dir_all(dir);
            std::fs::create_dir_all(dir)?;
            let index = Index::create_in_dir(dir, schema)?;
            let mut w: tantivy::IndexWriter = index.writer_with_num_threads(1, 15_000_000)?;
            for e in &entries {
                w.add_document(
                    doc!(fields.id => e.id.clone(), fields.title => e.title.clone(), fields.body => e.body.clone(), fields.source => e.source.clone()),
                )?;
            }
            w.commit()?;
            std::fs::write(&fp_path, fingerprint)?;
            index
        }
    };
    let reader = index.reader()?;
    reader.reload()?;
    Ok(Built { fingerprint: fingerprint.to_owned(), index, reader, fields, entries: entries.into_iter().map(|e| (e.id.clone(), e)).collect() })
}

async fn built(ctx: &Ctx) -> anyhow::Result<Arc<Built>> {
    let fp = fingerprint(ctx).await?;
    let dir = ctx.data_dir().join(INDEX_DIR);
    if let Some(b) = cache().lock().unwrap_or_else(|e| e.into_inner()).get(&dir)
        && b.fingerprint == fp
    {
        return Ok(b.clone());
    }
    let mut entries = lessons(ctx).await?;
    entries.extend(seed_entries().iter().cloned());
    let d = dir.clone();
    let b = Arc::new(tokio::task::spawn_blocking(move || build(&d, &fp, entries)).await??);
    cache().lock().unwrap_or_else(|e| e.into_inner()).insert(dir, b.clone());
    Ok(b)
}

/// Build the index if it is missing or stale. Called on start.
pub async fn ensure_index(ctx: &Ctx) -> anyhow::Result<()> {
    built(ctx).await.map(|_| ())
}

/// Best matches for `q`. An empty query lists farm lessons, then the seed.
pub async fn search(ctx: &Ctx, q: &str, limit: usize) -> anyhow::Result<Vec<Entry>> {
    let limit = limit.clamp(1, 100);
    let b = built(ctx).await?;
    if q.trim().is_empty() {
        let mut all = lessons(ctx).await?;
        all.extend(seed_entries().iter().cloned());
        all.truncate(limit);
        return Ok(all);
    }
    let q = q.to_owned();
    tokio::task::spawn_blocking(move || {
        let searcher = b.reader.searcher();
        let mut parser = QueryParser::for_index(&b.index, vec![b.fields.title, b.fields.body, b.fields.source]);
        parser.set_field_boost(b.fields.title, 2.0);
        let (query, _) = parser.parse_query_lenient(&q);
        let top = searcher.search(&query, &TopDocs::with_limit(limit).order_by_score())?;
        let mut out = Vec::new();
        for (_, addr) in top {
            let d: TantivyDocument = searcher.doc(addr)?;
            if let Some(e) = d.get_first(b.fields.id).and_then(|v| v.as_str()).and_then(|id| b.entries.get(id)) {
                out.push(e.clone());
            }
        }
        anyhow::Ok(out)
    })
    .await?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_splits_into_lessons() {
        let seed = seed_entries();
        assert!(seed.len() >= 15, "only {} seed lessons", seed.len());
        let e = seed.iter().find(|e| e.title == "Top-Third Rule").expect("universal top-third lesson");
        assert_eq!(e.source, "Universal");
        assert_eq!(e.kind, "mistake"); // "avoid forcing", as the kit classifies it
        assert!(e.id.starts_with("knowledge_universal_"));
    }
}
