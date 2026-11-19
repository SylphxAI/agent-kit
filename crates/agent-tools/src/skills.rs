//! Neutral skill discovery, `SKILL.md` frontmatter parsing, and non-destructive
//! skill view mounting across caller-configured target paths.

use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
};

/// Discovered skill descriptor parsed from `<skill_dir>/SKILL.md`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillEntry {
    pub slug: String,
    pub name: String,
    pub description: String,
    pub dir: PathBuf,
    pub skill_file: PathBuf,
}

/// Discover direct skills (`<root>/<skill>/SKILL.md`) and bounded synced layouts
/// (`<root>/synced/<bucket>/<skill>/SKILL.md`).
///
/// Later roots win within a class; direct skills always win over synced ones.
/// Buckets are sorted by name, with the first bucket winning a duplicate slug.
/// Hidden entries, plain files, and directories without `SKILL.md` are skipped.
pub fn discover(roots: &[PathBuf]) -> BTreeMap<String, PathBuf> {
    fn visible_dirs(root: &Path) -> Vec<(String, PathBuf)> {
        let mut dirs: Vec<_> = fs::read_dir(root)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name().into_string().ok()?;
                (!name.is_empty() && !name.starts_with('.') && entry.path().is_dir())
                    .then(|| (name, entry.path()))
            })
            .collect();
        dirs.sort_by(|a, b| a.0.cmp(&b.0));
        dirs
    }
    let mut synced = BTreeMap::new();
    let mut direct = BTreeMap::new();
    for root in roots {
        let mut in_root = BTreeMap::new();
        for (_, bucket) in visible_dirs(&root.join("synced")) {
            for (name, path) in visible_dirs(&bucket) {
                if path.join("SKILL.md").is_file() {
                    in_root.entry(name).or_insert(path);
                }
            }
        }
        synced.extend(in_root);
        for (name, path) in visible_dirs(root) {
            if path.join("SKILL.md").is_file() {
                direct.insert(name, path);
            }
        }
    }
    synced.extend(direct);
    synced
}

/// Parse `name` and `description` fields from YAML frontmatter at the top of `SKILL.md`.
pub fn parse_skill_frontmatter(slug: &str, markdown: &str) -> (String, String) {
    let mut name = slug.to_owned();
    let mut description = String::new();
    let trimmed = markdown.trim_start();
    if let Some(rest) = trimmed.strip_prefix("---\n")
        && let Some(end) = rest.find("\n---")
    {
        for line in rest[..end].lines() {
            let l = line.trim();
            if let Some(v) = l.strip_prefix("name:") {
                let v = v.trim().trim_matches('"').trim_matches('\'').trim();
                if !v.is_empty() {
                    name = v.to_owned();
                }
            } else if let Some(v) = l.strip_prefix("description:") {
                let v = v.trim().trim_matches('"').trim_matches('\'').trim();
                if !v.is_empty() {
                    description = v.to_owned();
                }
            }
        }
    }
    (name, description)
}

/// Discover skills across `roots` and parse each skill's `SKILL.md` frontmatter.
pub fn catalog(roots: &[PathBuf]) -> Vec<SkillEntry> {
    discover(roots)
        .into_iter()
        .map(|(slug, dir)| {
            let skill_file = dir.join("SKILL.md");
            let text = fs::read_to_string(&skill_file).unwrap_or_default();
            let (name, description) = parse_skill_frontmatter(&slug, &text);
            SkillEntry {
                slug,
                name,
                description,
                dir,
                skill_file,
            }
        })
        .collect()
}

#[cfg(unix)]
fn managed_source(path: &Path, roots: &[&Path]) -> bool {
    roots.iter().any(|root| {
        let Ok(relative) = path.strip_prefix(root) else {
            return false;
        };
        let parts: Vec<_> = relative.components().collect();
        parts
            .iter()
            .all(|part| matches!(part, std::path::Component::Normal(_)))
            && (parts.len() == 1 || (parts.len() == 3 && parts[0].as_os_str() == "synced"))
    })
}

