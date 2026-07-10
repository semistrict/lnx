use std::{
    collections::BTreeMap,
    ffi::{CStr, CString},
    fs, io,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        unix::ffi::OsStrExt,
    },
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result, bail};

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
    let home = dirs::home_dir();
    targets_for_path_with_home(path, cwd, home.as_deref())
}

pub fn targets_for_absolute_path(
    absolute: &Path,
    cwd: &Path,
    home: Option<&Path>,
) -> Vec<HostShareTarget> {
    let absolute = lexical_normalize(absolute);
    let cwd = lexical_normalize(cwd);
    if !absolute.is_absolute() || !cwd.is_absolute() {
        return Vec::new();
    }
    let home = home
        .map(lexical_normalize)
        .filter(|home| home.is_absolute());
    targets_for_normalized_absolute_path(&absolute, &cwd, home.as_deref())
}

fn targets_for_path_with_home(
    path: &Path,
    cwd: &Path,
    home: Option<&Path>,
) -> Result<Vec<HostShareTarget>> {
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        bail!(
            "host-share state paths must not contain '..': {}",
            path.display()
        );
    }
    let cwd = lexical_normalize(cwd);
    if !cwd.is_absolute() {
        bail!(
            "host-share working directory is not absolute: {}",
            cwd.display()
        );
    }
    let absolute = if path.is_absolute() {
        lexical_normalize(path)
    } else {
        lexical_normalize(&cwd.join(path))
    };
    let home = home
        .map(lexical_normalize)
        .filter(|home| home.is_absolute());
    Ok(targets_for_normalized_absolute_path(
        &absolute,
        &cwd,
        home.as_deref(),
    ))
}

