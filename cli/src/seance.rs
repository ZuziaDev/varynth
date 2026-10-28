//! Seance Mode & Session Tree Consultant for Varynth.
//! Summons insights, solutions and memories from previous conversations
//! to assist the current active turn.

use crate::config::Config;
use crate::session::Session;
use anyhow::Result;
use std::fs;

pub struct SeanceResult {
    pub session_id: String,
    pub title: String,
    pub snippet: String,
}

pub fn search_past_sessions(query: &str, max_results: usize) -> Result<Vec<SeanceResult>> {
    Config::ensure_home()?;
    let dir = Config::sessions_dir();
    let mut results: Vec<(i32, SeanceResult)> = Vec::new();
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let q = query.trim().to_lowercase();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }

        if let Ok(sess) = Session::load_path(path) {
            let title = sess.meta.title.clone();
            let mut match_score = 0;
            if title.to_lowercase().contains(&q) {
                match_score += 10;
            }

            let mut best_snippet = String::new();
            for msg in &sess.messages {
                if msg.content.to_lowercase().contains(&q) {
                    match_score += 1;
                    if best_snippet.is_empty() {
                        let preview: String = msg.content.chars().take(300).collect();
                        best_snippet = preview;
                    }
                }
            }

            if match_score > 0 {
                results.push((
                    match_score,
                    SeanceResult {
                        session_id: sess.meta.id.clone(),
                        title,
                        snippet: if best_snippet.is_empty() {
                            "Session matched by title".into()
                        } else {
                            best_snippet
                        },
                    },
                ));
            }
        }
    }

    results.sort_by(|a, b| b.0.cmp(&a.0));
    Ok(results.into_iter().take(max_results).map(|r| r.1).collect())
}
