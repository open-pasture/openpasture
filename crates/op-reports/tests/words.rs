//! The collars are audio only, so words for anything else never reach a
//! person (field-ready §2.15): not in the UI (`ui/src`, every file), not in
//! any string of the reports, the texts or the brief, nor in the welfare
//! record. The one exception is the report's method note saying the collars
//! have no stimulus.

use std::path::{Path, PathBuf};

const WORDS: [&str; 4] = ["shock", "stimul", "pulse", "zap"];

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

fn files(dir: &Path, exts: &[&str], out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        if p.is_dir() {
            if name != "node_modules" && name != "dist" && !name.starts_with('.') {
                files(&p, exts, out);
            }
        } else if exts.is_empty() || p.extension().is_some_and(|x| exts.contains(&x.to_string_lossy().as_ref())) {
            out.push(p);
        }
    }
}

fn hits(text: &str) -> Vec<&'static str> {
    let lower = text.to_lowercase();
    WORDS.iter().copied().filter(|w| lower.contains(w)).collect()
}

/// Every string literal in a Rust source: "…", r"…", r#"…"#, b"…".
fn rust_strings(src: &str) -> Vec<String> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                let mut depth = 0;
                while i < b.len() {
                    if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
                        depth += 1;
                        i += 2;
                    } else if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
                        depth -= 1;
                        i += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
            }
            b'r' if (b.get(i + 1) == Some(&b'"') || b.get(i + 1) == Some(&b'#')) && (i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_')) => {
                let mut j = i + 1;
                let mut hashes = 0;
                while b.get(j) == Some(&b'#') {
                    hashes += 1;
                    j += 1;
                }
                if b.get(j) != Some(&b'"') {
                    i += 1;
                    continue;
                }
                let start = j + 1;
                let close: String = std::iter::once('"').chain(std::iter::repeat_n('#', hashes)).collect();
                let end = src[start..].find(&close).map_or(b.len(), |k| start + k);
                out.push(src[start..end].to_owned());
                i = end + close.len();
            }
            b'"' => {
                let mut j = i + 1;
                let mut s = String::new();
                while j < b.len() && b[j] != b'"' {
                    if b[j] == b'\\' {
                        j += 1;
                    }
                    if j < b.len() {
                        s.push(b[j] as char);
                    }
                    j += 1;
                }
                out.push(s);
                i = j + 1;
            }
            b'\'' => {
                // A char literal ('x', '\n', '"'); a lifetime has no closing quote.
                if b.get(i + 1) == Some(&b'\\') {
                    i += 2;
                    while i < b.len() && b[i] != b'\'' {
                        i += 1;
                    }
                    i += 1;
                } else if b.get(i + 2) == Some(&b'\'') {
                    i += 3;
                } else {
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }
    out
}

#[test]
fn the_lexer_finds_strings_and_skips_comments() {
    let src = "// a zap comment\nlet a = \"one \\\"q\\\"\"; /* shock */ let b = r#\"raw \"x\"\"#; let c = '\"'; fn f<'a>() {} let d = \"two\";";
    assert_eq!(rust_strings(src), vec!["one \"q\"", "raw \"x\"", "two"]);
}

#[test]
fn no_word_for_a_stimulus_in_the_ui() {
    let mut list = Vec::new();
    files(&root().join("ui/src"), &[], &mut list);
    assert!(list.len() > 50, "found the UI sources");
    let mut found = Vec::new();
    for f in list {
        let Ok(text) = std::fs::read_to_string(&f) else { continue };
        for (n, line) in text.lines().enumerate() {
            let h = hits(line);
            if !h.is_empty() {
                found.push(format!("{}:{}: {h:?}", f.display(), n + 1));
            }
        }
    }
    assert!(found.is_empty(), "{found:#?}");
}

#[test]
fn no_word_for_a_stimulus_in_reports_texts_or_the_brief() {
    let r = root();
    let mut list = Vec::new();
    for dir in ["crates/op-reports/src", "crates/op-alerts/src", "crates/op-analytics/src/welfare"] {
        files(&r.join(dir), &["rs"], &mut list);
    }
    list.push(r.join("crates/op-analytics/src/welfare.rs"));
    list.push(r.join("crates/op-engine/src/brief.rs"));
    assert!(list.len() > 20, "found the sources");
    let mut found = Vec::new();
    let mut note_seen = 0;
    for f in &list {
        let text = std::fs::read_to_string(f).unwrap_or_else(|e| panic!("{}: {e}", f.display()));
        for s in rust_strings(&text) {
            if s == op_reports::AUDIO_ONLY_NOTE {
                note_seen += 1;
                continue;
            }
            let h = hits(&s);
            if !h.is_empty() {
                found.push(format!("{}: {s:?} {h:?}", f.display()));
            }
        }
    }
    assert!(found.is_empty(), "{found:#?}");
    assert_eq!(note_seen, 1, "the method note is the one exception");
}
