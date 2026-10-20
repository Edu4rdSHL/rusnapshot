use {
    crate::{
        args::Args,
        database, operations, replication,
        structs::{ReplicaRecord, SnapshotRecord},
        utils::strip_trailing_slash,
    },
    anyhow::{Context, Result, bail},
    chrono::Utc,
    prettytable::{
        Attr, Cell, Row, Table, color,
        format::{Alignment, FormatBuilder, LinePosition, LineSeparator, TableFormat},
        row,
    },
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

/// Delete the snapshot given with `--id`, or with `--from-replica` its replica at the target.
///
/// # Errors
///
/// Fails if the id is unknown or ambiguous, or the deletion fails.
pub fn manage_deletion(args: &Args, connection: &Connection) -> Result<()> {
    if let Some(target) = &args.from_replica {
        return delete_remote_replica(args, connection, target);
    }

    let record = find_one(connection, &args.snapshot_id)?;

    delete_record(args, connection, &record)
}

/// Restore the snapshot given with `--id` to its original source directory or to `--to`.
///
/// # Errors
///
/// Fails if the id is unknown or ambiguous, the target exists or the restore fails.
pub fn manage_restoring(args: &Args, connection: &Connection) -> Result<()> {
    if let Some(target) = &args.from_replica {
        return restore_from_replica(args, connection, target);
    }

    let record = find_one(connection, &args.snapshot_id)?;
    let snapshot = record.path();
    let target = restore_target(args, &record.source);

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

/// Where a restore goes: `--to` when given, else the directory the snapshot was taken from.
fn restore_target(args: &Args, source: &str) -> String {
    if args.dest_dir.is_empty() {
        strip_trailing_slash(source).to_string()
    } else {
        strip_trailing_slash(&args.dest_dir).to_string()
    }
}

/// Restore a snapshot from one of its replicas: it is brought back with `btrfs send`/`btrfs
/// receive` into `<dest_dir>/.restore/`, verified against the target, and the restored copy is
/// taken from it. The received subvolume is deleted afterwards; the restored copy keeps the data
/// because they share their extents.
///
/// The replica is looked up in the replicas table, so a snapshot no longer tracked on this
/// machine, rotated out by `--clean` for instance, can still be restored as long as its replica
/// is there. An empty `target` means the one recorded for it.
///
/// # Errors
///
/// Fails if the replica can't be resolved, the snapshot is already here, the restore target
/// exists, the replica is missing at the target, or the transfer or the verification fails.
fn restore_from_replica(args: &Args, connection: &Connection, target: &str) -> Result<()> {
    let replica = find_replica(connection, &args.snapshot_id, target, true)?;
    let restore_to = restore_target(args, &replica.source);
    let snapshots = snapshots_dir(&replica);
    let local = format!("{}/{}", strip_trailing_slash(&snapshots), replica.name);
    if Path::new(&local).exists() {
        bail!(
            "{} is already on this machine at {local}. Restore it with --restore --id {} and no --from-replica",
            replica.name,
            replica.snap_id
        );
    }
    if Path::new(&restore_to).exists() {
        bail!(
            "the restore target {restore_to} already exists. Move it out of the way first (for example: mv {restore_to} {restore_to}.old) or restore somewhere else with --to"
        );
    }

    let replication = external_replication(args, &replica.target)?;
    let remote = replication.subvolume_path(&replica.name);
    let filtered = replica.local_path.contains("/.staging/");
    if filtered {
        eprintln!(
            "Warning: the replica of {} at {} was sent from a filtered copy, so it does not contain the paths excluded for that target. The restore will be missing them.",
            replica.name, replication.url
        );
    }

    let staging = format!("{}/.restore", strip_trailing_slash(&snapshots));
    let received = format!("{staging}/{}", replica.name);

    if args.dry_run {
        println!(
            "[dry-run] would receive {remote} from {} into {staging} and restore it to {restore_to}",
            replication.url
        );
        return Ok(());
    }

    if !replication.exists(&remote)? {
        bail!("{remote} does not exist at {}", replication.url);
    }
    let source_info = replication.subvolume_info(&remote)?;

    // A copy left behind by an interrupted run would make the receive fail.
    if Path::new(&received).exists() {
        eprintln!("Warning: removing the leftover copy {received}");
        operations::del_snapshot(&received)?;
    }
    std::fs::create_dir_all(&staging).with_context(|| format!("failed to create {staging}"))?;

    println!(
        "Receiving {} from {} into {staging}",
        replica.name, replication.url
    );
    let transfer = match replication::receive_from_target(&replication, &replica.name, &staging) {
        Ok(transfer) => transfer,
        Err(err) => {
            let _ = operations::del_snapshot(&received);
            return Err(err);
        }
    };

    // A replica is itself a received subvolume, and `btrfs send` propagates the identity it was
    // received under, not the replica's own uuid. That original identity is what has to come out
    // the other side; only a replica created at the target by other means carries its own.
    let expected = source_info
        .received_uuid
        .as_deref()
        .unwrap_or(&source_info.uuid)
        .to_string();
    let landed = replication::local_subvolume_info(&received).and_then(|info| {
        if info.received_uuid.as_deref() == Some(expected.as_str()) {
            Ok(())
        } else {
            bail!(
                "verification of {} failed: received UUID {} does not match the UUID {expected} of {remote} at {}",
                replica.name,
                info.received_uuid.as_deref().unwrap_or("-"),
                replication.url
            )
        }
    });
    if let Err(err) = landed {
        let _ = operations::del_snapshot(&received);
        return Err(err);
    }

    let seconds = transfer.elapsed.as_secs_f64().max(0.001);
    println!(
        "Received {} in {seconds:.1}s, restoring to {restore_to}",
        replication::human_size(transfer.bytes)
    );
    if let Err(err) = operations::restore_snapshot(&received, &restore_to) {
        let _ = operations::del_snapshot(&received);
        return Err(err);
    }

    // The restored copy shares its extents with the received subvolume, so dropping it costs
    // nothing and leaves no untracked snapshot behind.
    if let Err(err) = operations::del_snapshot(&received) {
        eprintln!("Warning: {err:#}. The restore is complete, remove {received} by hand.");
    }
    let _ = std::fs::remove_dir(&staging);

    println!("The snapshot was successfully restored to {restore_to}");
    if filtered {
        println!(
            "Remember that the paths excluded for {} are not in it.",
            replication.url
        );
    }

    Ok(())
}

/// Delete the replica named by `--id` at its target and remember it as pruned, as the target
/// retention does, so the snapshot is not sent there again. Nothing on this machine is touched.
///
/// # Errors
///
/// Fails if the replica can't be resolved or the target can't be reached.
fn delete_remote_replica(args: &Args, connection: &Connection, target: &str) -> Result<()> {
    let replica = find_replica(connection, &args.snapshot_id, target, false)?;
    let replication = external_replication(args, &replica.target)?;
    let remote = replication.subvolume_path(&replica.name);
    if args.dry_run {
        println!(
            "[dry-run] would delete replica {} at {}",
            replica.name, replication.url
        );
        return Ok(());
    }

    if replication.exists(&remote)? {
        replication.delete_subvolume(&remote)?;
        database::mark_replica_pruned(connection, &replica)?;
        println!("Replica {} deleted at {}", replica.name, replication.url);
    } else {
        database::mark_replica_pruned(connection, &replica)?;
        eprintln!(
            "Warning: {remote} was already gone from {}, the replica is removed from the database only",
            replication.url
        );
    }

    Ok(())
}

/// The replica named by `--id`, a snapshot name or id, when `--from-replica` is given. Replicas
/// have their own table, so a snapshot no longer tracked on this machine is still found. An empty
/// `target` accepts any of them and fails when several targets hold one.
///
/// With `untracked_target`, a target with no record of the snapshot is accepted as long as the
/// snapshot is still tracked here: a restore from a disk this database never sent to.
///
/// # Errors
///
/// Fails when nothing matches or the match is ambiguous.
fn find_replica(
    connection: &Connection,
    id_or_name: &str,
    target: &str,
    untracked_target: bool,
) -> Result<ReplicaRecord> {
    let mut found: Vec<ReplicaRecord> = database::list_replicas(connection)?
        .into_iter()
        .filter(|replica| {
            (replica.name == id_or_name || replica.snap_id == id_or_name)
                && (target.is_empty() || replica.target == target)
        })
        .collect();

    match found.len() {
        1 => Ok(found.remove(0)),
        0 if untracked_target && !target.is_empty() => {
            let record = find_one(connection, id_or_name)?;
            Ok(ReplicaRecord {
                local_path: record.path(),
                name: record.name,
                snap_id: record.snap_id,
                target: target.to_string(),
                source: record.source,
                kind: record.kind,
                machine: record.machine,
                snapshot_date: record.date,
                parent_name: None,
                date: String::new(),
            })
        }
        0 if untracked_target => bail!(
            "no replica of '{id_or_name}' is recorded. If it is at a target the database does not know about, give it with --from-replica <TARGET>"
        ),
        0 if target.is_empty() => bail!("no replica of '{id_or_name}' is recorded"),
        0 => bail!("no replica of '{id_or_name}' is recorded at {target}"),
        _ => {
            let mut targets: Vec<String> =
                found.into_iter().map(|replica| replica.target).collect();
            targets.sort();
            bail!(
                "'{id_or_name}' is replicated to several targets, pick one with --from-replica <TARGET>: {}",
                targets.join(", ")
            )
        }
    }
}

/// Directory holding the snapshots a replica was sent from. Its `local_path` is either the
/// snapshot, `<dest_dir>/<name>`, or its filtered copy, `<dest_dir>/.staging/<id>/<name>`.
fn snapshots_dir(replica: &ReplicaRecord) -> String {
    if let Some((dir, _)) = replica.local_path.rsplit_once("/.staging/") {
        return dir.to_string();
    }
    Path::new(&replica.local_path)
        .parent()
        .map(|dir| dir.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Replication for a `--from-replica` target: reuses the `ssh_options` of the matching
/// `[[replicate]]` section so no extra configuration is needed.
fn external_replication(args: &Args, source: &str) -> Result<replication::Replication> {
    let config = args
        .replicate
        .iter()
        .find(|config| config.target == source)
        .cloned()
        .unwrap_or_else(|| crate::args::ReplicateConfig {
            target: source.to_string(),
            ..crate::args::ReplicateConfig::default()
        });

    replication::Replication::from_config(&config)
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
        println!();
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

    // `exists` would take an I/O error for a missing snapshot and drop the row of one still here.
    let on_disk = Path::new(&path)
        .try_exists()
        .with_context(|| format!("failed to check {path}"))?;
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

/// The default table with the `+` at the line junctions replaced by the line itself. These
/// tables are wider than a terminal, so the rules wrap at a different point than the rows they
/// separate and the crosses end up scattered across the screen.
fn plain_format() -> TableFormat {
    FormatBuilder::new()
        .column_separator('|')
        .borders('|')
        .separator(LinePosition::Top, LineSeparator::new('-', '-', '-', '-'))
        .separator(LinePosition::Title, LineSeparator::new('=', '=', '=', '='))
        .separator(LinePosition::Intern, LineSeparator::new('-', '-', '-', '-'))
        .separator(LinePosition::Bottom, LineSeparator::new('-', '-', '-', '-'))
        .padding(1, 1)
        .build()
}

/// A heading spanning the whole table, so each listing announces what it is.
fn banner(title: &str, columns: usize) -> Row {
    Row::new(vec![
        Cell::new_align(title, Alignment::CENTER)
            .with_style(Attr::Bold)
            .with_style(Attr::ForegroundColor(color::GREEN))
            .with_hspan(columns),
    ])
}

fn print_replicas(replicas: &[ReplicaRecord]) {
    let mut table = Table::new();
    table.set_format(plain_format());
    table.set_titles(banner("REPLICAS", 7));
    table.add_row(row![
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
    table.set_format(plain_format());
    table.set_titles(banner("SNAPSHOTS", 8));
    table.add_row(row![
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