/// Non-destructively project `global` + `<agent_root>/skills` overlay into
/// `<agent_root>/skills-view` and mirror symlinks to each caller-supplied relative
/// target under `config_root`. Existing non-managed directories and foreign symlinks
/// are preserved untouched.
#[cfg(unix)]
pub fn mount_mirrors(
    agent_root: &Path,
    global: &Path,
    config_root: &Path,
    mirror_rel_paths: &[&str],
) -> io::Result<()> {
    let overlay = agent_root.join("skills");
    let view = agent_root.join("skills-view");
    fs::create_dir_all(&overlay)?;
    fs::create_dir_all(&view)?;
    if fs::symlink_metadata(&overlay)?.file_type().is_symlink()
        || fs::symlink_metadata(&view)?.file_type().is_symlink()
    {
        return Err(io::Error::other(
            "skill overlay/view must be concrete directories, not symlinks",
        ));
    }
    let mut entries = BTreeMap::new();
    for (name, path) in discover(&[global.to_path_buf(), overlay.clone()]) {
        entries.insert(std::ffi::OsString::from(name), path);
    }
    sync_entries(&view, &entries, &[global, &overlay])?;

    for rel in mirror_rel_paths {
        let target = config_root.join(rel);
        let parent = target
            .parent()
            .ok_or_else(|| io::Error::other("skill mirror parent missing"))?;
        if parent
            .ancestors()
            .take_while(|p| *p != config_root)
            .any(|p| fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink()))
        {
            continue;
        }
        fs::create_dir_all(parent)?;
        match fs::symlink_metadata(&target) {
            Ok(meta) if meta.file_type().is_dir() => {
                sync_entries(&target, &entries, &[global, &overlay])?;
            }
            Ok(meta) if meta.file_type().is_symlink() => {
                let current = fs::read_link(&target)?;
                if current == global {
                    replace_link(&target, &view)?;
                }
            }
            Ok(_) => (),
            Err(e) if e.kind() == io::ErrorKind::NotFound => replace_link(&target, &view)?,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(unix)]
fn sync_entries(
    target: &Path,
    entries: &BTreeMap<std::ffi::OsString, PathBuf>,
    managed: &[&Path],
) -> io::Result<()> {
    for child in fs::read_dir(target)? {
        let child = child?;
        let path = child.path();
        if child.file_type()?.is_symlink() {
            let old = fs::read_link(&path)?;
            if managed_source(&old, managed) && !entries.contains_key(&child.file_name()) {
                fs::remove_file(path)?;
            }
        }
    }
    for (name, source) in entries {
        let path = target.join(name);
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let old = fs::read_link(&path)?;
                if old != *source && managed_source(&old, managed) {
                    replace_link(&path, source)?;
                }
            }
            Ok(_) => (),
            Err(e) if e.kind() == io::ErrorKind::NotFound => replace_link(&path, source)?,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(unix)]
fn replace_link(path: &Path, source: &Path) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("skill link parent missing"))?;
    let temp = parent.join(format!(".skills-{}", uuid::Uuid::now_v7()));
    std::os::unix::fs::symlink(source, &temp)?;
    if let Err(e) = fs::rename(&temp, path) {
        let _ = fs::remove_file(temp);
        return Err(e);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(root: &Path, slug: &str, body: &str) {
        fs::create_dir_all(root.join(slug)).unwrap();
        fs::write(root.join(slug).join("SKILL.md"), body).unwrap();
    }

    #[test]
    fn discover_and_catalog_respect_direct_over_synced_precedence() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global");
        let overlay = dir.path().join("overlay");
        write_skill(
            &global.join("synced/z"),
            "alpha",
            "---\nname: Alpha Z\ndescription: \"later bucket\"\n---\n",
        );
        write_skill(
            &global.join("synced/a"),
            "alpha",
            "---\nname: Alpha A\ndescription: \"first bucket\"\n---\n",
        );
        write_skill(
            &overlay,
            "alpha",
            "---\nname: Alpha Direct\ndescription: \"direct wins\"\n---\n",
        );
        write_skill(&global, ".hidden", "ignored");
        let entries = catalog(&[global, overlay]);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].slug, "alpha");
        assert_eq!(entries[0].name, "Alpha Direct");
        assert_eq!(entries[0].description, "direct wins");
    }

    #[cfg(unix)]
    #[test]
    fn mount_mirrors_projects_overlay_without_clobbering_local_or_foreign_mounts() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global");
        let agent = dir.path().join("agent");
        let config = agent.join("config");
        let personal = dir.path().join("personal");

        write_skill(&global, "shared", "shared skill");
        write_skill(&agent.join("skills"), "override", "agent skill");
        write_skill(&config.join("skills"), "local", "seat local");
        write_skill(&personal, "private", "personal skill");
        fs::create_dir_all(&config).unwrap();
        std::os::unix::fs::symlink(&personal, config.join("foreign")).unwrap();

        mount_mirrors(
            &agent,
            &global,
            &config,
            &["skills", ".agents/skills", "foreign/skills"],
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(config.join("skills/shared/SKILL.md")).unwrap(),
            "shared skill"
        );
        assert_eq!(
            fs::read_to_string(config.join("skills/local/SKILL.md")).unwrap(),
            "seat local"
        );
        assert_eq!(
            fs::read_to_string(config.join(".agents/skills/override/SKILL.md")).unwrap(),
            "agent skill"
        );
        assert!(!personal.join("skills").exists());
    }
}
