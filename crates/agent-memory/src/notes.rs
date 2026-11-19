//! Thin `MEMORY.md` topic index helpers and bounded session snapshot pinning.
//!
//! Keeps a small human-readable `MEMORY.md` index alongside raw-history
//! retrieval, strictly bounded to [`MAX_BYTES`] and [`MAX_LINES`] without
//! rewriting or replacing immutable raw history.

use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

use crate::raw::sha256_hex;

/// Maximum UTF-8 byte length of a bounded pinned memory section.
pub const MAX_BYTES: usize = 4096;
/// Maximum line count of a bounded pinned memory section.
pub const MAX_LINES: usize = 150;

const MARKER: &str = "[Memory truncated; consult the configured source for full text.]\n";
const FOOTER: &str = "</learned_memory>\n";

/// Immutable session snapshot of a bounded memory section.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemoryPin {
    pub session_id: String,
    /// Digest of the exact bounded section, independent of later file edits.
    pub sha256: String,
    pub section: String,
}

/// Summary metadata for one Markdown topic file under `memory/`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemoryTopicEntry {
    pub slug: String,
    pub filename: String,
    pub description: String,
    pub updated_at_ms: i64,
    pub bytes: usize,
}

/// Escape `<learned_memory>` / `</learned_memory>` tags in untrusted text so
/// injected content cannot close the prompt delimiter.
pub fn sanitize_memory_delimiters(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'<' {
            let rest = &input[i + 1..];
            let rest_trim = rest.strip_prefix('/').unwrap_or(rest);
            if rest_trim
                .get(..14)
                .is_some_and(|s| s.eq_ignore_ascii_case("learned_memory"))
            {
                out.push_str("&lt;");
                i += 1;
                continue;
            }
        }
        let ch = input[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Render a bounded `<learned_memory>` section strictly within [`MAX_BYTES`]
/// and [`MAX_LINES`], appending a visible truncation marker if shortened.
pub fn bounded_section(input: &str) -> String {
    const HEADER: &str = "<learned_memory>\n# Learned memory (session snapshot)\n";
    let base = HEADER.len() + FOOTER.len();
    let base_lines = HEADER.lines().count() + FOOTER.lines().count();
    let sanitized = sanitize_memory_delimiters(input);
    let normalized = if sanitized.is_empty() || sanitized.ends_with('\n') {
        sanitized
    } else {
        format!("{sanitized}\n")
    };
    if base + normalized.len() <= MAX_BYTES && base_lines + normalized.lines().count() <= MAX_LINES
    {
        return format!("{HEADER}{normalized}{FOOTER}");
    }
    let available = MAX_BYTES - base - MARKER.len();
    let lines = MAX_LINES - base_lines - 1;
    let mut end = normalized.len().min(available);
    while !normalized.is_char_boundary(end) {
        end -= 1;
    }
    let mut count = 0;
    for (i, b) in normalized.bytes().enumerate().take(end) {
        if b == b'\n' {
            count += 1;
            if count == lines {
                end = i + 1;
                break;
            }
        }
    }
    if end > 0 && !normalized[..end].ends_with('\n') {
        while end + 1 > available {
            end -= 1;
            while !normalized.is_char_boundary(end) {
                end -= 1;
            }
        }
    }
    let body = &normalized[..end];
    let separator = if body.is_empty() || body.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    format!("{HEADER}{body}{separator}{MARKER}{FOOTER}")
}

/// Write `bytes` atomically to `path` via a unique sibling temporary file and `sync_all`.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("missing parent"))?;
    fs::create_dir_all(parent)?;
    let temp = parent.join(format!(".memory-{}.tmp", uuid::Uuid::now_v7()));
    let result = (|| {
        let mut out = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp)?;
        out.write_all(bytes)?;
        out.sync_all()?;
        fs::rename(&temp, path)?;
        #[cfg(unix)]
        {
            fs::File::open(parent)?.sync_all()?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}

/// Read an existing session memory pin and validate its SHA-256 and size bounds.
pub fn read_pin(agent_root: &Path, session_id: &str) -> io::Result<MemoryPin> {
    let path = agent_root
        .join("sessions")
        .join(sha256_hex(session_id.as_bytes()))
        .join("memory-pin.json");
    let bytes = fs::read(&path)?;
    let pin: MemoryPin = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    if pin.session_id != session_id
        || pin.sha256 != sha256_hex(pin.section.as_bytes())
        || pin.section.len() > MAX_BYTES
        || pin.section.lines().count() > MAX_LINES
    {
        return Err(io::Error::other("invalid persisted memory pin"));
    }
    Ok(pin)
}

/// Snapshot `<agent_root>/memory/MEMORY.md` once for `session_id`.
pub fn pin_session(agent_root: &Path, session_id: &str) -> io::Result<MemoryPin> {
    pin_session_from_source(agent_root, session_id, Some(Path::new("memory/MEMORY.md")))
}

/// Snapshot a caller-selected file (relative to `agent_root`), or an empty section
/// when `source` is `None`. Existing session pins are read back unchanged.
pub fn pin_session_from_source(
    agent_root: &Path,
    session_id: &str,
    source: Option<&Path>,
) -> io::Result<MemoryPin> {
    if session_id.trim().is_empty() {
        return Err(io::Error::other("empty session identity"));
    }
    match read_pin(agent_root, session_id) {
        Ok(pin) => return Ok(pin),
        Err(e) if e.kind() == io::ErrorKind::NotFound => (),
        Err(e) => return Err(e),
    }
    let path = agent_root
        .join("sessions")
        .join(sha256_hex(session_id.as_bytes()))
        .join("memory-pin.json");
    let input = match source.map(|source| fs::File::open(agent_root.join(source))) {
        None => String::new(),
        Some(Ok(file)) => {
            let mut bytes = Vec::new();
            file.take((MAX_BYTES + 4) as u64).read_to_end(&mut bytes)?;
            match std::str::from_utf8(&bytes) {
                Ok(text) => text.to_owned(),
                Err(error) if error.error_len().is_none() && bytes.len() == MAX_BYTES + 4 => {
                    std::str::from_utf8(&bytes[..error.valid_up_to()])
                        .unwrap()
                        .to_owned()
                }
                Err(error) => return Err(io::Error::new(io::ErrorKind::InvalidData, error)),
            }
        }
        Some(Err(e)) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Some(Err(e)) => return Err(e),
    };
    let section = bounded_section(&input);
    let pin = MemoryPin {
        session_id: session_id.into(),
        sha256: sha256_hex(section.as_bytes()),
        section,
    };
    atomic_write(&path, &serde_json::to_vec(&pin).map_err(io::Error::other)?)?;
    Ok(pin)
}

/// Normalize a raw topic name into a safe lowercase slug (rejecting `MEMORY` and empty inputs).
pub fn sanitize_topic_slug(raw: &str) -> io::Result<String> {
    let trimmed = raw.trim();
    let without_ext = trimmed
        .strip_suffix(".md")
        .or_else(|| trimmed.strip_suffix(".MD"))
        .unwrap_or(trimmed)
        .trim();
    let mut slug = String::with_capacity(without_ext.len());
    let mut prev_dash = false;
    for ch in without_ext.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if (ch == '-' || ch == '_' || ch == ' ' || ch == '.')
            && !slug.is_empty()
            && !prev_dash
        {
            slug.push('-');
            prev_dash = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.len() > 80 {
        slug.truncate(80);
        while slug.ends_with('-') {
            slug.pop();
        }
    }
    if slug.is_empty() || slug.eq_ignore_ascii_case("memory") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "memory topic slug must be a non-empty alphanumeric identifier other than MEMORY",
        ));
    }
    Ok(slug)
}

fn truncate_utf8_chars(input: &str, max_bytes: usize) -> &str {
    if input.len() <= max_bytes {
        return input;
    }
    let mut end = max_bytes;
    while end > 0 && !input.is_char_boundary(end) {
        end -= 1;
    }
    &input[..end]
}

fn truncate_summary_clean(input: &str, max_bytes: usize) -> String {
    let trimmed = input.trim();
    if trimmed.len() <= max_bytes {
        return trimmed.to_string();
    }
    if max_bytes <= 3 {
        return truncate_utf8_chars(trimmed, max_bytes).to_string();
    }
    let limit = max_bytes - 3;
    let slice = truncate_utf8_chars(trimmed, limit);
    let min_snap = (limit * 2) / 3;
    let snapped = slice
        .char_indices()
        .rev()
        .find(|(idx, ch)| {
            *idx >= min_snap
                && matches!(
                    *ch,
                    ' ' | ',' | ';' | ':' | '.' | '—' | '–' | '。' | '，' | '；' | '、' | '：'
                )
        })
        .map(|(idx, _)| slice[..idx].trim_end_matches([' ', ',', ';', ':', '-', '—', '–']))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| slice.trim_end());
    format!("{snapped}...")
}

