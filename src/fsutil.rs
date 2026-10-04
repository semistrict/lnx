//! Small filesystem helpers shared across commands.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

/// Removes a file, symlink or directory tree at `path`; a missing path is
/// not an error. Symlinks are removed, never followed.
pub(crate) fn remove_path_if_exists(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => {
            fs::remove_dir_all(path).with_context(|| format!("remove {}", path.display()))
        }
        Ok(_) => fs::remove_file(path).with_context(|| format!("remove {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("stat {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_files_directories_and_symlinks_without_following() {
        let temp = tempfile::tempdir().expect("tempdir");
        let target = temp.path().join("target");
        fs::create_dir(&target).expect("create target dir");
        fs::write(target.join("kept"), b"kept").expect("write kept file");
        let link = temp.path().join("link");
        std::os::unix::fs::symlink(&target, &link).expect("create symlink");
        let file = temp.path().join("file");
        fs::write(&file, b"file").expect("write file");
        let tree = temp.path().join("tree");
        fs::create_dir_all(tree.join("nested")).expect("create tree");

        for path in [&link, &file, &tree, &temp.path().join("missing")] {
            remove_path_if_exists(path).expect("remove path");
            assert!(fs::symlink_metadata(path).is_err());
        }
        assert!(
            target.join("kept").exists(),
            "symlink target is not followed"
        );
    }
}
