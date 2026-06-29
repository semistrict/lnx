use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};

pub const WHITEOUT_MARKER: &str = ".lnx-whiteout";

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct HostShareTarget {
    pub tag: &'static str,
    pub share_root: PathBuf,
    pub suffix: PathBuf,
    pub absolute: PathBuf,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct PathState {
    pub target: HostShareTarget,
    pub upper_path: PathBuf,
    pub whiteout_path: PathBuf,
    pub upper_exists: bool,
    pub direct_whiteout: bool,
    pub covering_whiteout: Option<PathBuf>,
    pub descendant_state: bool,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct StateEntry {
    pub tag: &'static str,
    pub kind: StateEntryKind,
    pub logical_path: PathBuf,
    pub state_path: PathBuf,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum StateEntryKind {
    Copied,
    Hidden,
    DescendantState,
}

impl StateEntryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Copied => "copied",
            Self::Hidden => "hidden",
            Self::DescendantState => "descendant-state",
        }
    }
}

pub fn state_root(instance_dir: &Path) -> PathBuf {
    instance_dir.join("host-share-state")
}

pub fn targets_for_path(path: &Path, cwd: &Path) -> Result<Vec<HostShareTarget>> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let home = dirs::home_dir();
    Ok(targets_for_absolute_path(&absolute, cwd, home.as_deref()))
}

pub fn targets_for_absolute_path(
    absolute: &Path,
    cwd: &Path,
    home: Option<&Path>,
) -> Vec<HostShareTarget> {
    let mut targets = Vec::new();
    if let Some(home) = home
        && let Ok(suffix) = absolute.strip_prefix(home)
    {
        targets.push(HostShareTarget {
            tag: "home",
            share_root: home.to_path_buf(),
            suffix: suffix.to_path_buf(),
            absolute: absolute.to_path_buf(),
        });
    }
    let cwd_is_under_home = home.is_some_and(|home| cwd.starts_with(home));
    if !cwd_is_under_home && let Ok(suffix) = absolute.strip_prefix(cwd) {
        targets.push(HostShareTarget {
            tag: "cwd",
            share_root: cwd.to_path_buf(),
            suffix: suffix.to_path_buf(),
            absolute: absolute.to_path_buf(),
        });
    }
    targets
}

pub fn path_state(root: &Path, target: &HostShareTarget) -> Result<PathState> {
    let upper_path = root.join(target.tag).join("upper").join(&target.suffix);
    let whiteout_path = root.join(target.tag).join("whiteouts").join(&target.suffix);
    let direct_whiteout = direct_whiteout_exists(&whiteout_path)?;
    let upper_metadata = path_metadata(&upper_path)?;
    let lower_metadata = path_metadata(&target.absolute)?;
    let upper_namespace = upper_metadata
        .as_ref()
        .is_some_and(|metadata| metadata.file_type().is_dir())
        && lower_metadata
            .as_ref()
            .is_some_and(|metadata| metadata.file_type().is_dir());
    let upper_exists = upper_metadata.is_some() && !upper_namespace;
    let whiteout_namespace = path_metadata(&whiteout_path)?
        .as_ref()
        .is_some_and(|metadata| metadata.file_type().is_dir())
        && !direct_whiteout;
    let covering_whiteout = covering_whiteout(root, target)?;
    let descendant_state =
        upper_namespace || whiteout_namespace || has_descendant_state(&upper_path)?;
    Ok(PathState {
        target: target.clone(),
        upper_path,
        whiteout_path,
        upper_exists,
        direct_whiteout,
        covering_whiteout,
        descendant_state,
    })
}

pub fn list_state_entries(root: &Path, cwd: &Path) -> Result<Vec<StateEntry>> {
    let home = dirs::home_dir();
    list_state_entries_with_home(root, cwd, home.as_deref())
}

fn list_state_entries_with_home(
    root: &Path,
    cwd: &Path,
    home: Option<&Path>,
) -> Result<Vec<StateEntry>> {
    let mut entries = Vec::new();
    let shares = [
        home.map(|home| ("home", home.to_path_buf())),
        Some(("cwd", cwd.to_path_buf()))
            .filter(|(_, cwd)| !home.is_some_and(|home| cwd.starts_with(home))),
    ];
    for share in shares.into_iter().flatten() {
        let (tag, share_root) = share;
        collect_upper_entries(
            &root.join(tag).join("upper"),
            tag,
            &share_root,
            &mut entries,
        )?;
        collect_whiteout_entries(
            &root.join(tag).join("whiteouts"),
            tag,
            &share_root,
            &mut entries,
        )?;
    }
    entries.sort_by(|a, b| {
        a.logical_path
            .cmp(&b.logical_path)
            .then(a.kind.as_str().cmp(b.kind.as_str()))
            .then(a.tag.cmp(b.tag))
    });
    Ok(entries)
}

fn collect_upper_entries(
    root: &Path,
    tag: &'static str,
    share_root: &Path,
    entries: &mut Vec<StateEntry>,
) -> Result<()> {
    let mut paths = BTreeMap::new();
    collect_paths(root, root, &mut paths)?;
    for (suffix, state_path) in paths {
        let lower_path = share_root.join(&suffix);
        let kind = if path_metadata(&state_path)?
            .as_ref()
            .is_some_and(|metadata| metadata.file_type().is_dir())
            && path_metadata(&lower_path)?
                .as_ref()
                .is_some_and(|metadata| metadata.file_type().is_dir())
        {
            StateEntryKind::DescendantState
        } else {
            StateEntryKind::Copied
        };
        entries.push(StateEntry {
            tag,
            kind,
            logical_path: lower_path,
            state_path,
        });
    }
    Ok(())
}