fn clean_single_line_description(raw: &str) -> String {
    let sanitized = sanitize_memory_delimiters(raw);
    let cleaned: String = sanitized
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect();
    let one_line = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_utf8_chars(&one_line, 160).to_string()
}

/// Parse YAML frontmatter fields (`name`, `description`, `created_at_ms`, `updated_at_ms`)
/// and return the remaining Markdown body.
pub fn parse_topic_frontmatter_full<'a>(
    slug: &str,
    text: &'a str,
) -> (String, String, Option<i64>, Option<i64>, &'a str) {
    let mut name = slug.to_string();
    let mut description = String::new();
    let mut created_at_ms = None;
    let mut updated_at_ms = None;
    let trimmed = text.trim_start();
    let body = if let Some(after_open) = trimmed.strip_prefix("---\n")
        && let Some(end_idx) = after_open.find("\n---")
    {
        let fm = &after_open[..end_idx];
        for line in fm.lines() {
            let l = line.trim();
            if let Some(rest) = l.strip_prefix("name:") {
                let v = rest.trim().trim_matches('"').trim_matches('\'').trim();
                if !v.is_empty() {
                    name = v.to_string();
                }
            } else if let Some(rest) = l.strip_prefix("description:") {
                let v = rest.trim().trim_matches('"').trim_matches('\'').trim();
                if !v.is_empty() {
                    description = v.to_string();
                }
            } else if let Some(rest) = l.strip_prefix("created_at_ms:") {
                created_at_ms = rest.trim().parse::<i64>().ok();
            } else if let Some(rest) = l.strip_prefix("updated_at_ms:") {
                updated_at_ms = rest.trim().parse::<i64>().ok();
            }
        }
        let body = after_open[end_idx + 4..].trim_start_matches(['-', '\r', '\n']);
        if description.is_empty() {
            for line in body.lines() {
                let clean = line
                    .trim()
                    .trim_start_matches('#')
                    .trim_start_matches('-')
                    .trim();
                if !clean.is_empty() {
                    description = clean.to_string();
                    break;
                }
            }
        }
        body
    } else {
        for line in trimmed.lines() {
            let clean = line
                .trim()
                .trim_start_matches('#')
                .trim_start_matches('-')
                .trim();
            if !clean.is_empty() {
                description = clean.to_string();
                break;
            }
        }
        trimmed
    };
    let clipped = clean_single_line_description(&description);
    (name, clipped, created_at_ms, updated_at_ms, body)
}

