//! Workspace file listing, fuzzy `@` mention matching, and ACP prompt block expansion.
//! No UI dependencies; used by both `app` (completion) and `session` (prompt dispatch).

use std::fs;
use std::path::{Component, Path};
use std::process::Command;

use crate::agent::acp::PromptBlock;

/// Maximum number of files collected for `@` autocomplete.
const MAX_LISTED_FILES: usize = 10_000;
/// Maximum file size (in bytes) to inline as an ACP `resource` when the agent
/// advertises `embeddedContext`; larger or binary files use `resource_link`.
const MAX_EMBED_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMatch {
    pub path: String,
    /// Character indices in `path` that matched the query (for highlighting).
    pub positions: Vec<usize>,
    pub score: i64,
}

/// List workspace files relative to `root` using forward slashes.
/// Prefers `git ls-files` (fast and respects `.gitignore`), falling back to a
/// bounded directory walk outside git repos.
pub fn list_files(root: &Path) -> Vec<String> {
    if let Some(files) = git_list_files(root) {
        return files;
    }
    walk_list_files(root)
}

fn git_list_files(root: &Path) -> Option<Vec<String>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let mut files: Vec<String> = out
        .stdout
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .filter_map(|s| std::str::from_utf8(s).ok())
        .map(str::to_string)
        .collect();
    files.sort();
    files.dedup();
    files.truncate(MAX_LISTED_FILES);
    Some(files)
}

fn walk_list_files(root: &Path) -> Vec<String> {
    const SKIP_DIRS: &[&str] = &[
        ".git",
        "target",
        "node_modules",
        ".hg",
        ".svn",
        "dist",
        "build",
    ];
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if files.len() >= MAX_LISTED_FILES {
            break;
        }
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for entry in rd.flatten() {
            if files.len() >= MAX_LISTED_FILES {
                break;
            }
            let path = entry.path();
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                if name_str.starts_with('.') || SKIP_DIRS.contains(&name_str.as_ref()) {
                    continue;
                }
                stack.push(path);
            } else if ft.is_file() {
                if let Ok(rel) = path.strip_prefix(root) {
                    let s = rel
                        .components()
                        .filter_map(|c| match c {
                            Component::Normal(p) => Some(p.to_string_lossy()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("/");
                    if !s.is_empty() {
                        files.push(s);
                    }
                }
            }
        }
    }
    files.sort();
    files
}

/// Fuzzy-filter `files` against `query`, returning up to `limit` ranked matches.
pub fn fuzzy_filter(files: &[String], query: &str, limit: usize) -> Vec<FileMatch> {
    if limit == 0 {
        return Vec::new();
    }
    let query = query.trim();
    if query.is_empty() {
        return files
            .iter()
            .take(limit)
            .map(|p| FileMatch {
                path: p.clone(),
                positions: Vec::new(),
                score: 0,
            })
            .collect();
    }
    let needle: Vec<char> = query.chars().flat_map(char::to_lowercase).collect();
    let mut scored: Vec<FileMatch> = files
        .iter()
        .filter_map(|path| score_match(path, &needle))
        .collect();
    scored.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.path.cmp(&b.path)));
    scored.truncate(limit);
    scored
}

fn score_match(path: &str, needle: &[char]) -> Option<FileMatch> {
    if needle.is_empty() {
        return Some(FileMatch {
            path: path.to_string(),
            positions: Vec::new(),
            score: 0,
        });
    }
    let chars: Vec<char> = path.chars().collect();
    let lowered: Vec<char> = chars
        .iter()
        .map(|c| c.to_lowercase().next().unwrap_or(*c))
        .collect();
    let basename_start = path.rfind('/').map_or(0, |b| path[..=b].chars().count());

    // Try matching starting inside the basename first; if that succeeds it
    // produces a tighter, higher-scoring highlight than scattering across dirs.
    let positions = find_subsequence(&lowered, needle, basename_start)
        .or_else(|| find_subsequence(&lowered, needle, 0))?;

    let mut score: i64 = 0;
    let mut prev: Option<usize> = None;
    let all_in_basename = positions.first().is_some_and(|&p| p >= basename_start);
    if all_in_basename {
        score += 40;
    }
    for &idx in &positions {
        score += 10;
        if idx == 0 || idx == basename_start {
            score += 25;
        } else if matches!(chars[idx - 1], '/' | '_' | '-' | '.') {
            score += 15;
        }
        if let Some(p) = prev {
            if idx == p + 1 {
                score += 12;
            } else {
                score -= (idx - p - 1).min(10) as i64;
            }
        }
        prev = Some(idx);
    }
    // Prefer shorter paths when match quality ties.
    score -= (chars.len() as i64).min(200) / 4;

    Some(FileMatch {
        path: path.to_string(),
        positions,
        score,
    })
}

