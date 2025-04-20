use {
    crate::{args::Args, utils::is_same_character},
    anyhow::Result,
    std::{
        path::{Path, PathBuf},
        process::Command,
    },
};

#[must_use]
pub fn take_snapshot(args: &Args, snapshot_name: &str) -> bool {
    let snapshot_name = format!("{}/{}", args.dest_dir, snapshot_name);
    let mut btrfs_args = vec!["subvolume", "snapshot", &args.source_dir, &snapshot_name];
    if !args.read_write {
        btrfs_args.push("-r");
    }
    Command::new("btrfs")
        .args(&btrfs_args)
        .status()
        .expect("Error while taking the snapshot.")
        .success()
}

#[must_use]
pub fn del_snapshot(snapshot_name: &str) -> bool {
    // Refuse to delete the root subvolume.
    if snapshot_name == "/" || is_same_character(snapshot_name, '/') {
        println!(
            "Snapshot name to delete is: {snapshot_name}. Refusing to delete the root subvolume."
        );
        std::process::exit(1);
    }
    Command::new("btrfs")
        .args(["subvolume", "delete", snapshot_name])
        .status()
        .expect("Error deleting the snapshot.")
        .success()
}

#[must_use]
pub fn restore_snapshot(args: &Args, snapshot_name: &str) -> bool {
    !Path::new(&args.dest_dir).exists()
        && Command::new("btrfs")
            .args(["subvolume", "snapshot", snapshot_name, &args.dest_dir])
            .status()
            .expect("Error restoring the snapshot.")
            .success()
}

pub fn setup_directory_structure(args: &Args) -> Result<()> {
    let dest_dir = PathBuf::from(&args.dest_dir);
    if !dest_dir.exists() {
        Command::new("btrfs")
            .args(["subvolume", "create", &args.dest_dir])
            .spawn()?;
    }

    Ok(())
}
