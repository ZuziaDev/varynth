//! skills.sh directory client: search the skills.sh index, then fetch,
//! validate and install skills from skills.sh slugs, GitHub repositories or
//! direct raw-markdown URLs.
//!
//! Source forms accepted by [`resolve_source`]:
//! - `https://skills.sh/<owner>/<repo>/<skill>` (also bare `skills.sh/...`
//!   slugs and plain `owner/repo/skill` triplets)
//! - `<owner>/<repo>` GitHub shorthand (root SKILL.md on `main`, then `master`)
//! - any `https://` URL to raw skill markdown
//!
//! Note on [`search`]: skills.sh (Vercel's agent skills directory) exposes no
//! documented, stable machine-readable search API on its public pages, so the
//! client opportunistically tries `https://skills.sh/api/search?q=` first and
//! falls back to lenient `/<owner>/<repo>/<skill>` href extraction from the
//! server-rendered `https://skills.sh/search?q=` page. If neither endpoint is
//! reachable the caller gets a clear error pointing at direct installation
//! instead of a brittle scrape result.
//!
//! Everything here is synchronous and builds a blocking reqwest client: call
//! these functions from sync CLI/UI code paths only, never from inside an
//! async runtime.

#![allow(dead_code)]

use anyhow::{anyhow, bail, Context, Result};
use regex::Regex;
use serde::Serialize;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

/// The skills.sh search/install client is fully implemented; this constant is
/// retained only as a compatibility marker for older integrations.
pub const MODULE_READY: bool = true;

const FETCH_TIMEOUT: Duration = Duration::from_secs(20);
/// Skills are small markdown documents; larger payloads are rejected.
const MAX_SKILL_BYTES: usize = 2 * 1024 * 1024;
const INDEX_UNAVAILABLE: &str =
    "skills.sh index unavailable; install directly with skills.sh/<owner>/<repo>/<skill> or a GitHub URL";

/// A skill as shown in search results or the installed-skills picker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkillEntry {
    pub name: String,
    pub description: String,
    /// Where it came from: a skills.sh slug, GitHub `owner/repo`, a URL, or a
    /// local SKILL.md path (for [`list_installed`]).
    pub source: String,
    /// Popularity when the source provides it.
    pub installs: Option<u64>,
}

/// Result of a successful installation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkillInstallReport {
    pub name: String,
    /// Path of the installed SKILL.md file (`<dir>/<name>/SKILL.md`).
    pub path: PathBuf,
    /// Non-fatal observations: missing description, unset env requirements.
    pub warnings: Vec<String>,
}

/// Where a skill should be fetched from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillSource {
    /// A skills.sh index entry backed by a GitHub repo:
    /// `https://skills.sh/<owner>/<repo>/<skill>`.
    SkillsSh {
        owner: String,
        repo: String,
        skill: String,
    },
    /// A GitHub repository, resolved from its root SKILL.md.
    GitHubRepo { owner: String, repo: String },
    /// A direct URL to raw skill markdown.
    RawUrl(String),
}

impl SkillSource {
    /// Canonical string form; feeds back into [`resolve_source`].
    pub fn canonical(&self) -> String {
        match self {
            SkillSource::SkillsSh { owner, repo, skill } => format!("{owner}/{repo}/{skill}"),
            SkillSource::GitHubRepo { owner, repo } => format!("{owner}/{repo}"),
            SkillSource::RawUrl(url) => url.clone(),
        }
    }
}

/// Resolve a user-supplied skill source specifier. Rejects path traversal
/// (`..` segments), absolute path segments and empty input; every segment of
/// a slug must be 1-100 chars of `[A-Za-z0-9._-]` without a leading dot.
pub fn resolve_source(input: &str) -> Result<SkillSource> {
    let input = input.trim().trim_end_matches('/');
    if input.is_empty() {
        bail!("empty skill source");
    }
    let lower = input.to_ascii_lowercase();
    for scheme in ["https://", "http://"] {
        if lower.starts_with(scheme) {
            let rest = &input[scheme.len()..];
            let host_end = rest.find('/').unwrap_or(rest.len());
            let host = rest[..host_end].to_ascii_lowercase();
            let path = rest[host_end..].trim_start_matches('/');
            let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
            return match host.as_str() {
                // skills.sh indexes GitHub repos; the slug is owner/repo/skill.
                "skills.sh" => slug_source(&segments),
                // A github.com page URL maps to the repo shorthand; the raw
                // content has to come from raw.githubusercontent.com.
                "github.com" if segments.len() >= 2 => slug_source(&segments[..2]),
                "github.com" => bail!("GitHub URL needs at least an owner and repository"),
                // Anything else is fetched directly as raw markdown.
                _ => Ok(SkillSource::RawUrl(input.to_string())),
            };
        }
    }
    if lower.starts_with("skills.sh/") {
        let segments: Vec<&str> = input["skills.sh/".len()..]
            .split('/')
            .filter(|s| !s.is_empty())
            .collect();
        return slug_source(&segments);
    }
    let segments: Vec<&str> = input.split('/').filter(|s| !s.is_empty()).collect();
    slug_source(&segments)
}