fn find_subsequence(haystack: &[char], needle: &[char], from: usize) -> Option<Vec<usize>> {
    let mut positions = Vec::with_capacity(needle.len());
    let mut ni = 0;
    for (hi, &ch) in haystack.iter().enumerate().skip(from) {
        if ch == needle[ni] {
            positions.push(hi);
            ni += 1;
            if ni == needle.len() {
                return Some(positions);
            }
        }
    }
    None
}

/// If the cursor sits inside an `@query` token on the current line, returns
/// `(at_char_index, query)`.
pub fn active_mention(chars: &[char], cursor: usize) -> Option<(usize, String)> {
    let cursor = cursor.min(chars.len());
    let mut i = cursor;
    while i > 0 {
        let ch = chars[i - 1];
        if ch.is_whitespace() {
            return None;
        }
        if ch == '@' {
            let at = i - 1;
            if at == 0 || chars[at - 1].is_whitespace() {
                let query: String = chars[at + 1..cursor].iter().collect();
                return Some((at, query));
            }
            return None;
        }
        i -= 1;
    }
    None
}

/// Format a completed mention to insert into the prompt editor.
pub fn format_mention(path: &str) -> String {
    if path.chars().any(char::is_whitespace) {
        format!("@\"{path}\" ")
    } else {
        format!("@{path} ")
    }
}

/// Expand `@path` (and `@"path with spaces"`) mentions in `text` into ACP
/// prompt content blocks. Existing files become `Resource` (when `embed` is
/// true and the file is small valid UTF-8) or `ResourceLink`; unknown tokens
/// stay untouched as plain text.
pub fn expand_mentions(text: &str, cwd: &Path, embed: bool) -> Vec<PromptBlock> {
    let mut blocks: Vec<PromptBlock> = Vec::new();
    let mut text_buf = String::new();
    let bytes = text.as_bytes();
    let mut i = 0;

    while i < text.len() {
        let at_boundary = i == 0
            || text[..i]
                .chars()
                .next_back()
                .is_some_and(char::is_whitespace);
        if at_boundary && bytes[i] == b'@' {
            if let Some((raw_path, consumed, trailing)) = parse_mention_token(&text[i..]) {
                if let Some((block, extra_tail)) = resolve_mention(raw_path, cwd, embed) {
                    if !text_buf.is_empty() {
                        blocks.push(PromptBlock::Text(std::mem::take(&mut text_buf)));
                    }
                    blocks.push(block);
                    text_buf.push_str(extra_tail);
                    text_buf.push_str(trailing);
                    i += consumed;
                    continue;
                }
            }
        }
        let ch = text[i..].chars().next().unwrap_or(' ');
        text_buf.push(ch);
        i += ch.len_utf8();
    }

    if !text_buf.is_empty() || blocks.is_empty() {
        blocks.push(PromptBlock::Text(text_buf));
    }
    blocks
}

/// Parse `@"path with spaces"` or `@path` at the start of `s`.
/// Returns `(candidate_path, bytes_consumed, trailing_unconsumed_slice)`.
fn parse_mention_token(s: &str) -> Option<(&str, usize, &str)> {
    let rest = s.strip_prefix('@')?;
    if let Some(after_quote) = rest.strip_prefix('"') {
        let end = after_quote.find('"')?;
        let path = &after_quote[..end];
        if path.is_empty() {
            return None;
        }
        // `@` + `"` + path + `"`
        return Some((path, 2 + end + 1, ""));
    }
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    if end == 0 {
        return None;
    }
    Some((&rest[..end], 1 + end, ""))
}

fn resolve_mention<'a>(raw: &'a str, cwd: &Path, embed: bool) -> Option<(PromptBlock, &'a str)> {
    // Try exact path first; if absent, strip trailing sentence punctuation
    // (e.g. `look at @src/main.rs.`).
    let mut split = raw.len();
    loop {
        let candidate = &raw[..split];
        if !candidate.is_empty() {
            if let Some(block) = build_file_block(candidate, cwd, embed) {
                return Some((block, &raw[split..]));
            }
        }
        let last = candidate.chars().next_back()?;
        if matches!(last, '.' | ',' | ';' | ':' | ')' | ']' | '!' | '?') {
            split -= last.len_utf8();
        } else {
            return None;
        }
    }
}

fn build_file_block(rel: &str, cwd: &Path, embed: bool) -> Option<PromptBlock> {
    let joined = cwd.join(rel);
    let meta = fs::metadata(&joined).ok()?;
    if !meta.is_file() {
        return None;
    }
    let abs = fs::canonicalize(&joined).unwrap_or(joined);
    let uri = file_uri(&abs);
    let mime_type = guess_mime(rel).map(str::to_string);
    if embed && meta.len() <= MAX_EMBED_BYTES {
        if let Ok(content) = fs::read_to_string(&abs) {
            return Some(PromptBlock::Resource {
                uri,
                text: content,
                mime_type,
            });
        }
    }
    Some(PromptBlock::ResourceLink {
        uri,
        name: rel.to_string(),
        mime_type,
    })
}

