use {
    crate::{
        args::Args,
        structs::{ReplicaRecord, SnapshotRecord},
    },
    anyhow::{Context, Result, bail},
    sqlite::{Connection, State, Statement},
    std::{collections::HashSet, path::Path},
};

const SELECT_COLUMNS: &str =
    "name, snap_id, kind, source, destination, machine, ro_rw, datetime(date, 'localtime')";
const REPLICA_COLUMNS: &str = "name, snap_id, target, local_path, source, kind, machine, snapshot_date, parent_name, datetime(date, 'localtime')";
const QUOTED_MACHINE: &str =
    "length(machine) >= 2 AND substr(machine, 1, 1) = '\"' AND substr(machine, -1, 1) = '\"'";

/// Open the database and make sure the schema is in place.
///
/// The file is only created when taking a snapshot (its parent directory too); the other
/// operations fail with a clear message if it doesn't exist. With `--dry-run` a missing database
/// is replaced by an empty in-memory one so nothing is written to disk.
///
/// # Errors
///
/// Fails if the file can't be opened or created, or the schema can't be set up.
pub fn open(args: &Args) -> Result<Connection> {
    let path = Path::new(&args.database_file);
    if !path.exists() {
        if args.dry_run {
            let connection = sqlite::open(":memory:")?;
            setup_initial_database(&connection)?;
            return Ok(connection);
        }
        if !args.create_snapshot {
            bail!(
                "the database file {} does not exist. Create a snapshot first, or point to the right file with -d/--dfile",
                path.display()
            );
        }
        let missing_parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty() && !parent.exists());
        if let Some(parent) = missing_parent {
            std::fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create the database directory {}",
                    parent.display()
                )
            })?;
        }
    }

    let mut connection = sqlite::open(path)
        .with_context(|| format!("failed to open the database {}", path.display()))?;
    connection
        .set_busy_timeout(args.timeout)
        .context("failed to set the database busy timeout")?;
    setup_initial_database(&connection).with_context(|| {
        format!(
            "failed to access the database {} (it is in WAL mode, so its directory must be writable too)",
            path.display()
        )
    })?;

    Ok(connection)
}

/// Create the schema when missing and apply data migrations.
///
/// # Errors
///
/// Fails on any `SQLite` error.
pub fn setup_initial_database(connection: &Connection) -> Result<()> {
    if !table_exists(connection, "snapshots")? {
        connection.execute(
            "CREATE TABLE snapshots (name TEXT NOT NULL, snap_id TEXT NOT NULL, kind TEXT NOT NULL, source TEXT NOT NULL, destination TEXT NOT NULL, machine TEXT NOT NULL, ro_rw TEXT NOT NULL, date TEXT DEFAULT CURRENT_TIMESTAMP, PRIMARY KEY(name, snap_id))",
        )?;
        connection.execute("PRAGMA journal_mode=WAL")?;
    }
    if !table_exists(connection, "replicas")? {
        connection.execute(
            "CREATE TABLE replicas (name TEXT NOT NULL, snap_id TEXT NOT NULL, target TEXT NOT NULL, local_path TEXT NOT NULL, source TEXT NOT NULL, kind TEXT NOT NULL, machine TEXT NOT NULL, snapshot_date TEXT NOT NULL, parent_name TEXT, date TEXT DEFAULT CURRENT_TIMESTAMP, pruned TEXT, PRIMARY KEY(name, snap_id, target))",
        )?;
    }
    migrate_quoted_machines(connection)?;

    Ok(())
}

fn table_exists(connection: &Connection, table: &str) -> Result<bool> {
    let mut statement =
        connection.prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1")?;
    statement.bind((1, table))?;

    Ok(statement.next()? == State::Row)
}

/// Versions up to 0.5.3 stored the `machine` value coming from the configuration file with its
/// TOML quotes (`"host"` instead of `host`). Strip them so machine filtering works.
fn migrate_quoted_machines(connection: &Connection) -> Result<()> {
    let affected = {
        let mut statement = connection.prepare(format!(
            "SELECT count(*) FROM snapshots WHERE {QUOTED_MACHINE}"
        ))?;
        statement.next()?;
        statement.read::<i64, _>(0)?
    };
    if affected > 0 {
        connection.execute(format!(
            "UPDATE snapshots SET machine = substr(machine, 2, length(machine) - 2) WHERE {QUOTED_MACHINE}"
        ))?;
    }

    Ok(())
}

