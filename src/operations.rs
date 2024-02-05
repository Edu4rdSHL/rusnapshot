use crate::utils::is_same_character;
use std::path::PathBuf;

use {
    crate::args::Args,
    anyhow::Result,
    std::{path::Path, process::Command},
};

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

pub fn del_snapshot(snapshot_name: &str) -> bool {
    // Refuse to delete the root subvolume.
    if snapshot_name == "/" || is_same_character(snapshot_name, '/') {
        println!(
            "Snapshot name to delete is: {}. Refusing to delete the root subvolume.",
            snapshot_name
        );
        std::process::exit(1);
    }
    Command::new("btrfs")
        .args(["subvolume", "delete", snapshot_name])
        .status()
        .expect("Error deleting the snapshot.")
        .success()
}

pub fn restore_snapshot(args: &Args, snapshot_name: &str) -> bool {
    (!Path::new(&args.source_dir).exists()
        || Command::new("btrfs")
            .args(["subvolume", "delete", &args.source_dir])
            .status()
            .expect("Error deleting the subvolume.")
            .success())
        && Command::new("btrfs")
            .args(["subvolume", "snapshot", &snapshot_name, &args.source_dir])
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