/// Build a [`SkillSource`] from slash-separated path segments.
fn slug_source(segments: &[&str]) -> Result<SkillSource> {
    for segment in segments {
        if !valid_segment(segment) {
            bail!("invalid path segment {segment:?} in skill source");
        }
    }
    match segments {
        [owner, repo, skill] => Ok(SkillSource::SkillsSh {
            owner: owner.to_string(),
            repo: repo.trim_end_matches(".git").to_string(),
            skill: skill.to_string(),
        }),
        [owner, repo] => Ok(SkillSource::GitHubRepo {
            owner: owner.to_string(),
            repo: repo.trim_end_matches(".git").to_string(),
        }),
        other => bail!(
            "unsupported skill source {other:?}: expected \
             skills.sh/<owner>/<repo>/<skill>, <owner>/<repo>/<skill>, \
             <owner>/<repo> or an https:// URL to a raw SKILL.md"
        ),
    }
}

/// A single segment of a skills.sh slug or GitHub shorthand. Rejects `.` and
/// `..` (path traversal), leading dots, separators and over-long segments.
fn valid_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment.len() <= 100
        && !segment.starts_with('.')
        && segment
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Candidate raw.githubusercontent.com URLs for a skills.sh-style
/// `owner/repo/skill` entry, in priority order: `main` before `master`, the
/// skill directory at the repo root before a `skills/` prefix.
pub fn raw_urls_for_skill(owner: &str, repo: &str, skill: &str) -> Vec<String> {
    let mut urls = Vec::new();
    for branch in ["main", "master"] {
        for prefix in ["", "skills/"] {
            urls.push(format!(
                "https://raw.githubusercontent.com/{owner}/{repo}/{branch}/{prefix}{skill}/SKILL.md"
            ));
        }
    }
    urls
}

/// Candidate raw URLs for a bare `owner/repo` shorthand: the repository-root
/// SKILL.md on `main`, then `master`.
pub fn raw_urls_for_repo(owner: &str, repo: &str) -> Vec<String> {
    ["main", "master"]
        .iter()
        .map(|branch| format!("https://raw.githubusercontent.com/{owner}/{repo}/{branch}/SKILL.md"))
        .collect()
}

/// Validate a skill name: 2-64 chars of `[a-z0-9-_]` starting with a letter
/// or digit, after lowercasing. Rejects path traversal, absolute paths and
/// anything outside the charset.
pub fn sanitize_skill_name(raw: &str) -> Result<String> {
    static NAME_RE: OnceLock<Regex> = OnceLock::new();
    let re = NAME_RE.get_or_init(|| Regex::new(r"^[a-z0-9][a-z0-9-_]{1,63}$").unwrap());
    let name = raw.trim().to_ascii_lowercase();
    if !re.is_match(&name) {
        bail!(
            "invalid skill name {raw:?}: after lowercasing it must be 2-64 \
             chars of [a-z0-9-_] starting with a letter or digit (got {name:?})"
        );
    }
    Ok(name)
}

/// Fetch and validate a skill from `source`. Returns
/// `(frontmatter_block, raw_markdown)`; the frontmatter block is the leading
/// `---` section including delimiters.
///
/// Uses a 20 s timeout and a 2 MiB size cap, requires a `---` frontmatter
/// block with a valid `name:` (see [`sanitize_skill_name`]), and tries every
/// candidate raw URL for the source before failing.
pub fn fetch_skill(source: &SkillSource) -> Result<(String, String)> {
    let candidates = match source {
        SkillSource::SkillsSh { owner, repo, skill } => raw_urls_for_skill(owner, repo, skill),
        SkillSource::GitHubRepo { owner, repo } => raw_urls_for_repo(owner, repo),
        SkillSource::RawUrl(url) => vec![url.clone()],
    };
    let client = http_client()?;
    let mut failures = Vec::new();
    for url in &candidates {
        match fetch_url(&client, url) {
            Ok(found) => return Ok(found),
            Err(err) => failures.push(format!("{url}: {err}")),
        }
    }
    Err(anyhow!(
        "could not fetch a valid SKILL.md for `{}`: {}",
        source.canonical(),
        failures.join("; ")
    ))
}