fn targets_for_normalized_absolute_path(
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

fn lexical_normalize(path: &Path) -> PathBuf {
    let absolute = path.is_absolute();
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(
                    normalized.components().next_back(),
                    Some(Component::Normal(_))
                ) {
                    normalized.pop();
                } else if !absolute {
                    normalized.push(component.as_os_str());
                }
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    normalized
}

fn checked_state_path(root: &Path, target: &HostShareTarget, namespace: &str) -> Result<PathBuf> {
    if !matches!(target.tag, "home" | "cwd") {
        bail!("unsafe host-share tag: {}", target.tag);
    }
    if target.suffix.is_absolute()
        || target.suffix.components().any(|component| {
            matches!(
                component,
                Component::Prefix(_) | Component::RootDir | Component::ParentDir
            )
        })
    {
        bail!(
            "unsafe host-share state path: {}",
            target.absolute.display()
        );
    }

    let namespace_root = lexical_normalize(&root.join(target.tag).join(namespace));
    let state_path = lexical_normalize(&namespace_root.join(&target.suffix));
    if !state_path.starts_with(&namespace_root) {
        bail!(
            "host-share state path escapes {}: {}",
            namespace_root.display(),
            state_path.display()
        );
    }
    Ok(state_path)
}

pub fn remove_path_state(root: &Path, target: &HostShareTarget) -> Result<()> {
    if target.suffix.as_os_str().is_empty() {
        bail!("refusing to remove all host-share copy-on-write state");
    }
    remove_state_path_if_exists(root, target, "upper")?;
    remove_state_path_if_exists(root, target, "whiteouts")
}

fn remove_state_path_if_exists(
    root: &Path,
    target: &HostShareTarget,
    namespace: &str,
) -> Result<()> {
    let path = checked_state_path(root, target, namespace)?;
    remove_entry_beneath(root, target.tag, namespace, &target.suffix)
        .with_context(|| format!("remove {}", path.display()))
}

/// Removes one entry using directory-relative operations with no symlink
/// following. Every parent component is opened beneath the trusted state root,
/// and recursive deletion remains anchored to those open directory handles.
fn remove_entry_beneath(root: &Path, tag: &str, namespace: &str, suffix: &Path) -> Result<()> {
    let Some(mut parent) = open_root_dir_no_follow(root)? else {
        return Ok(());
    };
    for component in [tag.as_bytes(), namespace.as_bytes()] {
        let name = CString::new(component).context("host-share state component contains NUL")?;
        let Some(next) = open_child_dir_no_follow(parent.as_raw_fd(), &name)? else {
            return Ok(());
        };
        parent = next;
    }

    let components = suffix
        .components()
        .map(|component| match component {
            Component::Normal(name) => CString::new(name.as_bytes())
                .context("host-share state path component contains NUL"),
            _ => bail!("unsafe host-share state suffix: {}", suffix.display()),
        })
        .collect::<Result<Vec<_>>>()?;
    let Some((final_name, parent_components)) = components.split_last() else {
        bail!("refusing to remove an empty host-share state suffix");
    };
    for component in parent_components {
        let Some(next) = open_child_dir_no_follow(parent.as_raw_fd(), component)? else {
            return Ok(());
        };
        parent = next;
    }
    remove_entry_at(parent.as_raw_fd(), final_name)
}

fn open_root_dir_no_follow(path: &Path) -> Result<Option<OwnedFd>> {
    let path = CString::new(path.as_os_str().as_bytes()).context("state root contains NUL")?;
    let fd = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd >= 0 {
        return Ok(Some(unsafe { OwnedFd::from_raw_fd(fd) }));
    }
    let error = io::Error::last_os_error();
    if error.kind() == io::ErrorKind::NotFound {
        return Ok(None);
    }
    Err(error).context("open host-share state root without following symlinks")
}

fn open_child_dir_no_follow(parent: RawFd, name: &CStr) -> Result<Option<OwnedFd>> {
    let fd = unsafe {
        libc::openat(
            parent,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd >= 0 {
        return Ok(Some(unsafe { OwnedFd::from_raw_fd(fd) }));
    }
    let error = io::Error::last_os_error();
    if error.kind() == io::ErrorKind::NotFound {
        return Ok(None);
    }
    Err(error).with_context(|| {
        format!(
            "open host-share state directory {:?} without following symlinks",
            name
        )
    })
}

fn remove_entry_at(parent: RawFd, name: &CStr) -> Result<()> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    let stat_result = unsafe {
        libc::fstatat(
            parent,
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if stat_result != 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::NotFound {
            return Ok(());
        }
        return Err(error).context("stat host-share state entry without following symlinks");
    }
    let stat = unsafe { stat.assume_init() };
    if stat.st_mode & libc::S_IFMT == libc::S_IFDIR {
        let directory = open_child_dir_no_follow(parent, name)?
            .context("host-share state directory disappeared during removal")?;
        remove_directory_contents(directory.as_raw_fd())?;
        let result = unsafe { libc::unlinkat(parent, name.as_ptr(), libc::AT_REMOVEDIR) };
        if result != 0 {
            return Err(io::Error::last_os_error()).context("remove host-share state directory");
        }
        return Ok(());
    }
    let result = unsafe { libc::unlinkat(parent, name.as_ptr(), 0) };
    if result != 0 {
        return Err(io::Error::last_os_error()).context("remove host-share state entry");
    }
    Ok(())
}

fn remove_directory_contents(directory: RawFd) -> Result<()> {
    let duplicate = unsafe { libc::fcntl(directory, libc::F_DUPFD_CLOEXEC, 0) };
    if duplicate < 0 {
        return Err(io::Error::last_os_error())
            .context("duplicate host-share directory handle with close-on-exec");
    }
    let stream = unsafe { libc::fdopendir(duplicate) };
    if stream.is_null() {
        let error = io::Error::last_os_error();
        unsafe { libc::close(duplicate) };
        return Err(error).context("open host-share directory stream");
    }
    struct DirectoryStream(*mut libc::DIR);
    impl Drop for DirectoryStream {
        fn drop(&mut self) {
            unsafe { libc::closedir(self.0) };
        }
    }
    let stream = DirectoryStream(stream);
    loop {
        clear_readdir_errno();
        let entry = unsafe { libc::readdir(stream.0) };
        if entry.is_null() {
            if let Some(errno) = readdir_errno()
                && errno != 0
            {
                return Err(io::Error::from_raw_os_error(errno))
                    .context("read host-share directory during removal");
            }
            break;
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        if name.to_bytes() == b"." || name.to_bytes() == b".." {
            continue;
        }
        let name = CString::new(name.to_bytes()).context("directory entry contains NUL")?;
        remove_entry_at(directory, &name)?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn clear_readdir_errno() {
    unsafe { *libc::__error() = 0 };
}

#[cfg(target_os = "macos")]
fn readdir_errno() -> Option<libc::c_int> {
    Some(unsafe { *libc::__error() })
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn clear_readdir_errno() {
    unsafe { *libc::__errno_location() = 0 };
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn readdir_errno() -> Option<libc::c_int> {
    Some(unsafe { *libc::__errno_location() })
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "android")))]
fn clear_readdir_errno() {}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "android")))]
fn readdir_errno() -> Option<libc::c_int> {
    None
}

pub fn path_state(root: &Path, target: &HostShareTarget) -> Result<PathState> {
    let upper_path = checked_state_path(root, target, "upper")?;
    let whiteout_path = checked_state_path(root, target, "whiteouts")?;
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
    fn relative_parent_components_are_rejected_as_ambiguous() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let cwd = home.join("projects/app");

        let error = targets_for_path_with_home(Path::new("../sibling"), &cwd, Some(&home))
            .expect_err("reject parent component");

        assert!(error.to_string().contains("must not contain '..'"));
    }

    #[test]
    fn absolute_parent_components_are_rejected_as_ambiguous() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let cwd = home.join("projects/app");

        let error =
            targets_for_path_with_home(&home.join("projects/app/../sibling"), &cwd, Some(&home))
                .expect_err("reject parent component");

        assert!(error.to_string().contains("must not contain '..'"));
    }

    #[test]
    fn parent_components_that_leave_a_share_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let cwd = home.join("projects/app");

        let error = targets_for_path_with_home(Path::new("../../../outside"), &cwd, Some(&home))
            .expect_err("reject parent component");

        assert!(error.to_string().contains("must not contain '..'"));
    }

    #[test]
    fn removal_rejects_parent_suffix_without_touching_outside_state() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("state");
        let sentinel = temp.path().join("victim");
        fs::write(&sentinel, b"keep").unwrap();
        let target = HostShareTarget {
            tag: "home",
            share_root: temp.path().join("home"),
            suffix: PathBuf::from("../../../victim"),
            absolute: temp.path().join("home/victim"),
        };

        let error = remove_path_state(&root, &target).unwrap_err();

        assert!(error.to_string().contains("unsafe host-share state path"));
        assert_eq!(fs::read(&sentinel).unwrap(), b"keep");
    }

    #[test]
    fn removal_rejects_symlinked_parent_without_touching_outside_state() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("state");
        let outside = temp.path().join("outside");
        let victim = outside.join("victim");
        fs::create_dir_all(&victim).unwrap();
        fs::write(victim.join("sentinel"), b"keep").unwrap();
        fs::create_dir_all(root.join("home/upper")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("home/upper/redirect")).unwrap();
        let target = HostShareTarget {
            tag: "home",
            share_root: temp.path().join("home"),
            suffix: PathBuf::from("redirect/victim"),
            absolute: temp.path().join("home/redirect/victim"),
        };

        let error = remove_path_state(&root, &target).unwrap_err();

        assert!(format!("{error:#}").contains("without following symlinks"));
        assert_eq!(fs::read(victim.join("sentinel")).unwrap(), b"keep");
    }

    #[test]
    fn removal_clears_only_the_requested_state() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("state");
        let upper = root.join("home/upper/project");
        let whiteout = root.join("home/whiteouts/project");
        fs::create_dir_all(&upper).unwrap();
        fs::write(upper.join("copied"), b"data").unwrap();
        fs::create_dir_all(&whiteout).unwrap();
        fs::write(whiteout.join(WHITEOUT_MARKER), b"whiteout\n").unwrap();
        fs::create_dir_all(root.join("home/upper/keep")).unwrap();
        fs::create_dir_all(root.join("home/whiteouts/keep")).unwrap();
        let target = HostShareTarget {
            tag: "home",
            share_root: temp.path().join("home"),
            suffix: PathBuf::from("project"),
            absolute: temp.path().join("home/project"),
        };

        remove_path_state(&root, &target).unwrap();

        assert!(!upper.exists());
        assert!(!whiteout.exists());
        assert!(root.join("home/upper/keep").exists());
        assert!(root.join("home/whiteouts/keep").exists());
    }

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
