use {
    crate::structs::Args,
    std::{path::Path, process::Command},
};

pub fn take_snapshot(args: &Args) -> bool {
    Command::new("btrfs")
        .args(&[
            "subvolume",
            "snapshot",
            "-r",
            &args.source_dir,
            &format!("{}/{}", args.dest_dir, args.snapshot_name),
        ])
        .status()
        .expect("Error while taking the snapshot.")
        .success()
}

pub fn del_snapshot(args: &Args) -> bool {
    Command::new("btrfs")
        .args(&["subvolume", "delete", &args.snapshot_name])
        .status()
        .expect("Error deleting the snapshot.")
        .success()
}

pub fn restore_snapshot(args: &Args) -> bool {
    (!Path::new(&args.source_dir).exists()
        || Command::new("btrfs")
            .args(&["subvolume", "delete", &args.source_dir])
            .status()
            .expect("Error deleting the subvolume.")
            .success())
        && Command::new("btrfs")
            .args(&[
                "subvolume",
                "snapshot",
                &args.snapshot_name,
                &args.source_dir,
            ])
            .status()
            .expect("Error restoring the snapshot.")
            .success()
}
