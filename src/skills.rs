use anyhow::Result;
use serde::Serialize;
use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    pub body: String,
}

pub fn discover(cwd: &Path) -> Vec<Skill> {
    let mut out = Vec::new();
    let mut roots = vec![
        cwd.join(".varynth").join("skills"),
        cwd.join(".claude").join("skills"),
        crate::config::Config::home_dir().join("skills"),
    ];
    if let Some(home) = dirs::home_dir() {
        roots.push(home.join(".claude").join("skills"));
    }
    for root in roots {
        walk_skills(&root, &mut out);
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out.dedup_by(|a, b| a.name == b.name);
    out
}

fn walk_skills(root: &Path, out: &mut Vec<Skill>) {
    if !root.exists() {
        return;
    }
    let walker = walkdir::WalkDir::new(root).max_depth(4);
    for e in walker.into_iter().flatten() {
        if e.file_name() != "SKILL.md" {
            continue;
        }
        if let Ok(skill) = parse_skill(e.path()) {
            out.push(skill);
        }
    }
}

/// Parsed `---` frontmatter of a skill markdown document.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Frontmatter {
    /// Raw frontmatter block including its `---` delimiters, when present.
    pub raw: Option<String>,
    pub name: Option<String>,
    pub description: Option<String>,
    /// Names declared under an optional `env:` key (required environment
    /// variables). Accepts block lists (`- NAME` lines), inline `[A, B]`
    /// lists and plain scalars.
    pub env: Vec<String>,
}

/// Byte span of a leading `---` frontmatter block (delimiters included), if
/// the document starts with one. Lenient about leading whitespace.
pub fn frontmatter_span(md: &str) -> Option<Range<usize>> {
    let lead = md.len() - md.trim_start().len();
    if !md[lead..].starts_with("---") {
        return None;
    }
    let after_open = lead + 3;
    let close_rel = md[after_open..].find("\n---")?;
    let close = after_open + close_rel + 1;
    let end = md[close..].find('\n').map_or(md.len(), |n| close + n + 1);
    Some(lead..end)
}

/// Extract frontmatter keys (`name:`, `description:`, `env:`) from a skill
/// markdown document. A missing block or missing keys simply stay `None` /
/// empty, so callers can layer their own fallbacks on top.
pub fn parse_frontmatter(md: &str) -> Frontmatter {
    let mut fm = Frontmatter::default();
    let Some(span) = frontmatter_span(md) else {
        return fm;
    };
    fm.raw = Some(md[span.start..span.end].trim_end().to_string());
    let inner = &md[span.start + 3..span.end];
    let mut lines = inner.lines().peekable();
    while let Some(line) = lines.next() {
        let line = line.trim_end();
        if line.starts_with("---") {
            break; // closing delimiter
        }
        if let Some(v) = line.strip_prefix("name:") {
            fm.name = yaml_scalar(v);
        } else if let Some(v) = line.strip_prefix("description:") {
            fm.description = yaml_scalar(v);
        } else if let Some(v) = line.strip_prefix("env:") {
            let value = v.trim();
            if value.starts_with('[') {
                let list = value.trim_start_matches('[').trim_end_matches(']');
                for item in list.split(',') {
                    if let Some(s) = yaml_scalar(item) {
                        fm.env.push(s);
                    }
                }
            } else if !value.is_empty() {
                if let Some(s) = yaml_scalar(value) {
                    fm.env.push(s);
                }
            } else {
                // Block list: consecutive `- ITEM` lines.
                while let Some(next) = lines.peek() {
                    let Some(item) = next.trim().strip_prefix("- ") else {
                        break;
                    };
                    if let Some(s) = yaml_scalar(item) {
                        fm.env.push(s);
                    }
                    lines.next();
                }
            }
        }
    }
    fm
}

/// Trim a YAML scalar and strip matching surrounding quotes.
fn yaml_scalar(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let unquoted = trimmed
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| {
            trimmed
                .strip_prefix('\'')
                .and_then(|s| s.strip_suffix('\''))
        })
        .unwrap_or(trimmed)
        .trim();
    if unquoted.is_empty() {
        None
    } else {
        Some(unquoted.to_string())
    }
}

