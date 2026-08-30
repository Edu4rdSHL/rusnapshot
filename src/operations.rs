use {
    crate::utils::{is_same_character, strip_trailing_slash},
    anyhow::{Context, Result, bail},
    std::{path::Path, process::Command},
};

/// Arguments for `btrfs subvolume create <dest>`.
#[must_use]
pub fn create_args(dest_dir: &str) -> Vec<String> {
    vec![
        "subvolume".into(),
        "create".into(),
        strip_trailing_slash(dest_dir).into(),
    ]
}

/// Arguments for `btrfs subvolume snapshot [-r] <source> <target>`.
///
/// The `-r` flag goes before the positional arguments so the command also works when
/// `POSIXLY_CORRECT` is set (getopt stops at the first non-option in that case).
#[must_use]
pub fn snapshot_args(source: &str, target: &str, read_only: bool) -> Vec<String> {
    let mut args = vec!["subvolume".to_string(), "snapshot".to_string()];
    if read_only {
        args.push("-r".into());
    }
    args.push(strip_trailing_slash(source).into());
    args.push(strip_trailing_slash(target).into());

    args
}

/// Arguments for `btrfs subvolume delete <path>`.
#[must_use]
pub fn delete_args(path: &str) -> Vec<String> {
    vec![
        "subvolume".into(),
        "delete".into(),
        strip_trailing_slash(path).into(),
    ]
}

/// Human readable form of a `btrfs` invocation, for messages and `--dry-run`.
#[must_use]
pub fn command_line(args: &[String]) -> String {
    format!("btrfs {}", args.join(" "))
}

/// Run `btrfs` with the given arguments, failing if it can't be executed or exits with an error.
///
/// # Errors
///
/// Fails if the `btrfs` binary can't be run or returns a non-zero status.
pub fn run_btrfs(args: &[String]) -> Result<()> {
    let status = Command::new("btrfs")
        .args(args)
        .status()
        .context("failed to execute 'btrfs', make sure btrfs-progs is installed and in PATH")?;
    if !status.success() {
        bail!("'{}' failed with {status}", command_line(args));
    }

    Ok(())
}

/// Create the snapshots directory as a subvolume if it doesn't exist yet.
///
/// # Errors
///
/// Fails if `btrfs subvolume create` fails.
pub fn setup_directory_structure(dest_dir: &str, dry_run: bool) -> Result<()> {
    if Path::new(dest_dir).exists() {
        return Ok(());
    }
    let args = create_args(dest_dir);
    if dry_run {
        println!("[dry-run] would run: {}", command_line(&args));
        return Ok(());
    }
    println!("Setting up the directory structure: {dest_dir}");

    run_btrfs(&args)
}

/// Take a snapshot of `source` at `target`.
///
/// # Errors
///
/// Fails if `btrfs subvolume snapshot` fails.
pub fn take_snapshot(source: &str, target: &str, read_only: bool) -> Result<()> {
    run_btrfs(&snapshot_args(source, target, read_only))
}

/// Refuse to delete anything that doesn't look like a snapshot path: the root directory, a
/// relative path or a path without a final component.
///
/// # Errors
///
/// Fails if the path is not acceptable.
pub fn check_deletable(path: &str) -> Result<()> {
    let p = Path::new(path);
    if path.is_empty()
        || is_same_character(path, '/')
        || !p.is_absolute()
        || p.file_name().is_none()
    {
        bail!("refusing to delete '{path}': it doesn't look like a snapshot path");
    }

    Ok(())
}

/// Delete the snapshot subvolume at `path`.
///
/// # Errors
///
/// Fails if the path is not acceptable or `btrfs subvolume delete` fails.
pub fn del_snapshot(path: &str) -> Result<()> {
    check_deletable(path)?;

    run_btrfs(&delete_args(path))
}

/// Restore `snapshot` as a read-write subvolume at `target`, which must not exist.
///
/// # Errors
///
/// Fails if the target exists, the snapshot is missing or `btrfs subvolume snapshot` fails.
pub fn restore_snapshot(snapshot: &str, target: &str) -> Result<()> {
    if strip_trailing_slash(target) == "/" {
        bail!(
            "restoring the root subvolume in place is not possible while it is mounted. Boot from another system, or restore somewhere else with --to"
        );
    }
    if Path::new(target).exists() {
        bail!(
            "the restore target {target} already exists. Move it out of the way first (for example: mv {target} {target}.old) or restore somewhere else with --to"
        );
    }
    if !Path::new(snapshot).exists() {
        bail!("the snapshot {snapshot} does not exist on disk");
    }

    run_btrfs(&snapshot_args(snapshot, target, false))
}

#[cfg(test)]
mod tests {
    use super::{
        check_deletable, command_line, create_args, delete_args, restore_snapshot, snapshot_args,
    };

    #[test]
    fn read_only_flag_goes_before_positionals() {
        assert_eq!(
            snapshot_args("/home/", "/.snapshots/x", true),
            ["subvolume", "snapshot", "-r", "/home", "/.snapshots/x"]
        );
        assert_eq!(
            snapshot_args("/", "/.snapshots/x/", false),
            ["subvolume", "snapshot", "/", "/.snapshots/x"]
        );
    }

    #[test]
    fn other_commands() {
        assert_eq!(
            create_args("/.snapshots/"),
            ["subvolume", "create", "/.snapshots"]
        );
        assert_eq!(
            delete_args("/.snapshots/x"),
            ["subvolume", "delete", "/.snapshots/x"]
        );
        assert_eq!(
            command_line(&create_args("/s")),
            "btrfs subvolume create /s"
        );
    }

    #[test]
    fn deletion_guard() {
        assert!(check_deletable("/").is_err());
        assert!(check_deletable("//").is_err());
        assert!(check_deletable("").is_err());
        assert!(check_deletable("relative/path").is_err());
        assert!(check_deletable("/.snapshots/root-2026-01-01-00-00-00-000000").is_ok());
        assert!(check_deletable("/.snapshots/").is_ok());
    }

    #[test]
    fn restore_refuses_the_root_directory() {
        let err = restore_snapshot("/nonexistent-snapshot", "/")
            .unwrap_err()
            .to_string();
        assert!(err.contains("root subvolume"), "{err}");
        assert!(!err.contains("mv "), "{err}");
    }

    #[test]
    fn restore_refuses_existing_target() {
        let err = restore_snapshot("/nonexistent-snapshot", "/tmp")
            .unwrap_err()
            .to_string();
        assert!(err.contains("already exists"), "{err}");
        assert!(err.contains("--to"), "{err}");
        let err = restore_snapshot("/nonexistent-snapshot", "/nonexistent-target")
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not exist on disk"), "{err}");
    }
}