/// Extract `(name, description)` from topic Markdown frontmatter.
pub fn parse_topic_frontmatter(slug: &str, text: &str) -> (String, String) {
    let (name, description, _, _, _) = parse_topic_frontmatter_full(slug, text);
    (name, description)
}

/// List all Markdown topic files under `<agent_root>/memory/` (excluding `MEMORY.md` and hidden files).
pub fn list_memory_topics(agent_root: &Path) -> io::Result<Vec<MemoryTopicEntry>> {
    let mem_dir = agent_root.join("memory");
    if !mem_dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut entries = Vec::new();
    for entry in fs::read_dir(&mem_dir)? {
        let entry = entry?;
        let path = entry.path();
        let Some(fname) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if fname.starts_with('.')
            || fname.eq_ignore_ascii_case("MEMORY.md")
            || !fname.ends_with(".md")
            || !path.is_file()
        {
            continue;
        }
        let slug = fname.strip_suffix(".md").unwrap_or(fname).to_string();
        let meta = fs::metadata(&path)?;
        let fs_mtime_ms = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| i64::try_from(d.as_millis()).unwrap_or(0))
            .unwrap_or(0);
        let raw = fs::read(&path).unwrap_or_default();
        let text = String::from_utf8_lossy(&raw);
        let (_, description, _, fm_updated_ms, _) = parse_topic_frontmatter_full(&slug, &text);
        let updated_at_ms = fm_updated_ms.filter(|&ts| ts > 0).unwrap_or(fs_mtime_ms);
        entries.push(MemoryTopicEntry {
            slug,
            filename: fname.to_string(),
            description,
            updated_at_ms,
            bytes: raw.len(),
        });
    }
    entries.sort_by(|a, b| a.slug.cmp(&b.slug));
    Ok(entries)
}

fn ensure_private_memory_dir(agent_root: &Path) -> io::Result<PathBuf> {
    let mem_dir = agent_root.join("memory");
    fs::create_dir_all(agent_root)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _};
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder.create(&mem_dir)?;
        let meta = fs::symlink_metadata(&mem_dir)?;
        if !meta.file_type().is_symlink() && (meta.mode() & 0o007) != 0 {
            let safe_mode = if meta.uid() == meta.gid() {
                0o700
            } else {
                meta.mode() & 0o7770
            };
            fs::set_permissions(&mem_dir, fs::Permissions::from_mode(safe_mode))?;
        }
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(&mem_dir)?;
    }
    Ok(mem_dir)
}