fn collect_whiteout_entries(
    root: &Path,
    tag: &'static str,
    share_root: &Path,
    entries: &mut Vec<StateEntry>,
) -> Result<()> {
    let mut paths = BTreeMap::new();
    collect_paths(root, root, &mut paths)?;
    for (suffix, state_path) in paths {
        if suffix
            .file_name()
            .is_some_and(|name| name == WHITEOUT_MARKER)
        {
            continue;
        }
        let kind = if direct_whiteout_exists(&state_path)? {
            StateEntryKind::Hidden
        } else {
            StateEntryKind::DescendantState
        };
        entries.push(StateEntry {
            tag,
            kind,
            logical_path: share_root.join(&suffix),
            state_path,
        });
    }
    Ok(())
}

fn collect_paths(
    root: &Path,
    current: &Path,
    paths: &mut BTreeMap<PathBuf, PathBuf>,
) -> Result<()> {
    let read_dir = match fs::read_dir(current) {
        Ok(read_dir) => read_dir,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("read {}", current.display())),
    };
    for entry in read_dir {
        let entry = entry.with_context(|| format!("read {}", current.display()))?;
        let path = entry.path();
        let suffix = path
            .strip_prefix(root)
            .with_context(|| format!("strip {}", root.display()))?
            .to_path_buf();
        paths.insert(suffix, path.clone());
        if entry
            .file_type()
            .with_context(|| format!("stat {}", path.display()))?
            .is_dir()
        {
            collect_paths(root, &path, paths)?;
        }
    }
    Ok(())
}

fn covering_whiteout(root: &Path, target: &HostShareTarget) -> Result<Option<PathBuf>> {
    for ancestor in target.suffix.ancestors() {
        if ancestor.as_os_str().is_empty() {
            continue;
        }
        let whiteout = root.join(target.tag).join("whiteouts").join(ancestor);
        if direct_whiteout_exists(&whiteout)? {
            return Ok(Some(ancestor.to_path_buf()));
        }
    }
    Ok(None)
}

fn direct_whiteout_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => Ok(has_path(&path.join(WHITEOUT_MARKER))?),
        Ok(_) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("stat {}", path.display())),
    }
}

fn has_descendant_state(path: &Path) -> Result<bool> {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return Ok(false);
    };
    if !metadata.file_type().is_dir() {
        return Ok(false);
    }
    let mut read_dir = fs::read_dir(path).with_context(|| format!("read {}", path.display()))?;
    Ok(read_dir.next().transpose()?.is_some())
}

fn path_metadata(path: &Path) -> Result<Option<fs::Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("stat {}", path.display())),
    }
}

fn has_path(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("stat {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descendant_whiteout_directory_is_not_hidden() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("state");
        let share = temp.path().join("home");
        let project = share.join("src/project");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(root.join("home/whiteouts/src/project")).unwrap();
        let target = HostShareTarget {
            tag: "home",
            share_root: share.clone(),
            suffix: PathBuf::from("src/project"),
            absolute: project.clone(),
        };

        let state = path_state(&root, &target).unwrap();

        assert!(!state.direct_whiteout);
        assert_eq!(state.covering_whiteout, None);
        assert!(!state.upper_exists);
        assert!(state.descendant_state);
    }

    #[test]
    fn marker_whiteout_hides_descendants() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("state");
        let share = temp.path().join("home");
        let project = share.join("src/project");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(root.join("home/whiteouts/src")).unwrap();
        fs::write(
            root.join("home/whiteouts/src").join(WHITEOUT_MARKER),
            b"whiteout\n",
        )
        .unwrap();
        let target = HostShareTarget {
            tag: "home",
            share_root: share,
            suffix: PathBuf::from("src/project"),
            absolute: project,
        };

        let state = path_state(&root, &target).unwrap();

        assert_eq!(state.covering_whiteout, Some(PathBuf::from("src")));
    }

    #[test]
    fn list_state_entries_reports_logical_paths() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("state");
        let home = temp.path().join("home");
        let rel = PathBuf::from(".lnx-test-output");
        fs::create_dir_all(root.join("home/upper").join(&rel)).unwrap();
        fs::create_dir_all(home.join(".lnx-test-namespace")).unwrap();
        fs::create_dir_all(root.join("home/upper/.lnx-test-namespace")).unwrap();
        fs::create_dir_all(root.join("home/whiteouts")).unwrap();
        fs::write(root.join("home/whiteouts/.hidden"), b"whiteout\n").unwrap();

        let entries = list_state_entries_with_home(&root, Path::new("/tmp"), Some(&home)).unwrap();

        assert!(entries.iter().any(|entry| {
            entry.kind == StateEntryKind::Copied && entry.logical_path == home.join(&rel)
        }));
        assert!(entries.iter().any(|entry| {
            entry.kind == StateEntryKind::DescendantState
                && entry.logical_path == home.join(".lnx-test-namespace")
        }));
        assert!(entries.iter().any(|entry| {
            entry.kind == StateEntryKind::Hidden && entry.logical_path == home.join(".hidden")
        }));
    }
}
