use {
    crate::{
        args::Args,
        database, operations, replication,
        structs::{ReplicaRecord, SnapshotRecord},
        utils::strip_trailing_slash,
    },
    anyhow::{Context, Result, bail},
    chrono::Utc,
    prettytable::{Table, row},
    sqlite::Connection,
    std::path::Path,
};

/// Name for a new snapshot: `<prefix>-<UTC timestamp with microseconds>`.
#[must_use]
pub fn new_snapshot_name(prefix: &str) -> String {
    format!("{prefix}-{}", Utc::now().format("%Y-%m-%d-%H-%M-%S-%6f"))
}

/// Take a snapshot and record it. If recording fails the snapshot is removed again so the
/// database and the disk don't drift apart.
///
/// # Errors
///
/// Fails if the snapshot can't be taken or recorded.
pub fn manage_creation(args: &Args, connection: &Connection) -> Result<()> {
    let name = new_snapshot_name(&args.snapshot_prefix);
    let record = SnapshotRecord {
        snap_id: format!("{:x}", md5::compute(&name)),
        name,
        kind: args.snapshot_kind.clone(),
        source: args.source_dir.clone(),
        destination: args.dest_dir.clone(),
        machine: args.machine.clone(),
        ro_rw: args.read_write.to_string(),
        date: String::new(),
    };
    let target = record.path();
    let btrfs_args = operations::snapshot_args(&args.source_dir, &target, !args.read_write);

    if args.dry_run {
        println!(
            "[dry-run] would run: {}",
            operations::command_line(&btrfs_args)
        );
        println!(
            "[dry-run] would record snapshot {} ({}) with kind '{}' for machine '{}'",
            record.name, record.snap_id, record.kind, record.machine
        );
        return Ok(());
    }

    operations::run_btrfs(&btrfs_args)?;
    if let Err(err) = database::insert_snapshot(connection, &record) {
        eprintln!(
            "Error: the snapshot {target} could not be recorded in the database, removing it"
        );
        if let Err(rollback) = operations::del_snapshot(&target) {
            eprintln!("Error: {rollback:#}. The snapshot {target} is left on disk but untracked.");
        }
        return Err(err).context("failed to record the snapshot in the database");
    }
    println!(
        "Snapshot {} ({}) created at {target}",
        record.name, record.snap_id
    );

    Ok(())
}

/// Delete the snapshot given with `--id`.
///
/// # Errors
///
/// Fails if the id is unknown or ambiguous, or the deletion fails.
pub fn manage_deletion(args: &Args, connection: &Connection) -> Result<()> {
    let record = find_one(connection, &args.snapshot_id)?;

    delete_record(args, connection, &record)
}

/// Restore the snapshot given with `--id` to its original source directory or to `--to`.
///
/// # Errors
///
/// Fails if the id is unknown or ambiguous, the target exists or the restore fails.
pub fn manage_restoring(args: &Args, connection: &Connection) -> Result<()> {
    let record = find_one(connection, &args.snapshot_id)?;
    let snapshot = record.path();
    let target = if args.dest_dir.is_empty() {
        strip_trailing_slash(&record.source).to_string()
    } else {
        strip_trailing_slash(&args.dest_dir).to_string()
    };

    if args.dry_run {
        println!(
            "[dry-run] would run: {}",
            operations::command_line(&operations::snapshot_args(&snapshot, &target, false))
        );
        if Path::new(&target).exists() {
            println!(
                "[dry-run] note: {target} already exists, the restore will fail until it is moved out of the way"
            );
        }
        return Ok(());
    }

    println!(
        "Restoring snapshot {} ({}) to {target}",
        record.name, record.snap_id
    );
    operations::restore_snapshot(&snapshot, &target)?;
    println!("The snapshot was successfully restored to {target}");

    Ok(())
}

/// Print every tracked snapshot.
///
/// # Errors
///
/// Fails on any database error.
pub fn manage_listing(connection: &Connection) -> Result<()> {
    let records = database::list_all(connection)?;
    if records.is_empty() {
        println!("No snapshots are tracked in the database.");
    } else {
        print_table(&records);
    }
    let replicas = database::list_replicas(connection)?;
    if !replicas.is_empty() {
        println!("\nReplicas:");
        print_replicas(&replicas);
    }

    Ok(())
}

/// Delete the snapshots beyond the last `--keep` ones matching the prefix, kind, ro/rw mode
/// and machine. With `--dry-run` only shows them.
///
/// # Errors
///
/// Fails on a database error or if any of the snapshots couldn't be deleted (the remaining ones
/// are still processed).
pub fn keep_only_x(args: &Args, connection: &Connection) -> Result<()> {
    let mode = if args.read_write { "rw" } else { "ro" };
    let candidates = database::cleanup_candidates(
        connection,
        &args.snapshot_prefix,
        &args.snapshot_kind,
        args.read_write,
        &args.machine,
        args.keep_only,
    )?;
    let selection = format!(
        "{mode} snapshots with prefix '{}', kind '{}' and machine '{}'",
        args.snapshot_prefix, args.snapshot_kind, args.machine
    );

    if candidates.is_empty() {
        println!(
            "Nothing to clean: there are no more than {} {selection}.",
            args.keep_only
        );
        return Ok(());
    }

    let verb = if args.dry_run {
        "Would delete"
    } else {
        "Deleting"
    };
    println!(
        "{verb} {} snapshot(s), keeping the last {} {selection}:",
        candidates.len(),
        args.keep_only
    );
    print_table(&candidates);
    if args.dry_run {
        println!("[dry-run] nothing was deleted.");
        return Ok(());
    }

    let mut failures = 0;
    for record in &candidates {
        if let Err(err) = delete_record(args, connection, record) {
            failures += 1;
            eprintln!("Error: {err:#}");
        }
    }
    if failures > 0 {
        bail!(
            "{failures} of {} snapshot(s) could not be deleted",
            candidates.len()
        );
    }

    Ok(())
}