/// Encode a filesystem path as a `file://` URI, percent-encoding reserved bytes.
pub fn file_uri(path: &Path) -> String {
    let raw = path.to_string_lossy();
    let mut out = String::from("file://");
    if !raw.starts_with('/') {
        out.push('/');
    }
    for &b in raw.as_bytes() {
        let safe = b.is_ascii_alphanumeric() || matches!(b, b'/' | b'-' | b'_' | b'.' | b'~');
        if safe {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn guess_mime(path: &str) -> Option<&'static str> {
    let ext = Path::new(path).extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "rs" => "text/x-rust",
        "ts" | "tsx" => "text/typescript",
        "js" | "jsx" | "mjs" | "cjs" => "text/javascript",
        "py" => "text/x-python",
        "go" => "text/x-go",
        "c" | "h" => "text/x-c",
        "cpp" | "cc" | "hpp" => "text/x-c++",
        "java" => "text/x-java",
        "json" => "application/json",
        "toml" => "application/toml",
        "yaml" | "yml" => "application/yaml",
        "md" | "markdown" => "text/markdown",
        "org" | "txt" => "text/plain",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "sh" | "bash" | "zsh" => "text/x-shellscript",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_mention_detects_at_token_at_cursor() {
        let buf: Vec<char> = "check @src/ma".chars().collect();
        assert_eq!(active_mention(&buf, buf.len()), Some((6, "src/ma".into())));

        let buf: Vec<char> = "@".chars().collect();
        assert_eq!(active_mention(&buf, 1), Some((0, String::new())));

        // Email-like `@` inside a word must not trigger.
        let buf: Vec<char> = "foo@bar".chars().collect();
        assert_eq!(active_mention(&buf, buf.len()), None);

        // Space after mention closes it.
        let buf: Vec<char> = "@src/main.rs ".chars().collect();
        assert_eq!(active_mention(&buf, buf.len()), None);
    }

    #[test]
    fn fuzzy_filter_prefers_basename_and_exact_prefix() {
        let files: Vec<String> = vec![
            "docs/main_guide.md".into(),
            "src/ui/markdown.rs".into(),
            "src/main.rs".into(),
            "Cargo.toml".into(),
        ];
        let m = fuzzy_filter(&files, "main", 10);
        assert_eq!(m[0].path, "src/main.rs");
        assert!(m.iter().any(|x| x.path == "docs/main_guide.md"));
        assert!(!m.iter().any(|x| x.path == "Cargo.toml"));

        let m = fuzzy_filter(&files, "md", 10);
        assert!(m.iter().any(|x| x.path == "src/ui/markdown.rs"));
    }

    #[test]
    fn expand_mentions_resolves_resource_and_resource_link_and_keeps_punctuation() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        fs::write(dir.path().join("my file.txt"), "hello\n").unwrap();

        // Embedded context on: small UTF-8 files become `Resource`, trailing `.` stays text.
        let blocks = expand_mentions(
            "Explain @src/main.rs. Also @\"my file.txt\" and @missing.rs",
            dir.path(),
            true,
        );
        assert_eq!(blocks.len(), 5);
        assert_eq!(blocks[0], PromptBlock::Text("Explain ".into()));
        match &blocks[1] {
            PromptBlock::Resource {
                uri,
                text,
                mime_type,
            } => {
                assert!(uri.ends_with("/src/main.rs"), "{uri}");
                assert_eq!(text, "fn main() {}\n");
                assert_eq!(mime_type.as_deref(), Some("text/x-rust"));
            }
            other => panic!("expected Resource, got {other:?}"),
        }
        assert_eq!(blocks[2], PromptBlock::Text(". Also ".into()));
        match &blocks[3] {
            PromptBlock::Resource { uri, text, .. } => {
                assert!(uri.ends_with("/my%20file.txt"), "{uri}");
                assert_eq!(text, "hello\n");
            }
            other => panic!("expected Resource, got {other:?}"),
        }
        assert_eq!(blocks[4], PromptBlock::Text(" and @missing.rs".into()));

        // Embedded context off: uses `ResourceLink`.
        let blocks = expand_mentions("See @src/main.rs", dir.path(), false);
        assert_eq!(blocks.len(), 2);
        match &blocks[1] {
            PromptBlock::ResourceLink {
                uri,
                name,
                mime_type,
            } => {
                assert!(uri.ends_with("/src/main.rs"), "{uri}");
                assert_eq!(name, "src/main.rs");
                assert_eq!(mime_type.as_deref(), Some("text/x-rust"));
            }
            other => panic!("expected ResourceLink, got {other:?}"),
        }
    }

    #[test]
    fn list_files_works_in_plain_directory_and_skips_ignored_dirs() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::create_dir_all(dir.path().join("target")).unwrap();
        fs::write(dir.path().join("src/lib.rs"), "").unwrap();
        fs::write(dir.path().join("target/out.bin"), "").unwrap();

        let files = list_files(dir.path());
        assert_eq!(files, vec!["src/lib.rs"]);
    }
}