/// [`resolve_source`] + [`fetch_skill`] convenience.
pub fn fetch_skill_str(input: &str) -> Result<(String, String)> {
    fetch_skill(&resolve_source(input)?)
}

fn http_client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .timeout(FETCH_TIMEOUT)
        .user_agent(concat!("varynth/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building HTTP client")
}

fn fetch_url(client: &reqwest::blocking::Client, url: &str) -> Result<(String, String)> {
    let resp = client
        .get(url)
        .send()
        .with_context(|| format!("requesting {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        bail!("HTTP {status}");
    }
    if let Some(len) = resp.content_length() {
        if len as usize > MAX_SKILL_BYTES {
            bail!("response of {len} bytes exceeds the {MAX_SKILL_BYTES}-byte skill size limit");
        }
    }
    let mut bytes = Vec::new();
    resp.take(MAX_SKILL_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("reading {url}"))?;
    if bytes.len() > MAX_SKILL_BYTES {
        bail!("skill exceeds the {MAX_SKILL_BYTES}-byte size limit");
    }
    let md = String::from_utf8(bytes).with_context(|| format!("{url} is not valid UTF-8"))?;
    validate_skill_md(&md)?;
    Ok(split_frontmatter(&md))
}

fn split_frontmatter(md: &str) -> (String, String) {
    match crate::skills::frontmatter_span(md) {
        Some(span) => (
            md[span.start..span.end].trim_end().to_string(),
            md.to_string(),
        ),
        None => (String::new(), md.to_string()),
    }
}

/// A fetched document only counts as a skill when it carries a `---`
/// frontmatter block whose `name:` passes [`sanitize_skill_name`]. This is
/// what keeps random web content and path-traversal names out of `install`.
fn validate_skill_md(md: &str) -> Result<()> {
    let fm = crate::skills::parse_frontmatter(md);
    if fm.raw.is_none() {
        bail!("document has no `---` frontmatter block");
    }
    let name = fm
        .name
        .filter(|n| !n.trim().is_empty())
        .ok_or_else(|| anyhow!("frontmatter block has no `name:` key"))?;
    sanitize_skill_name(&name)?;
    Ok(())
}

/// Install a skill into `<dir>/<name>/SKILL.md`. Refuses to overwrite an
/// existing installation unless `force` is set. Network fetch + validation
/// happen here; see [`fetch_skill`] for the rules.
pub fn install_to(source: &SkillSource, dir: &Path, force: bool) -> Result<SkillInstallReport> {
    let (_frontmatter, raw) = fetch_skill(source)?;
    install_raw(&raw, dir, force)
}

/// [`resolve_source`] + [`install_to`] into the user-level registry
/// (`~/.varynth/skills`).
pub fn install(source: &SkillSource) -> Result<SkillInstallReport> {
    let dir = crate::config::Config::home_dir().join("skills");
    install_to(source, &dir, false)
}

/// [`resolve_source`] + install into the user-level registry.
pub fn install_str(input: &str) -> Result<SkillInstallReport> {
    install(&resolve_source(input)?)
}

/// Validate an already-fetched skill document and write it below `dir`.
/// No network; this is the testable core of [`install_to`].
pub fn install_raw(raw: &str, dir: &Path, force: bool) -> Result<SkillInstallReport> {
    validate_skill_md(raw)?;
    let fm = crate::skills::parse_frontmatter(raw);
    let name = sanitize_skill_name(
        fm.name
            .as_deref()
            .ok_or_else(|| anyhow!("frontmatter block has no `name:` key"))?,
    )?;
    let skill_dir = dir.join(&name);
    let skill_md = skill_dir.join("SKILL.md");
    let mut warnings = Vec::new();
    if skill_md.exists() && !force {
        bail!(
            "skill `{}` is already installed at {}; pass force to overwrite",
            name,
            skill_md.display()
        );
    }
    fs::create_dir_all(&skill_dir).with_context(|| format!("creating {}", skill_dir.display()))?;
    fs::write(&skill_md, raw).with_context(|| format!("writing {}", skill_md.display()))?;
    if fm
        .description
        .as_deref()
        .map(str::trim)
        .unwrap_or("")
        .is_empty()
    {
        warnings.push(
            "frontmatter has no description; the skill will be listed without one".to_string(),
        );
    }
    let missing = check_requirements(raw);
    if !missing.is_empty() {
        warnings.push(format!(
            "required environment variables not set: {}",
            missing.join(", ")
        ));
    }
    Ok(SkillInstallReport {
        name,
        path: skill_md,
        warnings,
    })
}

/// Environment variables a skill requires that are NOT set in the current
/// process. Requirements come from the frontmatter `env:` key and `$$VAR`
/// tokens in the document body (frontmatter is excluded from token scanning).
pub fn check_requirements(raw_md: &str) -> Vec<String> {
    static ENV_TOKEN_RE: OnceLock<Regex> = OnceLock::new();
    static ENV_NAME_RE: OnceLock<Regex> = OnceLock::new();
    let token_re =
        ENV_TOKEN_RE.get_or_init(|| Regex::new(r"\$\$([A-Za-z_][A-Za-z0-9_]*)").unwrap());
    let name_re = ENV_NAME_RE.get_or_init(|| Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*$").unwrap());

    let fm = crate::skills::parse_frontmatter(raw_md);
    let mut required: Vec<String> = Vec::new();
    for name in &fm.env {
        if name_re.is_match(name) && !required.contains(name) {
            required.push(name.clone());
        }
    }
    let body = match crate::skills::frontmatter_span(raw_md) {
        Some(span) => &raw_md[span.end..],
        None => raw_md,
    };
    for cap in token_re.captures_iter(body) {
        let name = cap[1].to_string();
        if !required.contains(&name) {
            required.push(name);
        }
    }
    required.retain(|name| std::env::var(name).is_err());
    required
}

/// List skills installed under `dir` (name/description from frontmatter,
/// source = local SKILL.md path), sorted by name. Input for install pickers.
/// Only documents with a `---` frontmatter block count as installed skills.
pub fn list_installed(dir: &Path) -> Vec<SkillEntry> {
    let mut out = Vec::new();
    if !dir.is_dir() {
        return out;
    }
    for entry in walkdir::WalkDir::new(dir)
        .max_depth(4)
        .into_iter()
        .flatten()
    {
        if entry.file_name() != "SKILL.md" {
            continue;
        }
        if let Ok(skill) = crate::skills::parse_skill_file(entry.path()) {
            if crate::skills::frontmatter_span(&skill.body).is_none() {
                continue; // not a skill document; don't offer it in pickers
            }
            out.push(SkillEntry {
                name: skill.name,
                description: skill.description,
                source: entry.path().display().to_string(),
                installs: None,
            });
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out.dedup_by(|a, b| a.name == b.name);
    out
}

/// Search the skills.sh index. Tries the JSON API at `/api/search` first,
/// then lenient HTML extraction from the `/search` page. Network call — not
/// covered by unit tests; the response parsers below are.
pub fn search(query: &str) -> Result<Vec<SkillEntry>> {
    let query = query.trim();
    if query.is_empty() {
        bail!("empty search query");
    }
    let client = http_client()?;
    // 1) JSON API (may not exist; any failure falls through to the page).
    if let Some(entries) = fetch_parse(&client, &search_api_url(query), parse_search_json) {
        if !entries.is_empty() {
            return Ok(entries);
        }
    }
    // 2) Server-rendered search page, lenient slug extraction. An empty
    //    result here means "reached the index, no matches".
    if let Some(entries) = fetch_parse(&client, &search_html_url(query), parse_search_html) {
        return Ok(entries);
    }
    Err(anyhow!("{INDEX_UNAVAILABLE}"))
}

fn fetch_parse(
    client: &reqwest::blocking::Client,
    url: &str,
    parse: fn(&str) -> Vec<SkillEntry>,
) -> Option<Vec<SkillEntry>> {
    let resp = client.get(url).send().ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let mut bytes = Vec::new();
    resp.take(MAX_SKILL_BYTES as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    let text = String::from_utf8(bytes).ok()?;
    Some(parse(&text))
}

/// Fetch the skills.sh front page (featured / popular skills) to seed the
/// interactive `/skills-search` browser when it opens without a query.
/// Best-effort: the same lenient slug extraction as the search-page
/// fallback, so an unreachable or restructured index surfaces as the
/// standard "index unavailable" error instead of a hard failure.
pub fn popular() -> Result<Vec<SkillEntry>> {
    const FRONT_PAGE: &str = "https://skills.sh/";
    let client = http_client()?;
    let resp = client
        .get(FRONT_PAGE)
        .send()
        .with_context(|| format!("requesting {FRONT_PAGE}"))?;
    let status = resp.status();
    if !status.is_success() {
        bail!("{FRONT_PAGE}: HTTP {status}");
    }
    let mut bytes = Vec::new();
    resp.take(MAX_SKILL_BYTES as u64)
        .read_to_end(&mut bytes)
        .with_context(|| format!("reading {FRONT_PAGE}"))?;
    let entries = parse_search_html(&String::from_utf8_lossy(&bytes));
    if entries.is_empty() {
        bail!("{INDEX_UNAVAILABLE}");
    }
    Ok(entries)
}

/// GET endpoint probed for JSON search results.
pub fn search_api_url(query: &str) -> String {
    format!(
        "https://skills.sh/api/search?q={}",
        encode_query_component(query)
    )
}

/// Server-rendered search page used as fallback.
pub fn search_html_url(query: &str) -> String {
    format!(
        "https://skills.sh/search?q={}",
        encode_query_component(query)
    )
}

/// Percent-encode a query component (unreserved chars stay literal).
pub fn encode_query_component(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Lenient parser for the skills.sh search API response: accepts a top-level
/// array or an object with a `skills`/`results`/`data`/`items` array; per-item
/// keys are matched loosely. Unparseable input yields an empty vec.
pub fn parse_search_json(body: &str) -> Vec<SkillEntry> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return Vec::new();
    };
    let items: Vec<&serde_json::Value> = match &value {
        serde_json::Value::Array(items) => items.iter().collect(),
        serde_json::Value::Object(map) => map
            .get("skills")
            .or_else(|| map.get("results"))
            .or_else(|| map.get("data"))
            .or_else(|| map.get("items"))
            .and_then(serde_json::Value::as_array)
            .map(|items| items.iter().collect())
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    items
        .iter()
        .filter_map(|item| entry_from_json(item))
        .collect()
}

fn entry_from_json(item: &serde_json::Value) -> Option<SkillEntry> {
    let obj = item.as_object()?;
    let str_field = |keys: &[&str]| {
        keys.iter()
            .find_map(|key| obj.get(*key))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    };
    let slug = str_field(&["source", "url", "href", "repo", "repository", "slug"]);
    let name = str_field(&["name", "skill", "title"])
        .or_else(|| slug.as_deref().and_then(entry_name_from_slug))?;
    let mut source = slug.unwrap_or_else(|| name.clone());
    while source.starts_with('/') {
        source.remove(0);
    }
    let installs = obj
        .get("installs")
        .or_else(|| obj.get("install_count"))
        .or_else(|| obj.get("installs_total"))
        .or_else(|| obj.get("downloads"))
        .and_then(|value| match value {
            serde_json::Value::Number(n) => n.as_u64(),
            serde_json::Value::String(s) => parse_installs(s),
            _ => None,
        });
    Some(SkillEntry {
        name: entry_name_from_slug(&name)?,
        description: str_field(&["description", "desc", "summary"]).unwrap_or_default(),
        source,
        installs,
    })
}

/// `"owner/repo/skill"` -> `"skill"`; returns the last non-empty segment.
fn entry_name_from_slug(slug: &str) -> Option<String> {
    let tail = slug.split('/').filter(|s| !s.is_empty()).next_back()?;
    let tail = tail.trim();
    if tail.is_empty() {
        None
    } else {
        Some(tail.to_string())
    }
}

/// Parse human install counts like `3.7M`, `955.4K`, `2B`, `1,181,452`, `42`.
pub fn parse_installs(raw: &str) -> Option<u64> {
    let cleaned = raw.trim().replace(',', "");
    if cleaned.is_empty() {
        return None;
    }
    let (number, multiplier) = match cleaned.chars().next_back()? {
        'k' | 'K' => (&cleaned[..cleaned.len() - 1], 1_000u64),
        'm' | 'M' => (&cleaned[..cleaned.len() - 1], 1_000_000),
        'b' | 'B' => (&cleaned[..cleaned.len() - 1], 1_000_000_000),
        _ => (cleaned.as_str(), 1),
    };
    let number = number.trim();
    if let Ok(value) = number.parse::<u64>() {
        return Some(value * multiplier);
    }
    let value = number.parse::<f64>().ok()?;
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    Some((value * multiplier as f64) as u64)
}

/// Lenient extraction of `/<owner>/<repo>/<skill>` links from the skills.sh
/// HTML search page. Install counts are not reliably colocated in the markup,
/// so entries come back with `installs: None`.
pub fn parse_search_html(body: &str) -> Vec<SkillEntry> {
    static SKILL_HREF_RE: OnceLock<Regex> = OnceLock::new();
    let re = SKILL_HREF_RE.get_or_init(|| {
        Regex::new(r#"href="/([A-Za-z0-9][A-Za-z0-9._-]*)/([A-Za-z0-9][A-Za-z0-9._-]*)/([A-Za-z0-9][A-Za-z0-9._-]*)""#)
            .unwrap()
    });
    let mut out: Vec<SkillEntry> = Vec::new();
    for cap in re.captures_iter(body) {
        let (owner, repo, skill) = (&cap[1], &cap[2], &cap[3]);
        let source = format!("skills.sh/{owner}/{repo}/{skill}");
        if out.iter().any(|entry| entry.source == source) {
            continue;
        }
        out.push(SkillEntry {
            name: skill.to_string(),
            description: String::new(),
            source,
            installs: None,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD_MD: &str =
        "---\nname: test-skill\ndescription: A test skill\n---\n# Test Skill\nBody.\n";

    #[test]
    fn resolve_source_accepts_skills_sh_urls() {
        for input in [
            "https://skills.sh/owner/repo/skill",
            "http://skills.sh/owner/repo/skill",
            "skills.sh/owner/repo/skill",
            "owner/repo/skill",
            "https://skills.sh/owner/repo/skill/",
        ] {
            assert_eq!(
                resolve_source(input).unwrap(),
                SkillSource::SkillsSh {
                    owner: "owner".into(),
                    repo: "repo".into(),
                    skill: "skill".into()
                },
                "input: {input}"
            );
        }
    }

    #[test]
    fn resolve_source_maps_github_shorthand() {
        assert_eq!(
            resolve_source("owner/repo").unwrap(),
            SkillSource::GitHubRepo {
                owner: "owner".into(),
                repo: "repo".into()
            }
        );
        assert_eq!(
            resolve_source("https://github.com/owner/repo").unwrap(),
            SkillSource::GitHubRepo {
                owner: "owner".into(),
                repo: "repo".into()
            }
        );
        assert_eq!(
            resolve_source("someone/thing.git").unwrap(),
            SkillSource::GitHubRepo {
                owner: "someone".into(),
                repo: "thing".into()
            }
        );
    }

    #[test]
    fn resolve_source_keeps_raw_urls() {
        let url = "https://raw.githubusercontent.com/owner/repo/main/skills/x/SKILL.md";
        assert_eq!(
            resolve_source(url).unwrap(),
            SkillSource::RawUrl(url.to_string())
        );
        assert_eq!(
            resolve_source("https://example.com/anything.md").unwrap(),
            SkillSource::RawUrl("https://example.com/anything.md".to_string())
        );
    }

    #[test]
    fn resolve_source_rejects_traversal_and_garbage() {
        for bad in [
            "",
            "   ",
            "justname",
            "owner/repo/../../etc/passwd",
            "../evil",
            "owner/../repo",
            "https://skills.sh/owner/../repo/skill",
            "a/b/c/d",
        ] {
            assert!(resolve_source(bad).is_err(), "expected error for {bad:?}");
        }
    }

    #[test]
    fn raw_urls_prefer_main_over_master_and_root_over_skills_prefix() {
        assert_eq!(
            raw_urls_for_skill("o", "r", "s"),
            vec![
                "https://raw.githubusercontent.com/o/r/main/s/SKILL.md",
                "https://raw.githubusercontent.com/o/r/main/skills/s/SKILL.md",
                "https://raw.githubusercontent.com/o/r/master/s/SKILL.md",
                "https://raw.githubusercontent.com/o/r/master/skills/s/SKILL.md",
            ]
        );
        assert_eq!(
            raw_urls_for_repo("o", "r"),
            vec![
                "https://raw.githubusercontent.com/o/r/main/SKILL.md",
                "https://raw.githubusercontent.com/o/r/master/SKILL.md",
            ]
        );
    }

    #[test]
    fn sanitize_skill_name_accepts_and_lowercases_valid_names() {
        assert_eq!(sanitize_skill_name("My-Skill").unwrap(), "my-skill");
        assert_eq!(sanitize_skill_name("  ok_name-2 ").unwrap(), "ok_name-2");
        assert_eq!(sanitize_skill_name("a1").unwrap(), "a1");
        let max_ok = format!("a{}", "x".repeat(63));
        assert_eq!(sanitize_skill_name(&max_ok).unwrap(), max_ok);
    }

    #[test]
    fn sanitize_skill_name_rejects_invalid_names() {
        for bad in [
            "a",     // too short
            "",      // empty
            "-lead", // must start with a letter or digit
            "_lead",
            "../evil", // path traversal
            "/abs",    // absolute path
            "has.dot", // dot outside the charset
            "has space",
            "n@me",
        ] {
            assert!(
                sanitize_skill_name(bad).is_err(),
                "expected rejection of {bad:?}"
            );
        }
        assert!(sanitize_skill_name(&format!("a{}", "x".repeat(64))).is_err());
    }

    #[test]
    fn check_requirements_reports_unset_env_vars() {
        // PATH is set on every platform; the VARYNTH_TEST_* names stay unset.
        let md = "---\nname: x\ndescription: y\nenv:\n  - PATH\n  - VARYNTH_TEST_UNSET_A\n---\nUses $$VARYNTH_TEST_UNSET_B and $$PATH and $$VARYNTH_TEST_UNSET_B again.\n";
        let missing = check_requirements(md);
        assert!(missing.contains(&"VARYNTH_TEST_UNSET_A".to_string()));
        assert!(missing.contains(&"VARYNTH_TEST_UNSET_B".to_string()));
        assert!(!missing.contains(&"PATH".to_string()));
        assert_eq!(
            missing
                .iter()
                .filter(|name| *name == "VARYNTH_TEST_UNSET_B")
                .count(),
            1,
            "duplicates must be reported once"
        );
    }

    #[test]
    fn check_requirements_skips_non_identifiers_and_frontmatter() {
        let md = "---\nname: x\nenv:\n  - not an identifier\n---\nNo tokens here.\n";
        assert!(check_requirements(md).is_empty());
    }

    #[test]
    fn parse_installs_handles_suffixes_and_commas() {
        assert_eq!(parse_installs("3.7M"), Some(3_700_000));
        assert_eq!(parse_installs("955.4K"), Some(955_400));
        assert_eq!(parse_installs("1,181,452"), Some(1_181_452));
        assert_eq!(parse_installs("42"), Some(42));
        assert_eq!(parse_installs("2B"), Some(2_000_000_000));
        assert_eq!(parse_installs("n/a"), None);
        assert_eq!(parse_installs(""), None);
    }

    #[test]
    fn parse_search_json_reads_array_and_object_payloads() {
        let array = r#"[
            {"name": "find-skills", "description": "Find skills", "slug": "vercel-labs/skills/find-skills", "installs": 3700000},
            {"slug": "anthropics/skills/frontend-design", "installs": "955.4K"}
        ]"#;
        let entries = parse_search_json(array);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "find-skills");
        assert_eq!(entries[0].source, "vercel-labs/skills/find-skills");
        assert_eq!(entries[0].installs, Some(3_700_000));
        assert_eq!(entries[1].name, "frontend-design");
        assert_eq!(entries[1].installs, Some(955_400));

        let object = r#"{"skills": [{"name": "grill-me", "description": "Grill the user", "source": "mattpocock/skills/grill-me"}]}"#;
        let entries = parse_search_json(object);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].source, "mattpocock/skills/grill-me");
        assert_eq!(entries[0].installs, None);

        assert!(parse_search_json("not json at all").is_empty());
        assert!(parse_search_json("{\"other\": []}").is_empty());
    }

    #[test]
    fn parse_search_html_extracts_owner_repo_skill_links() {
        let html = r#"<html><body>
            <a href="/vercel-labs/skills/find-skills">find-skills</a> 3.7M installs
            <a href="/vercel-labs/skills/find-skills">dup</a>
            <a href="/mattpocock/skills/grill-me">grill-me</a> 1.3M
            <a href="/agent/claude-code">agent</a>
            <a href="/topic/react">topic</a>
            <a href="/docs/api">docs</a>
        </body></html>"#;
        let entries = parse_search_html(html);
        assert_eq!(
            entries.len(),
            2,
            "duplicates and 2-segment links are dropped"
        );
        assert_eq!(entries[0].name, "find-skills");
        assert_eq!(
            entries[0].source,
            "skills.sh/vercel-labs/skills/find-skills"
        );
        assert_eq!(entries[0].installs, None);
        assert_eq!(entries[1].name, "grill-me");
    }

    #[test]
    fn encode_query_component_escapes_unreserved_only() {
        assert_eq!(encode_query_component("rust cli"), "rust%20cli");
        assert_eq!(encode_query_component("a&b=c/d?e"), "a%26b%3Dc%2Fd%3Fe");
        assert_eq!(encode_query_component("safe-._~x9"), "safe-._~x9");
        assert_eq!(encode_query_component("café"), "caf%C3%A9");
    }

    #[test]
    fn search_urls_embed_encoded_query() {
        assert_eq!(
            search_api_url("rust cli"),
            "https://skills.sh/api/search?q=rust%20cli"
        );
        assert_eq!(
            search_html_url("rust cli"),
            "https://skills.sh/search?q=rust%20cli"
        );
    }

    #[test]
    fn install_roundtrip_refuses_overwrite_then_forces() {
        let dir = tempfile::tempdir().unwrap();
        let report = install_raw(GOOD_MD, dir.path(), false).unwrap();
        assert_eq!(report.name, "test-skill");
        assert_eq!(report.path, dir.path().join("test-skill").join("SKILL.md"));
        assert_eq!(
            std::fs::read_to_string(&report.path).unwrap(),
            GOOD_MD,
            "the raw document is stored verbatim"
        );
        assert!(report.warnings.is_empty());

        let err = install_raw(GOOD_MD, dir.path(), false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("already installed"), "got: {err}");

        let updated = GOOD_MD.replace("A test skill", "A better test skill");
        let report = install_raw(&updated, dir.path(), true).unwrap();
        assert_eq!(
            std::fs::read_to_string(&report.path).unwrap(),
            updated,
            "force overwrites the stored document"
        );
    }

    #[test]
    fn install_raw_warns_on_missing_description_and_env() {
        let dir = tempfile::tempdir().unwrap();
        let md = "---\nname: envy-skill\n---\nNeeds $$VARYNTH_TEST_UNSET_C.\n";
        let report = install_raw(md, dir.path(), false).unwrap();
        assert_eq!(report.warnings.len(), 2);
        assert!(report.warnings[0].contains("description"));
        assert!(report.warnings[1].contains("VARYNTH_TEST_UNSET_C"));
    }

    #[test]
    fn install_raw_rejects_invalid_documents() {
        let dir = tempfile::tempdir().unwrap();
        for bad in [
            "# no frontmatter\n",
            "---\ndescription: no name\n---\nbody\n",
            "---\nname: ../evil\n---\nbody\n",
            "---\nname: x\n---\nbody\n", // name too short
        ] {
            assert!(
                install_raw(bad, dir.path(), false).is_err(),
                "expected rejection of {bad:?}"
            );
        }
    }

    #[test]
    fn list_installed_reads_frontmatter_and_skips_invalid() {
        let dir = tempfile::tempdir().unwrap();
        for (dirname, md) in [
            (
                "b-skill",
                "---\nname: b-skill\ndescription: second\n---\nbody\n",
            ),
            (
                "a-skill",
                "---\nname: a-skill\ndescription: first\n---\nbody\n",
            ),
        ] {
            let skill_dir = dir.path().join(dirname);
            std::fs::create_dir_all(&skill_dir).unwrap();
            std::fs::write(skill_dir.join("SKILL.md"), md).unwrap();
        }
        let broken = dir.path().join("broken");
        std::fs::create_dir_all(&broken).unwrap();
        std::fs::write(broken.join("SKILL.md"), "no frontmatter").unwrap();

        let entries = list_installed(dir.path());
        assert_eq!(entries.len(), 2, "invalid skills are skipped");
        assert_eq!(entries[0].name, "a-skill");
        assert_eq!(entries[1].name, "b-skill");
        assert_eq!(entries[0].description, "first");
        assert_eq!(entries[0].installs, None);
        assert!(
            entries[0].source.ends_with("a-skill/SKILL.md")
                || entries[0].source.ends_with("a-skill\\SKILL.md")
        );

        assert!(list_installed(&dir.path().join("missing")).is_empty());
    }
}
