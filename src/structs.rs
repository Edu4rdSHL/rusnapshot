#[derive(Clone, Debug)]
pub struct Args {
    pub create_snapshot: bool,
    pub delete_snapshot: bool,
    pub list_snapshots: bool,
    pub clean_snapshots: bool,
    pub restore_snapshot: bool,
    pub rw_snapshots: bool,
    pub dest_dir: String,
    pub source_dir: String,
    pub database_file: String,
    pub snapshot_name: String,
    pub snapshot_id: String,
    pub snapshot_prefix: String,
    pub snapshot_kind: String,
    pub snapshot_ro_rw: String,
    pub keep_only: usize,
}

pub struct Database {
    pub name: String,
    pub snap_id: String,
    pub kind: String,
    pub source: String,
    pub destination: String,
    pub ro_rw: String,
    pub date: String,
}

impl Database {
    pub fn default() -> Database {
        Database {
            name: String::new(),
            snap_id: String::new(),
            kind: String::new(),
            source: String::new(),
            destination: String::new(),
            ro_rw: String::new(),
            date: String::new(),
        }
    }
}