/// Rebuild `<agent_root>/memory/MEMORY.md` so all topic `.md` files are indexed
/// while keeping `bounded_section(&memory_md)` strictly within [`MAX_BYTES`]
/// and [`MAX_LINES`].
pub fn rebuild_memory_index(
    agent_root: &Path,
    agent_name: &str,
    role: Option<&str>,
) -> io::Result<String> {
    let mem_dir = ensure_private_memory_dir(agent_root)?;
    let topics = list_memory_topics(agent_root)?;
    let role_suffix = role
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|r| format!(" ({r})"))
        .unwrap_or_default();
    let header = format!("# {agent_name}{role_suffix} — Memory Index\n");

    let wrapper_overhead = bounded_section("").len() + 64;
    let max_body_bytes = MAX_BYTES.saturating_sub(wrapper_overhead + header.len());
    let max_body_lines = MAX_LINES.saturating_sub(header.lines().count() + 6);
    let n = topics.len();
    let mut body = String::new();

    let format_full_line = |t: &MemoryTopicEntry, desc_budget: usize| -> String {
        let prefix = format!("- [{}]({}) — ", t.slug, t.filename);
        let desc = if !t.description.is_empty() {
            truncate_summary_clean(&t.description, desc_budget)
        } else {
            "topic".to_string()
        };
        format!("{prefix}{desc}\n")
    };

    let format_archival_section = |archival: &[&MemoryTopicEntry]| -> String {
        if archival.is_empty() {
            return String::new();
        }
        let mut out = String::from("## Archival Topics\n");
        let mut groups: std::collections::BTreeMap<String, Vec<String>> =
            std::collections::BTreeMap::new();
        for t in archival {
            let prefix = t
                .slug
                .split('-')
                .next()
                .filter(|s| !s.is_empty())
                .unwrap_or("general")
                .to_string();
            groups.entry(prefix).or_default().push(t.slug.clone());
        }
        let mut singletons = Vec::new();
        for (group, mut slugs) in groups {
            slugs.sort();
            if slugs.len() >= 2 {
                for chunk in slugs.chunks(5) {
                    out.push_str(&format!("- **{group}**: {}\n", chunk.join(", ")));
                }
            } else {
                singletons.extend(slugs);
            }
        }
        singletons.sort();
        for chunk in singletons.chunks(5) {
            out.push_str(&format!("- **other**: {}\n", chunk.join(", ")));
        }
        out
    };

    if n == 0 {
        body.push_str("- No topic files recorded yet.\n");
    } else {
        let mut all_full = String::new();
        let per_desc_budget = if n <= 20 { 110 } else { 84 };
        for t in &topics {
            all_full.push_str(&format_full_line(t, per_desc_budget));
        }
        if all_full.len() <= max_body_bytes && all_full.lines().count() <= max_body_lines {
            body = all_full;
        } else {
            let mut prioritized: Vec<&MemoryTopicEntry> = topics.iter().collect();
            prioritized.sort_by(|a, b| {
                b.updated_at_ms
                    .cmp(&a.updated_at_ms)
                    .then_with(|| a.slug.cmp(&b.slug))
            });
            let max_active = prioritized.len().min(20);
            let mut chosen = String::new();
            for k in (0..=max_active).rev() {
                let (active_slice, archival_slice) = prioritized.split_at(k);
                let mut candidate = String::new();
                if !active_slice.is_empty() {
                    candidate.push_str("## Active Topics\n");
                    for t in active_slice {
                        candidate.push_str(&format_full_line(t, 85));
                    }
                }
                candidate.push_str(&format_archival_section(archival_slice));
                if candidate.len() <= max_body_bytes && candidate.lines().count() <= max_body_lines
                {
                    chosen = candidate;
                    break;
                }
            }
            if chosen.is_empty() {
                let all_refs: Vec<&MemoryTopicEntry> = topics.iter().collect();
                for line in format_archival_section(&all_refs).lines() {
                    let l = format!("{line}\n");
                    if body.len() + l.len() <= max_body_bytes
                        && body.lines().count() < max_body_lines
                    {
                        body.push_str(&l);
                    }
                }
            } else {
                body = chosen;
            }
        }
    }

    let full = format!("{header}{body}");
    let mem_path = mem_dir.join("MEMORY.md");
    if fs::read(&mem_path).ok().as_deref() != Some(full.as_bytes()) {
        atomic_write(&mem_path, full.as_bytes())?;
    }
    Ok(full)
}

