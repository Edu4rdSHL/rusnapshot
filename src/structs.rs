use crate::utils::strip_trailing_slash;

/// A snapshot as tracked in the database. Field order matches the table columns.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SnapshotRecord {
    pub name: String,
    pub snap_id: String,
    pub kind: String,
    pub source: String,
    pub destination: String,
    pub machine: String,
    pub ro_rw: String,
    pub date: String,
}

impl SnapshotRecord {
    /// Full path of the snapshot subvolume on disk (`<destination>/<name>`).
    ///
    /// Works both with destinations stored with a trailing slash (current format) and without it
    /// (databases created by older versions).
    #[must_use]
    pub fn path(&self) -> String {
        match strip_trailing_slash(&self.destination) {
            // Never turn a missing destination into an absolute path under the root directory.
            "" => self.name.clone(),
            "/" => format!("/{}", self.name),
            dir => format!("{dir}/{}", self.name),
        }
    }
}

/// A replica of a snapshot at a replication target, as tracked in the `replicas` table.
///
/// It carries its own copy of the snapshot metadata so it stays usable after the local
/// snapshot has been cleaned up.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ReplicaRecord {
    pub name: String,
    pub snap_id: String,
    /// Target URL as written in the configuration.
    pub target: String,
    pub local_path: String,
    pub source: String,
    pub kind: String,
    pub machine: String,
    /// Creation date of the snapshot (UTC, as stored in `snapshots.date`).
    pub snapshot_date: String,
    /// Snapshot used as parent for the incremental send, if any.
    pub parent_name: Option<String>,
    pub date: String,
}

#[cfg(test)]
mod tests {
    use super::SnapshotRecord;

    #[test]
    fn path_handles_trailing_slash_in_destination() {
        let with = SnapshotRecord {
            name: "root-2026-01-01-00-00-00-000000".into(),
            destination: "/.snapshots/".into(),
            ..SnapshotRecord::default()
        };
        let without = SnapshotRecord {
            destination: "/.snapshots".into(),
            ..with.clone()
        };
        assert_eq!(with.path(), "/.snapshots/root-2026-01-01-00-00-00-000000");
        assert_eq!(
            without.path(),
            "/.snapshots/root-2026-01-01-00-00-00-000000"
        );
    }

    #[test]
    fn path_edge_cases() {
        let root = SnapshotRecord {
            name: "x".into(),
            destination: "/".into(),
            ..SnapshotRecord::default()
        };
        assert_eq!(root.path(), "/x");
        let empty = SnapshotRecord {
            destination: String::new(),
            ..root.clone()
        };
        assert_eq!(empty.path(), "x");
        assert!(crate::operations::check_deletable(&empty.path()).is_err());
    }
}
