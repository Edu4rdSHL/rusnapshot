use sqlite::Connection;

#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Database {
    pub name: String,
    pub snap_id: String,
    pub kind: String,
    pub source: String,
    pub destination: String,
    pub ro_rw: String,
    pub machine: String,
    pub date: String,
}

pub struct ExtraArgs {
    pub snapshot_name: String,
    pub database_connection: Connection,
}