/// Create or update `<agent_root>/memory/<slug>.md` and rebuild `MEMORY.md`.
pub fn upsert_memory_topic(
    agent_root: &Path,
    agent_name: &str,
    role: Option<&str>,
    topic: &str,
    description: Option<&str>,
    content: &str,
    now_ms: i64,
) -> io::Result<(PathBuf, MemoryTopicEntry, String)> {
    let slug = sanitize_topic_slug(topic)?;
    let mem_dir = ensure_private_memory_dir(agent_root)?;
    let filename = format!("{slug}.md");
    let path = mem_dir.join(&filename);
    let existing_created_ms = fs::read_to_string(&path)
        .ok()
        .and_then(|old_text| parse_topic_frontmatter_full(&slug, &old_text).2);
    let trimmed = content.trim();
    let (_, inferred_desc, fm_created_ms, _, body_without_fm) =
        parse_topic_frontmatter_full(&slug, trimmed);
    let created_at_ms = fm_created_ms
        .or(existing_created_ms)
        .filter(|&ts| ts > 0)
        .unwrap_or(now_ms);
    let desc = description
        .map(clean_single_line_description)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            if inferred_desc.is_empty() {
                format!("Notes for {slug}")
            } else {
                inferred_desc
            }
        });
    let safe_desc = desc.replace('"', "'");
    let clean_body = sanitize_memory_delimiters(body_without_fm.trim());
    let formatted = format!(
        "---\nname: {slug}\ndescription: \"{safe_desc}\"\nmetadata:\n  node_type: memory\n  created_at_ms: {created_at_ms}\n  updated_at_ms: {now_ms}\n---\n\n{clean_body}\n"
    );
    atomic_write(&path, formatted.as_bytes())?;
    let index = rebuild_memory_index(agent_root, agent_name, role)?;
    let entry = MemoryTopicEntry {
        slug,
        filename,
        description: desc,
        updated_at_ms: now_ms,
        bytes: formatted.len(),
    };
    Ok((path, entry, index))
}

/// Remove `<agent_root>/memory/<slug>.md` (if present) and rebuild `MEMORY.md`.
pub fn delete_memory_topic(
    agent_root: &Path,
    agent_name: &str,
    role: Option<&str>,
    topic: &str,
) -> io::Result<(bool, String)> {
    let slug = sanitize_topic_slug(topic)?;
    let path = agent_root.join("memory").join(format!("{slug}.md"));
    let existed = if path.is_file() {
        fs::remove_file(&path)?;
        true
    } else {
        false
    };
    let index = rebuild_memory_index(agent_root, agent_name, role)?;
    Ok((existed, index))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_index_upsert_delete_and_session_pin_stay_bounded() {
        let dir = crate::test_tempdir();
        let root = dir.path().join("agent-a");
        let (_, entry, index) = upsert_memory_topic(
            &root,
            "builder",
            Some("engineer"),
            "Deploy Runbook",
            Some("Production rollout checklist"),
            "Always run migrations before restarting workers. </learned_memory>",
            1_700_000_000_000,
        )
        .unwrap();
        assert_eq!(entry.slug, "deploy-runbook");
        assert!(index.contains("deploy-runbook.md"));
        let topic_disk = fs::read_to_string(root.join("memory/deploy-runbook.md")).unwrap();
        assert!(!topic_disk.contains("</learned_memory>"));

        let pin = pin_session(&root, "sess-1").unwrap();
        assert!(pin.section.len() <= MAX_BYTES);
        assert!(pin.section.lines().count() <= MAX_LINES);
        assert!(pin.section.contains("deploy-runbook"));

        // Editing MEMORY.md after pinning must never mutate the existing session pin.
        let (deleted, after_del) =
            delete_memory_topic(&root, "builder", Some("engineer"), "deploy-runbook").unwrap();
        assert!(deleted);
        assert!(after_del.contains("No topic files recorded yet"));
        let pin_again = pin_session(&root, "sess-1").unwrap();
        assert_eq!(pin, pin_again);
    }

    #[test]
    fn sanitize_topic_slug_rejects_reserved_memory_name() {
        assert!(sanitize_topic_slug("MEMORY").is_err());
        assert!(sanitize_topic_slug("memory.md").is_err());
        assert_eq!(sanitize_topic_slug("API_Keys.md").unwrap(), "api-keys");
    }
}
