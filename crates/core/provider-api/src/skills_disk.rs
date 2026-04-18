//! Disk scanner for user-authored `SKILL.md` files.
//!
//! A "skill" on disk is a directory named after the skill, containing a
//! `SKILL.md` file whose head is a YAML-like frontmatter block:
//!
//! ```text
//! ---
//! name: my-skill
//! description: What this skill does, in one line.
//! argument-hint: "[optional]"
//! ---
//!
//! Body goes here...
//! ```
//!
//! The scanner walks a list of roots, discovers every `SKILL.md`, parses
//! its frontmatter, and emits a [`ProviderCommand`] with
//! `kind: CommandKind::UserSkill { source }`.
//!
//! Parsing is intentionally permissive: a malformed frontmatter just
//! skips that file and logs a `tracing::debug!` line. We only reject
//! entries that lack a usable `name`. Project-local skills win over
//! global skills with the same name (the scanner collapses duplicates
//! after all roots are walked).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::{CommandKind, ProviderCommand, ProviderKind, SkillSource};

/// A single directory to scan, plus the `SkillSource` each discovered
/// skill should carry.
#[derive(Debug, Clone)]
pub struct ScanRoot {
    pub path: PathBuf,
    pub source: SkillSource,
}

/// Compose the canonical list of scan roots for a provider.
///
/// - `home_dirs` are relative entries (e.g. `".claude"`) that map to
///   `~/<entry>/skills` on disk.
/// - `project_dirs` are relative entries (e.g. `".claude/skills"`) that
///   map to `<cwd>/<entry>` on disk.
///
/// Home-rooted paths always get `SkillSource::DiskGlobal`; cwd-rooted
/// paths get `SkillSource::DiskProject`. Missing directories are
/// filtered out at scan time, not here.
pub fn scan_roots_for(
    home_dirs: &[&str],
    project_dirs: &[&str],
    cwd: Option<&Path>,
) -> Vec<ScanRoot> {
    let mut roots = Vec::new();

    if let Some(home) = home_dir() {
        for entry in home_dirs {
            roots.push(ScanRoot {
                path: home.join(entry).join("skills"),
                source: SkillSource::DiskGlobal,
            });
        }
    }

    if let Some(cwd) = cwd {
        for entry in project_dirs {
            roots.push(ScanRoot {
                path: cwd.join(entry),
                source: SkillSource::DiskProject,
            });
        }
    }

    roots
}