fn parse_skill(path: &Path) -> Result<Skill> {
    let body = fs::read_to_string(path)?;
    let fm = parse_frontmatter(&body);
    let mut name = path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .unwrap_or("skill")
        .to_string();
    if let Some(n) = fm.name {
        name = n;
    }
    let mut description = fm.description.unwrap_or_default();
    if description.is_empty() {
        description = body
            .lines()
            .find(|l| !l.trim().is_empty() && !l.starts_with("---") && !l.starts_with('#'))
            .unwrap_or("skill")
            .trim()
            .chars()
            .take(160)
            .collect();
    }
    Ok(Skill {
        name,
        description,
        path: path.to_path_buf(),
        body,
    })
}

/// Parse a single SKILL.md file from disk. Public wrapper around the internal
/// parser so other modules (e.g. `skills_search::list_installed`) can reuse
/// the exact same name/description semantics as `discover`.
pub fn parse_skill_file(path: &Path) -> Result<Skill> {
    parse_skill(path)
}

pub fn render_for_prompt(skills: &[Skill]) -> String {
    if skills.is_empty() {
        return String::new();
    }
    let mut s =
        String::from("\n\n# Available skills\nInvoke with /name. Bodies are loaded on demand.\n");
    for sk in skills {
        s.push_str(&format!("- /{} — {}\n", sk.name, sk.description));
    }
    s
}

pub fn load_named<'a>(skills: &'a [Skill], name: &str) -> Option<&'a Skill> {
    let n = name.trim_start_matches('/');
    skills.iter().find(|s| s.name.eq_ignore_ascii_case(n))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_parses_name_description_and_env_block_list() {
        let md = "---\nname: find-skills\ndescription: Find skills on skills.sh\nenv:\n  - GITHUB_TOKEN\n  - OPENAI_API_KEY\n---\n\n# Body\n";
        let fm = parse_frontmatter(md);
        assert_eq!(fm.name.as_deref(), Some("find-skills"));
        assert_eq!(fm.description.as_deref(), Some("Find skills on skills.sh"));
        assert_eq!(fm.env, vec!["GITHUB_TOKEN", "OPENAI_API_KEY"]);
        assert!(fm.raw.as_deref().unwrap().starts_with("---"));
    }

    #[test]
    fn frontmatter_handles_inline_env_and_quoted_values() {
        let md = "---\nname: \"quoted-name\"\ndescription: 'single quoted'\nenv: [A_KEY, 'B_KEY']\n---\nbody";
        let fm = parse_frontmatter(md);
        assert_eq!(fm.name.as_deref(), Some("quoted-name"));
        assert_eq!(fm.description.as_deref(), Some("single quoted"));
        assert_eq!(fm.env, vec!["A_KEY", "B_KEY"]);
    }

    #[test]
    fn frontmatter_missing_block_yields_defaults() {
        assert_eq!(
            parse_frontmatter("# just markdown\n"),
            Frontmatter::default()
        );
        // Delimiters present but no keys.
        let fm = parse_frontmatter("---\n---\nbody");
        assert!(fm.raw.is_some());
        assert_eq!(fm.name, None);
        assert!(fm.env.is_empty());
    }

    #[test]
    fn frontmatter_span_end_is_body_start() {
        let md = "---\nname: x\n---\nbody line";
        let span = frontmatter_span(md).unwrap();
        assert_eq!(&md[span.end..], "body line");
        assert_eq!(md[span.start..span.end].trim_end(), "---\nname: x\n---");
        assert!(frontmatter_span("no frontmatter here").is_none());
        assert!(frontmatter_span("---").is_none());
    }

    #[test]
    fn parse_skill_file_prefers_frontmatter_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("SKILL.md");
        std::fs::write(
            &path,
            "---\nname: my-skill\ndescription: does things\n---\n# My Skill\n",
        )
        .unwrap();
        let skill = parse_skill_file(&path).unwrap();
        assert_eq!(skill.name, "my-skill");
        assert_eq!(skill.description, "does things");
    }
}