fn find_one(connection: &Connection, id_or_name: &str) -> Result<SnapshotRecord> {
    let mut found = database::find_snapshots(connection, id_or_name)?;
    match found.len() {
        0 => bail!(
            "no snapshot found with id or name '{id_or_name}'. Use --list to see the tracked snapshots"
        ),
        1 => Ok(found.remove(0)),
        n => bail!("'{id_or_name}' matches {n} snapshots, use the snapshot id instead of the name"),
    }
}

/// Delete a snapshot from disk and from the database. If the subvolume is already gone from
/// this machine (but its directory is there) only the row is removed; if it belongs to another
/// machine or the whole snapshots directory is missing nothing is touched.
fn delete_record(args: &Args, connection: &Connection, record: &SnapshotRecord) -> Result<()> {
    let path = record.path();
    if args.dry_run {
        println!(
            "[dry-run] would delete snapshot {} ({}) at {path}",
            record.name, record.snap_id
        );
        return Ok(());
    }

    let on_disk = Path::new(&path).exists();
    if on_disk {
        operations::del_snapshot(&path)?;
    } else if record.machine != args.machine {
        bail!(
            "snapshot {} ({}) is not present at {path}: it was created on machine '{}' and this is '{}'",
            record.name,
            record.snap_id,
            record.machine,
            args.machine
        );
    } else if !Path::new(&path).is_absolute() || !Path::new(&record.destination).is_dir() {
        // The whole snapshots directory is missing, most likely the subvolume is not mounted.
        // Forgetting the row here would lose track of a snapshot that still exists.
        bail!(
            "snapshot {} ({}) is not present at {path} and its directory {} is not available either (is the snapshots subvolume mounted?). Keeping it in the database",
            record.name,
            record.snap_id,
            record.destination
        );
    } else {
        eprintln!(
            "Warning: snapshot {} ({}) no longer exists at {path}, removing it from the database only.",
            record.name, record.snap_id
        );
    }
    remove_filtered_copies(record);
    database::delete_snapshot(connection, record)?;
    if on_disk {
        println!("Snapshot {} ({}) deleted", record.name, record.snap_id);
    } else {
        println!(
            "Snapshot {} ({}) removed from the database",
            record.name, record.snap_id
        );
    }

    Ok(())
}

/// Delete the filtered copies built for replication (`<destination>/.staging/*/<name>`) of a
/// snapshot that is being deleted. Failures are reported but don't stop the deletion.
fn remove_filtered_copies(record: &SnapshotRecord) {
    let Ok(entries) = std::fs::read_dir(replication::staging_root(&record.destination)) else {
        return;
    };
    for entry in entries.flatten() {
        let copy = entry.path().join(&record.name);
        if !copy.exists() {
            continue;
        }
        let copy = copy.to_string_lossy().into_owned();
        match operations::del_snapshot(&copy) {
            Ok(()) => {
                println!("Deleted the filtered copy {copy}");
                // The per-list directory is a plain directory; drop it once empty.
                let _ = std::fs::remove_dir(entry.path());
            }
            Err(err) => eprintln!("Warning: {err:#}"),
        }
    }
}

fn print_replicas(replicas: &[ReplicaRecord]) {
    let mut table = Table::new();
    table.set_titles(row![
        bcFg => "NAME",
        "TARGET",
        "KIND",
        "MACHINE",
        "PARENT",
        "FILTERED",
        "REPLICATED"
    ]);
    for replica in replicas {
        table.add_row(row![ d =>
            replica.name,
            replica.target,
            replica.kind,
            replica.machine,
            replica.parent_name.as_deref().unwrap_or("-"),
            if replica.local_path.contains("/.staging/") { "yes" } else { "-" },
            replica.date,
        ]);
    }
    table.printstd();
}

fn print_table(records: &[SnapshotRecord]) {
    let mut table = Table::new();
    table.set_titles(row![
        bcFg => "NAME",
       "ID",
       "KIND",
       "SOURCE DIR",
       "DESTINATION DIR",
       "MACHINE",
       "RW",
       "DATE"
    ]);
    for record in records {
        table.add_row(row![ d =>
            record.name,
            record.snap_id,
            record.kind,
            record.source,
            record.destination,
            record.machine,
            record.ro_rw,
            record.date,
        ]);
    }
    table.printstd();
}

#[cfg(test)]
mod tests {
    use super::new_snapshot_name;

    #[test]
    fn snapshot_name_format() {
        let name = new_snapshot_name("root");
        // root-YYYY-MM-DD-HH-MM-SS-ffffff
        assert_eq!(name.len(), "root-2026-01-01-00-00-00-000000".len());
        assert!(name.starts_with("root-"));
        assert!(name[5..].chars().all(|c| c.is_ascii_digit() || c == '-'));
    }
}
