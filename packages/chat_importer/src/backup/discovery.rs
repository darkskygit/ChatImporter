use anyhow::{Context, Result};
use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct BackupCandidate {
    pub source_id: String,
    pub display_path: PathBuf,
    pub kind: BackupCandidateKind,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum BackupCandidateKind {
    StandardRoot(PathBuf),
    ImazingVersionsDb { versions_db: PathBuf },
}

pub fn is_standard_backup_root(path: impl AsRef<Path>) -> bool {
    let path = path.as_ref();
    path.is_dir()
        && path.join("Status.plist").is_file()
        && path.join("Info.plist").is_file()
        && path.join("Manifest.plist").is_file()
}

pub fn discover_backup_candidates(paths: &[PathBuf]) -> Result<Vec<BackupCandidate>> {
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();

    for path in paths {
        discover_from_path(path, &mut seen, &mut candidates)
            .with_context(|| format!("failed to discover backups under {}", path.display()))?;
    }

    candidates.sort_by(|a, b| a.display_path.cmp(&b.display_path));
    Ok(candidates)
}

fn discover_from_path(
    input: &Path,
    seen: &mut HashSet<PathBuf>,
    candidates: &mut Vec<BackupCandidate>,
) -> Result<()> {
    let input = input.canonicalize()?;

    if let Some(root) =
        nearest_standard_backup_root(&input).filter(|root| !is_in_imazing_versions(root))
    {
        push_standard_candidate(root, seen, candidates)?;
    }

    if input.is_dir() {
        for entry in WalkDir::new(&input)
            .into_iter()
            .filter_map(|entry| entry.ok())
        {
            let path = entry.path();
            if is_standard_backup_root(path) && !is_in_imazing_versions(path) {
                push_standard_candidate(path.to_path_buf(), seen, candidates)?;
            }
            if is_imazing_versions_db(path) {
                push_imazing_candidate(path.to_path_buf(), seen, candidates)?;
            }
        }
    }

    Ok(())
}

fn is_in_imazing_versions(path: &Path) -> bool {
    path.components()
        .any(|component| component.as_os_str() == "iMazing.Versions")
}

fn is_imazing_versions_db(path: &Path) -> bool {
    if path.file_name().and_then(|name| name.to_str()) != Some("Versions.db") {
        return false;
    }

    let components = path
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .collect::<Vec<_>>();
    components.windows(4).any(|window| {
        window[0] == "iMazing.Versions"
            && window[1] == "Versions"
            && !window[2].is_empty()
            && window[3] == "Versions.db"
    })
}

fn nearest_standard_backup_root(path: &Path) -> Option<PathBuf> {
    let mut current = if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent()?.to_path_buf()
    };

    loop {
        if is_standard_backup_root(&current) {
            return Some(current);
        }
        if !current.pop() {
            return None;
        }
    }
}

fn push_standard_candidate(
    path: PathBuf,
    seen: &mut HashSet<PathBuf>,
    candidates: &mut Vec<BackupCandidate>,
) -> Result<()> {
    let canonical = path.canonicalize()?;
    if seen.insert(canonical.clone()) {
        candidates.push(BackupCandidate {
            source_id: source_id("standard", &canonical),
            display_path: canonical.clone(),
            kind: BackupCandidateKind::StandardRoot(canonical),
        });
    }
    Ok(())
}

fn push_imazing_candidate(
    path: PathBuf,
    seen: &mut HashSet<PathBuf>,
    candidates: &mut Vec<BackupCandidate>,
) -> Result<()> {
    let canonical = path.canonicalize()?;
    if seen.insert(canonical.clone()) {
        candidates.push(BackupCandidate {
            source_id: source_id("imazing", &canonical),
            display_path: canonical.clone(),
            kind: BackupCandidateKind::ImazingVersionsDb {
                versions_db: canonical,
            },
        });
    }
    Ok(())
}

fn source_id(kind: &str, path: &Path) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    kind.hash(&mut hasher);
    path.hash(&mut hasher);
    format!("{}:{:x}", kind, hasher.finish())
}
