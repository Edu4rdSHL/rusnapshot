use {
    crate::{database, operations, structs::ExtraArgs},
    anyhow::Result,
    clap::Parser,
    serde::{Deserialize, Serialize},
    sqlite::Connection,
    std::collections::BTreeMap,
};

/// Simple and handy btrfs snapshoting tool.
#[derive(Parser, Debug, Default, Serialize, Deserialize, Clone)]
#[clap(author, version, about, long_about = None, arg_required_else_help = true)]
pub struct Args {
    /// Path to configuration file.
    #[clap(short = 'c', long = "config")]
    pub config_file: Option<String>,
    /// Directory where snapshots should be saved.
    #[clap(long = "to", requires_all = &["create_snapshot"], default_value = "")]
    pub dest_dir: String,
    /// Directory from where snapshots should be created. It can also be used to specify the directory where a snapshot will be restored.
    #[clap(long = "from", requires_all = &["create_snapshot"], default_value = "")]
    pub source_dir: String,
    /// Snapshot id or name to work with.
    #[clap(long = "id", default_value = "")]
    pub snapshot_id: String,
    /// Path to the SQLite database file.
    #[clap(
        short = 'd',
        long = "dfile",
        env = "RUSNAPSHOT_DB_FILE",
        default_value = "/.rusnapshot/rusnapshot.db"
    )]
    pub database_file: String,
    /// Prefix for the snapshot name.
    #[clap(short = 'p', long = "prefix", default_value = "rusnapshot")]
    pub snapshot_prefix: String,
    /// Used to specify a differentiator between snapshots with the same prefix.
    #[clap(long = "kind", default_value = "rusnapshot")]
    pub snapshot_kind: String,
    /// Keep only the last X items.
    #[clap(short = 'k', long = "keep", default_value = "10")]
    pub keep_only: usize,
    /// Time in milliseconds until SQLite can return a timeout. Do not touch if you don't know what you are doing.
    #[clap(long = "timeout", default_value = "5000")]
    pub timeout: usize,
    /// Create a read-only/ro snapshot.
    #[clap(long = "create", conflicts_with_all = &["restore_snapshot", "delete_snapshot", "list_snapshots", "clean_snapshots"])]
    pub create_snapshot: bool,
    /// Enable snapshots cleaning, will keep only the last X snapshots specified with -k/--keep.
    /// This option requires: -k/--keep, -p/--prefix and --kind via command line or configuration file.
    #[clap(long = "clean")]
    pub clean_snapshots: bool,
    /// Delete a snapshot.
    #[clap(long = "del", requires_all = &["snapshot_id"])]
    pub delete_snapshot: bool,
    /// Restore a specific snapshot.
    #[clap(short = 'r', long = "restore", requires_all = &["snapshot_id"])]
    pub restore_snapshot: bool,
    /// List the snapshots tracked in the database.
    #[clap(short = 'l', long = "list")]
    pub list_snapshots: bool,
    /// Create read-write/rw snapshots.
    #[clap(short = 'w', long = "rw")]
    pub read_write: bool,
}

impl Args {
    /// Get the database connection string.
    #[must_use]
    pub fn database_connection(&self) -> Connection {
        match sqlite::open(&self.database_file) {
            Ok(mut connection) => {
                connection
                    .set_busy_timeout(self.timeout)
                    .expect("Failed to set database timeout");

                connection
            }
            Err(e) => {
                eprintln!("Error while trying to stablish the database connection. Error: {e}");
                std::process::exit(1)
            }
        }
    }

    /// Initialize the database and directory structure.
    pub fn init(&self, extra_args: &ExtraArgs) -> Result<()> {
        if !std::path::Path::new(&self.dest_dir).exists() {
            println!("Setting up the directory structure.");
            operations::setup_directory_structure(self)?;
        }
        database::setup_initial_database(&extra_args.database_connection)?;

        Ok(())
    }

    pub fn check_for_source_and_dest_dir(&mut self) {
        if self.source_dir.is_empty() || self.dest_dir.is_empty() {
            eprintln!("Specify both source and destination directories before taking a snapshot.");
            std::process::exit(1);
        }
    }

    /// Deserialize the configuration file.
    pub fn from_config_file(&mut self) -> Result<()> {
        let config_file = std::fs::read_to_string(self.config_file.as_deref().unwrap())?;
        let config: BTreeMap<String, toml::Value> = toml::from_str(&config_file)?;

        if let Some(dest_dir) = config.get("dest_dir") {
            self.dest_dir = dest_dir
                .as_str()
                .expect("Failed to parse dest_dir")
                .to_string();
        }
        if let Some(source_dir) = config.get("source_dir") {
            self.source_dir = source_dir
                .as_str()
                .expect("Failed to parse source_dir")
                .to_string();
        }
        if let Some(snapshot_prefix) = config.get("snapshot_prefix") {
            self.snapshot_prefix = snapshot_prefix
                .as_str()
                .expect("Failed to parse snapshot_prefix")
                .to_string();
        }
        if let Some(snapshot_kind) = config.get("snapshot_kind") {
            self.snapshot_kind = snapshot_kind
                .as_str()
                .expect("Failed to parse snapshot_kind")
                .to_string();
        }
        if let Some(database_file) = config.get("database_file") {
            self.database_file = database_file
                .as_str()
                .expect("Failed to parse database_file")
                .to_string();
        }
        if let Some(keep_only) = config.get("keep_only") {
            self.keep_only = keep_only
                .to_string()
                .parse()
                .expect("Failed to parse keep_only, make sure it's a number");
        }
        if let Some(timeout) = config.get("timeout") {
            self.timeout = timeout
                .to_string()
                .parse()
                .expect("Failed to parse timeout, make sure it's a number");
        }

        Ok(())
    }
}