/// Record a new snapshot. The date is filled by the database.
///
/// # Errors
///
/// Fails on any `SQLite` error, including a duplicated name/id.
pub fn insert_snapshot(connection: &Connection, record: &SnapshotRecord) -> Result<()> {
    let mut statement = connection.prepare(
        "INSERT INTO snapshots (name, snap_id, kind, source, destination, machine, ro_rw) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )?;
    statement.bind((1, record.name.as_str()))?;
    statement.bind((2, record.snap_id.as_str()))?;
    statement.bind((3, record.kind.as_str()))?;
    statement.bind((4, record.source.as_str()))?;
    statement.bind((5, record.destination.as_str()))?;
    statement.bind((6, record.machine.as_str()))?;
    statement.bind((7, record.ro_rw.as_str()))?;
    statement.next()?;

    Ok(())
}

/// Remove a snapshot row.
///
/// # Errors
///
/// Fails on any `SQLite` error.
pub fn delete_snapshot(connection: &Connection, record: &SnapshotRecord) -> Result<()> {
    let mut statement =
        connection.prepare("DELETE FROM snapshots WHERE name = ?1 AND snap_id = ?2")?;
    statement.bind((1, record.name.as_str()))?;
    statement.bind((2, record.snap_id.as_str()))?;
    statement.next()?;

    Ok(())
}

/// Snapshots whose name or id is `id_or_name`.
///
/// # Errors
///
/// Fails on any `SQLite` error.
pub fn find_snapshots(connection: &Connection, id_or_name: &str) -> Result<Vec<SnapshotRecord>> {
    let mut statement = connection.prepare(format!(
        "SELECT {SELECT_COLUMNS} FROM snapshots WHERE name = ?1 OR snap_id = ?1 ORDER BY date DESC, name DESC"
    ))?;
    statement.bind((1, id_or_name))?;

    collect(&mut statement)
}

/// Every tracked snapshot, newest first.
///
/// # Errors
///
/// Fails on any `SQLite` error.
pub fn list_all(connection: &Connection) -> Result<Vec<SnapshotRecord>> {
    let mut statement = connection.prepare(format!(
        "SELECT {SELECT_COLUMNS} FROM snapshots ORDER BY date DESC, name DESC"
    ))?;

    collect(&mut statement)
}

/// Snapshots that `--clean` would delete: everything beyond the `keep` newest ones among the
/// snapshots whose name starts with `prefix` (a `LIKE 'prefix%'` match, on purpose) and that
/// match `kind`, the ro/rw mode and `machine`.
///
/// # Errors
///
/// Fails on any `SQLite` error.
pub fn cleanup_candidates(
    connection: &Connection,
    prefix: &str,
    kind: &str,
    read_write: bool,
    machine: &str,
    keep: usize,
) -> Result<Vec<SnapshotRecord>> {
    let pattern = format!("{prefix}%");
    let ro_rw = read_write.to_string();
    let keep = i64::try_from(keep).context("--keep is too large")?;
    let mut statement = connection.prepare(format!(
        "SELECT {SELECT_COLUMNS} FROM (SELECT row_number() OVER (ORDER BY date DESC, name DESC) AS n, * FROM snapshots WHERE name LIKE ?1 AND kind = ?2 AND ro_rw = ?3 AND machine = ?4) WHERE n > ?5 ORDER BY date DESC, name DESC"
    ))?;
    statement.bind((1, pattern.as_str()))?;
    statement.bind((2, kind))?;
    statement.bind((3, ro_rw.as_str()))?;
    statement.bind((4, machine))?;
    statement.bind((5, keep))?;

    collect(&mut statement)
}

/// Read-only snapshots of this prefix (`LIKE 'prefix%'`) and machine, oldest first, with the
/// raw UTC date so it can be copied into the replicas table.
///
/// # Errors
///
/// Fails on any `SQLite` error.
pub fn pending_replication(
    connection: &Connection,
    prefix: &str,
    machine: &str,
) -> Result<Vec<SnapshotRecord>> {
    let pattern = format!("{prefix}%");
    let mut statement = connection.prepare(
        "SELECT name, snap_id, kind, source, destination, machine, ro_rw, date FROM snapshots WHERE name LIKE ?1 AND machine = ?2 AND ro_rw = 'false' ORDER BY date ASC, name ASC",
    )?;
    statement.bind((1, pattern.as_str()))?;
    statement.bind((2, machine))?;

    collect(&mut statement)
}

/// Record a verified replica.
///
/// # Errors
///
/// Fails on any `SQLite` error.
pub fn insert_replica(connection: &Connection, replica: &ReplicaRecord) -> Result<()> {
    let mut statement = connection.prepare(
        "INSERT OR REPLACE INTO replicas (name, snap_id, target, local_path, source, kind, machine, snapshot_date, parent_name) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )?;
    statement.bind((1, replica.name.as_str()))?;
    statement.bind((2, replica.snap_id.as_str()))?;
    statement.bind((3, replica.target.as_str()))?;
    statement.bind((4, replica.local_path.as_str()))?;
    statement.bind((5, replica.source.as_str()))?;
    statement.bind((6, replica.kind.as_str()))?;
    statement.bind((7, replica.machine.as_str()))?;
    statement.bind((8, replica.snapshot_date.as_str()))?;
    statement.bind((9, replica.parent_name.as_deref()))?;
    statement.next()?;

    Ok(())
}

/// Mark a replica as deleted from the target by the remote retention. The row stays so the
/// snapshot is not sent again; it only stops being a parent candidate.
///
/// # Errors
///
/// Fails on any `SQLite` error.
pub fn mark_replica_pruned(connection: &Connection, replica: &ReplicaRecord) -> Result<()> {
    let mut statement = connection.prepare(
        "UPDATE replicas SET pruned = CURRENT_TIMESTAMP WHERE name = ?1 AND snap_id = ?2 AND target = ?3",
    )?;
    statement.bind((1, replica.name.as_str()))?;
    statement.bind((2, replica.snap_id.as_str()))?;
    statement.bind((3, replica.target.as_str()))?;
    statement.next()?;

    Ok(())
}

/// Forget a replica completely, so the snapshot becomes pending again.
///
/// # Errors
///
/// Fails on any `SQLite` error.
pub fn delete_replica(connection: &Connection, replica: &ReplicaRecord) -> Result<()> {
    let mut statement = connection
        .prepare("DELETE FROM replicas WHERE name = ?1 AND snap_id = ?2 AND target = ?3")?;
    statement.bind((1, replica.name.as_str()))?;
    statement.bind((2, replica.snap_id.as_str()))?;
    statement.bind((3, replica.target.as_str()))?;
    statement.next()?;

    Ok(())
}

/// Names of the snapshots already replicated to `target`, including the replicas that the
/// remote retention deleted since (they must not be sent again).
///
/// # Errors
///
/// Fails on any `SQLite` error.
pub fn replicated_names(connection: &Connection, target: &str) -> Result<HashSet<String>> {
    let mut statement = connection.prepare("SELECT name FROM replicas WHERE target = ?1")?;
    statement.bind((1, target))?;
    let mut names = HashSet::new();
    while statement.next()? == State::Row {
        names.insert(statement.read::<String, _>(0)?);
    }

    Ok(names)
}

/// Replicas present at `target` of the same source subvolume and machine, newest snapshot
/// first: the candidates to be the parent of an incremental send.
///
/// # Errors
///
/// Fails on any `SQLite` error.
pub fn parent_candidates(
    connection: &Connection,
    target: &str,
    source: &str,
    machine: &str,
) -> Result<Vec<ReplicaRecord>> {
    let mut statement = connection.prepare(format!(
        "SELECT {REPLICA_COLUMNS} FROM replicas WHERE target = ?1 AND source = ?2 AND machine = ?3 AND pruned IS NULL ORDER BY snapshot_date DESC, name DESC"
    ))?;
    statement.bind((1, target))?;
    statement.bind((2, source))?;
    statement.bind((3, machine))?;

    collect_replicas(&mut statement)
}

/// Kinds of the replicas present at `target` for this prefix and machine.
///
/// # Errors
///
/// Fails on any `SQLite` error.
pub fn replica_kinds(
    connection: &Connection,
    target: &str,
    prefix: &str,
    machine: &str,
) -> Result<Vec<String>> {
    let pattern = format!("{prefix}%");
    let mut statement = connection.prepare(
        "SELECT DISTINCT kind FROM replicas WHERE target = ?1 AND name LIKE ?2 AND machine = ?3 AND pruned IS NULL ORDER BY kind",
    )?;
    statement.bind((1, target))?;
    statement.bind((2, pattern.as_str()))?;
    statement.bind((3, machine))?;
    let mut kinds = Vec::new();
    while statement.next()? == State::Row {
        kinds.push(statement.read::<String, _>(0)?);
    }

    Ok(kinds)
}

/// Replicas present at `target` beyond the newest `keep` ones of this prefix, kind and machine.
///
/// # Errors
///
/// Fails on any `SQLite` error.
pub fn remote_prune_candidates(
    connection: &Connection,
    target: &str,
    prefix: &str,
    kind: &str,
    machine: &str,
    keep: usize,
) -> Result<Vec<ReplicaRecord>> {
    let pattern = format!("{prefix}%");
    let keep = i64::try_from(keep).context("keep is too large")?;
    let mut statement = connection.prepare(format!(
        "SELECT {REPLICA_COLUMNS} FROM (SELECT row_number() OVER (ORDER BY snapshot_date DESC, name DESC) AS n, * FROM replicas WHERE target = ?1 AND name LIKE ?2 AND kind = ?3 AND machine = ?4 AND pruned IS NULL) WHERE n > ?5 ORDER BY snapshot_date ASC, name ASC"
    ))?;
    statement.bind((1, target))?;
    statement.bind((2, pattern.as_str()))?;
    statement.bind((3, kind))?;
    statement.bind((4, machine))?;
    statement.bind((5, keep))?;

    collect_replicas(&mut statement)
}

/// Every replica present at its target, newest first.
///
/// # Errors
///
/// Fails on any `SQLite` error.
pub fn list_replicas(connection: &Connection) -> Result<Vec<ReplicaRecord>> {
    let mut statement = connection.prepare(format!(
        "SELECT {REPLICA_COLUMNS} FROM replicas WHERE pruned IS NULL ORDER BY snapshot_date DESC, name DESC, target"
    ))?;

    collect_replicas(&mut statement)
}

fn collect_replicas(statement: &mut Statement) -> Result<Vec<ReplicaRecord>> {
    let mut replicas = Vec::new();
    while statement.next()? == State::Row {
        replicas.push(ReplicaRecord {
            name: statement.read(0)?,
            snap_id: statement.read(1)?,
            target: statement.read(2)?,
            local_path: statement.read(3)?,
            source: statement.read(4)?,
            kind: statement.read(5)?,
            machine: statement.read(6)?,
            snapshot_date: statement.read(7)?,
            parent_name: statement.read::<Option<String>, _>(8)?,
            date: statement.read::<Option<String>, _>(9)?.unwrap_or_default(),
        });
    }

    Ok(replicas)
}

fn collect(statement: &mut Statement) -> Result<Vec<SnapshotRecord>> {
    let mut records = Vec::new();
    while statement.next()? == State::Row {
        records.push(read_record(statement)?);
    }

    Ok(records)
}

fn read_record(statement: &Statement) -> Result<SnapshotRecord> {
    Ok(SnapshotRecord {
        name: statement.read(0)?,
        snap_id: statement.read(1)?,
        kind: statement.read(2)?,
        source: statement.read(3)?,
        destination: statement.read(4)?,
        machine: statement.read(5)?,
        ro_rw: statement.read(6)?,
        date: statement.read::<Option<String>, _>(7)?.unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use {
        super::{
            cleanup_candidates, delete_replica, delete_snapshot, find_snapshots, insert_replica,
            insert_snapshot, list_all, list_replicas, mark_replica_pruned, parent_candidates,
            pending_replication, remote_prune_candidates, replica_kinds, replicated_names,
            setup_initial_database,
        },
        crate::structs::{ReplicaRecord, SnapshotRecord},
        sqlite::Connection,
    };

    fn db() -> Connection {
        let connection = sqlite::open(":memory:").unwrap();
        setup_initial_database(&connection).unwrap();
        connection
    }

    /// Insert a record with an explicit date so ordering is deterministic.
    fn insert(
        connection: &Connection,
        name: &str,
        kind: &str,
        machine: &str,
        rw: bool,
        date: &str,
    ) {
        let record = SnapshotRecord {
            name: name.into(),
            snap_id: format!("id-{name}"),
            kind: kind.into(),
            source: "/home/".into(),
            destination: "/.snapshots/".into(),
            machine: machine.into(),
            ro_rw: rw.to_string(),
            date: String::new(),
        };
        insert_snapshot(connection, &record).unwrap();
        connection
            .execute(format!(
                "UPDATE snapshots SET date = '{date}' WHERE name = '{name}'"
            ))
            .unwrap();
    }

    fn names(records: &[SnapshotRecord]) -> Vec<&str> {
        records.iter().map(|r| r.name.as_str()).collect()
    }

    #[test]
    fn setup_is_idempotent() {
        let connection = db();
        setup_initial_database(&connection).unwrap();
        setup_initial_database(&connection).unwrap();
        assert!(list_all(&connection).unwrap().is_empty());
    }

    #[test]
    fn insert_find_delete() {
        let connection = db();
        insert(
            &connection,
            "root-1",
            "daily",
            "host",
            false,
            "2026-01-01 00:00:00",
        );
        let by_name = find_snapshots(&connection, "root-1").unwrap();
        let by_id = find_snapshots(&connection, "id-root-1").unwrap();
        assert_eq!(by_name, by_id);
        assert_eq!(by_name.len(), 1);
        assert_eq!(by_name[0].machine, "host");
        assert_eq!(by_name[0].ro_rw, "false");
        assert!(!by_name[0].date.is_empty());
        assert!(find_snapshots(&connection, "nope").unwrap().is_empty());

        delete_snapshot(&connection, &by_name[0]).unwrap();
        assert!(list_all(&connection).unwrap().is_empty());
    }

    #[test]
    fn values_with_quotes_are_stored_verbatim() {
        let connection = db();
        let record = SnapshotRecord {
            name: "it's-1".into(),
            snap_id: "id".into(),
            kind: "k\"k".into(),
            source: "/home/it's/".into(),
            destination: "/.snapshots/".into(),
            machine: "o'host".into(),
            ro_rw: "false".into(),
            date: String::new(),
        };
        insert_snapshot(&connection, &record).unwrap();
        let found = find_snapshots(&connection, "it's-1").unwrap();
        assert_eq!(found[0].source, "/home/it's/");
        assert_eq!(found[0].machine, "o'host");
        assert_eq!(found[0].kind, "k\"k");
    }

    #[test]
    fn duplicated_snapshot_is_rejected() {
        let connection = db();
        insert(
            &connection,
            "root-1",
            "daily",
            "host",
            false,
            "2026-01-01 00:00:00",
        );
        let record = find_snapshots(&connection, "root-1").unwrap().remove(0);
        assert!(insert_snapshot(&connection, &record).is_err());
    }

    #[test]
    fn list_is_newest_first() {
        let connection = db();
        insert(&connection, "a", "k", "h", false, "2026-01-01 00:00:00");
        insert(&connection, "c", "k", "h", false, "2026-01-03 00:00:00");
        insert(&connection, "b", "k", "h", false, "2026-01-02 00:00:00");
        assert_eq!(names(&list_all(&connection).unwrap()), ["c", "b", "a"]);
    }

    #[test]
    fn cleanup_keeps_the_newest() {
        let connection = db();
        for day in 1..=5 {
            insert(
                &connection,
                &format!("root-{day}"),
                "daily",
                "h",
                false,
                &format!("2026-01-0{day} 00:00:00"),
            );
        }
        let candidates = cleanup_candidates(&connection, "root", "daily", false, "h", 3).unwrap();
        assert_eq!(names(&candidates), ["root-2", "root-1"]);
        assert!(
            cleanup_candidates(&connection, "root", "daily", false, "h", 5)
                .unwrap()
                .is_empty()
        );
        let all = cleanup_candidates(&connection, "root", "daily", false, "h", 0).unwrap();
        assert_eq!(all.len(), 5);
    }

    #[test]
    fn cleanup_uses_name_as_tiebreaker_for_same_second() {
        let connection = db();
        insert(
            &connection,
            "root-2026-01-01-00-00-00-000001",
            "daily",
            "h",
            false,
            "2026-01-01 00:00:00",
        );
        insert(
            &connection,
            "root-2026-01-01-00-00-00-000002",
            "daily",
            "h",
            false,
            "2026-01-01 00:00:00",
        );
        let candidates = cleanup_candidates(&connection, "root", "daily", false, "h", 1).unwrap();
        assert_eq!(names(&candidates), ["root-2026-01-01-00-00-00-000001"]);
    }

    #[test]
    fn cleanup_prefix_is_a_like_pattern() {
        // Intentional behaviour: `home` also matches `home-data-*`.
        let connection = db();
        insert(
            &connection,
            "home-1",
            "k",
            "h",
            false,
            "2026-01-01 00:00:00",
        );
        insert(
            &connection,
            "home-data-1",
            "k",
            "h",
            false,
            "2026-01-02 00:00:00",
        );
        insert(
            &connection,
            "root-1",
            "k",
            "h",
            false,
            "2026-01-03 00:00:00",
        );
        let candidates = cleanup_candidates(&connection, "home", "k", false, "h", 0).unwrap();
        assert_eq!(names(&candidates), ["home-data-1", "home-1"]);
    }

    #[test]
    fn cleanup_filters_by_kind_mode_and_machine() {
        let connection = db();
        insert(
            &connection,
            "root-1",
            "daily",
            "A",
            false,
            "2026-01-01 00:00:00",
        );
        insert(
            &connection,
            "root-2",
            "weekly",
            "A",
            false,
            "2026-01-02 00:00:00",
        );
        insert(
            &connection,
            "root-3",
            "daily",
            "A",
            true,
            "2026-01-03 00:00:00",
        );
        insert(
            &connection,
            "root-4",
            "daily",
            "B",
            false,
            "2026-01-04 00:00:00",
        );
        insert(
            &connection,
            "root-5",
            "daily",
            "B",
            false,
            "2026-01-05 00:00:00",
        );

        let daily_ro_a = cleanup_candidates(&connection, "root", "daily", false, "A", 0).unwrap();
        assert_eq!(names(&daily_ro_a), ["root-1"]);
        let daily_rw_a = cleanup_candidates(&connection, "root", "daily", true, "A", 0).unwrap();
        assert_eq!(names(&daily_rw_a), ["root-3"]);
        let weekly_a = cleanup_candidates(&connection, "root", "weekly", false, "A", 0).unwrap();
        assert_eq!(names(&weekly_a), ["root-2"]);
        // Machine B's newer snapshots must not push machine A's out of its own keep window.
        assert!(
            cleanup_candidates(&connection, "root", "daily", false, "A", 1)
                .unwrap()
                .is_empty()
        );
        let daily_b = cleanup_candidates(&connection, "root", "daily", false, "B", 1).unwrap();
        assert_eq!(names(&daily_b), ["root-4"]);
    }

    fn replica(
        name: &str,
        target: &str,
        kind: &str,
        date: &str,
        parent: Option<&str>,
    ) -> ReplicaRecord {
        ReplicaRecord {
            name: name.into(),
            snap_id: format!("id-{name}"),
            target: target.into(),
            local_path: format!("/.snapshots/{name}"),
            source: "/home/".into(),
            kind: kind.into(),
            machine: "h".into(),
            snapshot_date: date.into(),
            parent_name: parent.map(str::to_string),
            date: String::new(),
        }
    }

    #[test]
    fn pending_replication_is_ro_only_and_oldest_first() {
        let connection = db();
        insert(
            &connection,
            "root-2",
            "daily",
            "h",
            false,
            "2026-01-02 00:00:00",
        );
        insert(
            &connection,
            "root-1",
            "daily",
            "h",
            false,
            "2026-01-01 00:00:00",
        );
        insert(
            &connection,
            "root-3",
            "daily",
            "h",
            true,
            "2026-01-03 00:00:00",
        );
        insert(
            &connection,
            "root-4",
            "daily",
            "other",
            false,
            "2026-01-04 00:00:00",
        );
        insert(
            &connection,
            "home-1",
            "daily",
            "h",
            false,
            "2026-01-05 00:00:00",
        );
        let pending = pending_replication(&connection, "root", "h").unwrap();
        assert_eq!(names(&pending), ["root-1", "root-2"]);
        // Raw UTC date, not converted to local time.
        assert_eq!(pending[0].date, "2026-01-01 00:00:00");
    }

    #[test]
    fn replica_queries() {
        let connection = db();
        let nas = "ssh://nas/srv";
        insert_replica(
            &connection,
            &replica("root-1", nas, "daily", "2026-01-01 00:00:00", None),
        )
        .unwrap();
        insert_replica(
            &connection,
            &replica(
                "root-2",
                nas,
                "daily",
                "2026-01-02 00:00:00",
                Some("root-1"),
            ),
        )
        .unwrap();
        insert_replica(
            &connection,
            &replica(
                "root-3",
                nas,
                "weekly",
                "2026-01-03 00:00:00",
                Some("root-2"),
            ),
        )
        .unwrap();
        insert_replica(
            &connection,
            &replica("root-4", "/mnt/usb", "daily", "2026-01-04 00:00:00", None),
        )
        .unwrap();
        insert_replica(
            &connection,
            &replica("home-1", nas, "daily", "2026-01-05 00:00:00", None),
        )
        .unwrap();

        let names_at_nas = replicated_names(&connection, nas).unwrap();
        assert_eq!(names_at_nas.len(), 4);
        assert!(names_at_nas.contains("root-3") && !names_at_nas.contains("root-4"));

        let parents = parent_candidates(&connection, nas, "/home/", "h").unwrap();
        let parent_names: Vec<&str> = parents.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(parent_names, ["home-1", "root-3", "root-2", "root-1"]);
        assert_eq!(parents[1].parent_name.as_deref(), Some("root-2"));
        assert!(parents[3].parent_name.is_none());
        assert!(
            parent_candidates(&connection, nas, "/other/", "h")
                .unwrap()
                .is_empty()
        );

        assert_eq!(
            replica_kinds(&connection, nas, "root", "h").unwrap(),
            ["daily", "weekly"]
        );
        let prune = remote_prune_candidates(&connection, nas, "root", "daily", "h", 1).unwrap();
        assert_eq!(prune.len(), 1);
        assert_eq!(prune[0].name, "root-1");
        assert!(
            remote_prune_candidates(&connection, nas, "root", "weekly", "h", 1)
                .unwrap()
                .is_empty()
        );

        // Re-recording an existing replica replaces it instead of failing.
        insert_replica(
            &connection,
            &replica("root-1", nas, "daily", "2026-01-01 00:00:00", None),
        )
        .unwrap();
        // Pruned replicas stay recorded, so they are not sent again, but stop being parents.
        assert_eq!(list_replicas(&connection).unwrap().len(), 5);
        mark_replica_pruned(&connection, &prune[0]).unwrap();
        assert!(
            replicated_names(&connection, nas)
                .unwrap()
                .contains("root-1")
        );
        assert!(
            parent_candidates(&connection, nas, "/home/", "h")
                .unwrap()
                .iter()
                .all(|r| r.name != "root-1")
        );
        assert!(
            remote_prune_candidates(&connection, nas, "root", "daily", "h", 1)
                .unwrap()
                .is_empty()
        );
        assert_eq!(list_replicas(&connection).unwrap().len(), 4);
        assert!(!list_replicas(&connection).unwrap()[0].date.is_empty());
        // Forgetting a replica removes it completely, so its snapshot is pending again.
        delete_replica(&connection, &prune[0]).unwrap();
        assert!(
            !replicated_names(&connection, nas)
                .unwrap()
                .contains("root-1")
        );
        assert_eq!(list_replicas(&connection).unwrap().len(), 4);
    }

    #[test]
    fn quoted_machines_are_migrated() {
        let connection = db();
        insert(
            &connection,
            "root-1",
            "daily",
            "\"Oribos\"",
            false,
            "2026-01-01 00:00:00",
        );
        insert(
            &connection,
            "root-2",
            "daily",
            "Oribos",
            false,
            "2026-01-02 00:00:00",
        );
        insert(
            &connection,
            "root-3",
            "daily",
            "\"",
            false,
            "2026-01-03 00:00:00",
        );
        setup_initial_database(&connection).unwrap();
        let machines: Vec<String> = list_all(&connection)
            .unwrap()
            .into_iter()
            .map(|r| r.machine)
            .collect();
        assert_eq!(machines, ["\"", "Oribos", "Oribos"]);
        assert_eq!(
            cleanup_candidates(&connection, "root", "daily", false, "Oribos", 0)
                .unwrap()
                .len(),
            2
        );
    }
}