/// Discover every `SKILL.md` under `roots`, parse its frontmatter, and
/// emit one [`ProviderCommand`] per valid skill. Project-local entries
/// override global entries with the same name (so a user's
/// project-scoped skill shadows a global skill of the same name).
pub fn scan(roots: &[ScanRoot], provider: ProviderKind) -> Vec<ProviderCommand> {
    // Pass 1: collect every candidate from every root, preserving order
    // so the dedupe step can rely on "project wins over global".
    let mut candidates: Vec<(ScanRoot, ParsedSkill)> = Vec::new();
    for root in roots {
        if !root.path.is_dir() {
            tracing::debug!(path = %root.path.display(), "skills_disk: root missing, skipping");
            continue;
        }
        let entries = match std::fs::read_dir(&root.path) {
            Ok(it) => it,
            Err(err) => {
                tracing::debug!(
                    path = %root.path.display(),
                    error = %err,
                    "skills_disk: read_dir failed"
                );
                continue;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let skill_md = path.join("SKILL.md");
            if !skill_md.is_file() {
                continue;
            }
            let raw = match std::fs::read_to_string(&skill_md) {
                Ok(s) => s,
                Err(err) => {
                    tracing::debug!(
                        path = %skill_md.display(),
                        error = %err,
                        "skills_disk: read_to_string failed"
                    );
                    continue;
                }
            };
            let parsed = match parse_frontmatter(&raw) {
                Some(p) => p,
                None => {
                    tracing::debug!(
                        path = %skill_md.display(),
                        "skills_disk: no usable frontmatter"
                    );
                    continue;
                }
            };
            candidates.push((root.clone(), parsed));
        }
    }

    // Pass 2: project entries override global entries with the same
    // name. Emit project entries first, then fill in globals whose name
    // hasn't been seen yet.
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<ProviderCommand> = Vec::new();

    for source in [SkillSource::DiskProject, SkillSource::DiskGlobal] {
        for (root, parsed) in &candidates {
            if root.source != source {
                continue;
            }
            if !seen.insert(parsed.name.clone()) {
                continue;
            }
            out.push(ProviderCommand {
                id: format!("{}:user_skill:{}", provider.as_tag(), parsed.name),
                name: parsed.name.clone(),
                description: parsed.description.clone(),
                kind: CommandKind::UserSkill { source },
                user_invocable: true,
                arg_hint: parsed.arg_hint.clone(),
            });
        }
    }

    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

#[derive(Debug, Clone)]
struct ParsedSkill {
    name: String,
    description: String,
    arg_hint: Option<String>,
}

/// Strip an optional UTF-8 BOM, accept `\n` and `\r\n` line endings,
/// find the first `---` fence and the next one, and harvest
/// `key: value` lines between. Returns `None` if the frontmatter is
/// missing or doesn't contain a usable `name`.
fn parse_frontmatter(raw: &str) -> Option<ParsedSkill> {
    let stripped = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    let mut lines = stripped.lines();

    // First non-empty line must be "---".
    let first = loop {
        match lines.next() {
            Some(l) if l.trim().is_empty() => continue,
            Some(l) => break l,
            None => return None,
        }
    };
    if first.trim() != "---" {
        return None;
    }

    let mut name: Option<String> = None;
    let mut description = String::new();
    let mut arg_hint: Option<String> = None;

    for line in lines {
        if line.trim() == "---" {
            break;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = unquote(value.trim());
        match key.as_str() {
            "name" => {
                if !value.is_empty() {
                    name = Some(value);
                }
            }
            "description" => {
                description = value;
            }
            // Accept both "argument-hint" (Claude skills) and "arg-hint"
            // as aliases so providers that pick either spelling work.
            "argument-hint" | "arg-hint" | "arguments" => {
                if !value.is_empty() {
                    arg_hint = Some(value);
                }
            }
            _ => {}
        }
    }

    Some(ParsedSkill {
        name: name?,
        description,
        arg_hint,
    })
}

fn unquote(s: &str) -> String {
    let trimmed = s.trim();
    let bytes = trimmed.as_bytes();
    if bytes.len() >= 2
        && ((bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\''))
    {
        trimmed[1..trimmed.len() - 1].to_string()
    } else {
        trimmed.to_string()
    }
}

fn home_dir() -> Option<PathBuf> {
    // std::env::home_dir was stabilised back in 1.86 but its old
    // definition is deprecated with known-wrong behavior on Windows
    // (reads HOME, falls through to USERPROFILE). For the flowstate
    // targets (macOS + Linux + Windows via HOME-aware tooling) the
    // environment-variable fallback is exactly what we want — we only
    // care about `$HOME/.claude/skills` and friends, which the user
    // can override by exporting HOME.
    if let Some(home) = std::env::var_os("HOME") {
        return Some(PathBuf::from(home));
    }
    if let Some(home) = std::env::var_os("USERPROFILE") {
        return Some(PathBuf::from(home));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_skill(dir: &Path, name: &str, body: &str) {
        let skill_dir = dir.join(name);
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(skill_dir.join("SKILL.md"), body).unwrap();
    }

    #[test]
    fn parses_basic_frontmatter() {
        let body = "---\nname: my-skill\ndescription: Does things.\nargument-hint: \"[path]\"\n---\n\nBody\n";
        let parsed = parse_frontmatter(body).expect("parse");
        assert_eq!(parsed.name, "my-skill");
        assert_eq!(parsed.description, "Does things.");
        assert_eq!(parsed.arg_hint.as_deref(), Some("[path]"));
    }

    #[test]
    fn tolerates_bom_and_crlf() {
        let body = "\u{feff}---\r\nname: bom-skill\r\ndescription: ok\r\n---\r\n";
        let parsed = parse_frontmatter(body).expect("parse");
        assert_eq!(parsed.name, "bom-skill");
        assert_eq!(parsed.description, "ok");
    }

    #[test]
    fn skips_when_no_name() {
        let body = "---\ndescription: no name here\n---\n";
        assert!(parse_frontmatter(body).is_none());
    }

    #[test]
    fn skips_when_not_frontmatter() {
        let body = "# Just markdown\n\nNo fence.\n";
        assert!(parse_frontmatter(body).is_none());
    }

    #[test]
    fn project_overrides_global_for_same_name() {
        let tmp = tempfile::tempdir().unwrap();
        let global = tmp.path().join("global");
        let project = tmp.path().join("project");
        fs::create_dir_all(&global).unwrap();
        fs::create_dir_all(&project).unwrap();

        write_skill(
            &global,
            "shared",
            "---\nname: shared\ndescription: global copy\n---\n",
        );
        write_skill(
            &project,
            "shared",
            "---\nname: shared\ndescription: project copy\n---\n",
        );
        write_skill(
            &global,
            "only-global",
            "---\nname: only-global\ndescription: g\n---\n",
        );

        let roots = vec![
            ScanRoot {
                path: global.clone(),
                source: SkillSource::DiskGlobal,
            },
            ScanRoot {
                path: project.clone(),
                source: SkillSource::DiskProject,
            },
        ];
        let out = scan(&roots, ProviderKind::Claude);
        assert_eq!(out.len(), 2);

        let shared = out.iter().find(|c| c.name == "shared").unwrap();
        assert_eq!(shared.description, "project copy");
        assert!(matches!(
            shared.kind,
            CommandKind::UserSkill {
                source: SkillSource::DiskProject
            }
        ));

        let only = out.iter().find(|c| c.name == "only-global").unwrap();
        assert!(matches!(
            only.kind,
            CommandKind::UserSkill {
                source: SkillSource::DiskGlobal
            }
        ));
    }

    #[test]
    fn missing_dirs_are_silently_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = vec![ScanRoot {
            path: tmp.path().join("does-not-exist"),
            source: SkillSource::DiskProject,
        }];
        let out = scan(&roots, ProviderKind::Claude);
        assert!(out.is_empty());
    }

    #[test]
    fn non_skill_files_are_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        // Directory without a SKILL.md — ignored.
        fs::create_dir_all(root.join("no-skill-md")).unwrap();
        fs::write(root.join("no-skill-md").join("notes.md"), "hi").unwrap();
        // Loose file at root — ignored.
        fs::write(root.join("loose.txt"), "nothing").unwrap();
        write_skill(
            &root,
            "valid",
            "---\nname: valid\ndescription: ok\n---\n",
        );
        let roots = vec![ScanRoot {
            path: root,
            source: SkillSource::DiskProject,
        }];
        let out = scan(&roots, ProviderKind::Claude);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "valid");
    }
}
