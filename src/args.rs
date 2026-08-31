use {
    crate::{replication::Replication, utils},
    anyhow::{Context, Result, bail},
    clap::{ArgMatches, CommandFactory, FromArgMatches, Parser, parser::ValueSource},
    serde::Deserialize,
};

/// Simple and handy btrfs snapshoting tool.
#[derive(Parser, Debug, Default, Clone)]
#[command(author, version, about, long_about = None, arg_required_else_help = true)]
pub struct Args {
    /// Path to configuration file. Options given on the command line take precedence over it.
    #[arg(short = 'c', long = "config")]
    pub config_file: Option<String>,
    /// Directory where snapshots should be saved. With --restore, directory where the snapshot will be restored (must not exist).
    #[arg(long = "to", default_value = "", conflicts_with_all = ["delete_snapshot", "clean_snapshots", "send_snapshots"])]
    pub dest_dir: String,
    /// Directory (subvolume) from where snapshots should be created.
    #[arg(long = "from", default_value = "", conflicts_with_all = ["restore_snapshot", "delete_snapshot", "clean_snapshots", "send_snapshots"])]
    pub source_dir: String,
    /// Snapshot id or name to work with.
    #[arg(long = "id", default_value = "")]
    pub snapshot_id: String,
    /// Path to the `SQLite` database file.
    #[arg(
        short = 'd',
        long = "dfile",
        env = "RUSNAPSHOT_DB_FILE",
        default_value = "/.snapshots/rustnapshot.sqlite"
    )]
    pub database_file: String,
    /// Prefix for the snapshot name.
    #[arg(short = 'p', long = "prefix", default_value = "rusnapshot")]
    pub snapshot_prefix: String,
    /// Used to specify a differentiator between snapshots with the same prefix.
    #[arg(long = "kind", default_value = "rusnapshot")]
    pub snapshot_kind: String,
    /// Keep only the last X items.
    #[arg(short = 'k', long = "keep", default_value = "3")]
    pub keep_only: usize,
    /// Time in milliseconds until `SQLite` can return a timeout. Do not touch if you don't know what you are doing.
    #[arg(long = "timeout", default_value = "10000")]
    pub timeout: usize,
    /// Create a read-only/ro snapshot.
    #[arg(
        long = "create",
        group = "operation",
        conflicts_with = "list_snapshots"
    )]
    pub create_snapshot: bool,
    /// Enable snapshots cleaning, will keep only the last X snapshots specified with -k/--keep.
    /// Only the snapshots whose name starts with -p/--prefix and that match --kind, -m/--machine and the ro/rw mode (see --rw) are considered.
    #[arg(long = "clean", group = "operation")]
    pub clean_snapshots: bool,
    /// Delete a snapshot.
    #[arg(long = "del", group = "operation", requires = "snapshot_id")]
    pub delete_snapshot: bool,
    /// Restore a specific snapshot to its original source directory, or to --to if given. The target must not exist.
    #[arg(
        short = 'r',
        long = "restore",
        group = "operation",
        requires = "snapshot_id"
    )]
    pub restore_snapshot: bool,
    /// List the snapshots tracked in the database.
    #[arg(short = 'l', long = "list")]
    pub list_snapshots: bool,
    /// Create read-write/rw snapshots. With --clean, work on rw snapshots instead of ro ones.
    #[arg(short = 'w', long = "rw")]
    pub read_write: bool,
    /// Machine name to be used in the metadata and to select the snapshots to clean. Defaults to the hostname.
    #[arg(short, long, default_value = "")]
    pub machine: String,
    /// Replicate the read-only snapshots of this prefix and machine to the replication targets with btrfs send/receive.
    /// Targets come from the replicate sections of the configuration file or from --target.
    #[arg(long = "send", group = "operation")]
    pub send_snapshots: bool,
    /// Replication target for --send, instead of the configuration file ones: an absolute path or ssh://user@host:port/path (user and port are optional).
    #[arg(long = "target", requires = "send_snapshots", conflicts_with_all = ["create_snapshot", "delete_snapshot", "restore_snapshot", "clean_snapshots"])]
    pub target: Option<String>,
    /// With --target, path inside the snapshot to leave out of the replicas, relative to the snapshot root. Repeatable.
    #[arg(long = "exclude", requires = "target", value_name = "PATH")]
    pub exclude: Vec<String>,

    /// Restore from a replica instead of from the local snapshot. The target is taken from the
    /// database; give one (an absolute path or ssh://user@host:port/path) to pick between several.
    #[arg(
        long = "from-replica",
        value_name = "TARGET",
        num_args = 0..=1,
        default_missing_value = "",
        requires = "restore_snapshot",
        conflicts_with_all = ["create_snapshot", "delete_snapshot", "clean_snapshots", "send_snapshots"]
    )]
    pub from_replica: Option<String>,
    /// Show what would be created, deleted, restored or sent without doing it.
    #[arg(long = "dry-run")]
    pub dry_run: bool,
    /// Replication targets, from the configuration file or --target.
    #[arg(skip)]
    pub replicate: Vec<ReplicateConfig>,
}

impl Args {
    /// Parse the command line and, when `-c/--config` is given, fill from the file every option
    /// that was not explicitly set on the command line or through an environment variable.
    ///
    /// # Errors
    ///
    /// Fails if the configuration file can't be read or parsed.
    pub fn parse_with_config() -> Result<Self> {
        let matches = Self::command().get_matches();
        Self::from_matches(&matches)
    }

    /// Same as [`Self::parse_with_config`] but from already parsed matches.
    ///
    /// # Errors
    ///
    /// Fails if the matches can't be converted or the configuration file can't be read or parsed.
    pub fn from_matches(matches: &ArgMatches) -> Result<Self> {
        let mut args = Self::from_arg_matches(matches)?;
        if let Some(path) = args.config_file.clone() {
            let config = Config::from_file(&path)?;
            // Options that only exist in the file (such as `replicate`) have no clap id.
            config.merge_into(&mut args, |id| {
                matches.try_contains_id(id).is_ok()
                    && matches!(
                        matches.value_source(id),
                        Some(ValueSource::CommandLine | ValueSource::EnvVariable)
                    )
            });
        }
        if let Some(target) = &args.target {
            args.replicate = vec![ReplicateConfig {
                target: target.clone(),
                keep: None,
                ssh_options: Vec::new(),
                exclude: args.exclude.clone(),
            }];
        }

        Ok(args)
    }

    /// Make the source and destination directories absolute with a trailing slash, fill the
    /// machine name from the hostname when empty and validate the snapshot prefix.
    ///
    /// # Errors
    ///
    /// Fails if a path can't be resolved, the hostname can't be read or the prefix is invalid.
    pub fn normalize(&mut self) -> Result<()> {
        if !self.source_dir.is_empty() {
            self.source_dir = utils::normalize_dir(&self.source_dir)?;
        }
        if !self.dest_dir.is_empty() {
            self.dest_dir = utils::normalize_dir(&self.dest_dir)?;
        }
        if self.machine.is_empty() {
            self.machine = utils::machine_name()?;
        }
        if self.snapshot_prefix.is_empty() || self.snapshot_prefix.contains('/') {
            bail!("the snapshot prefix must not be empty or contain '/'");
        }
        if self.database_file.is_empty() {
            bail!("the database file path (-d/--dfile) must not be empty");
        }
        for config in &self.replicate {
            Replication::from_config(config)?;
        }

        Ok(())
    }

    /// Both directories are needed to take a snapshot.
    ///
    /// # Errors
    ///
    /// Fails if the source or the destination directory is missing.
    pub fn check_creation_requirements(&self) -> Result<()> {
        if self.source_dir.is_empty() || self.dest_dir.is_empty() {
            bail!(
                "specify both the source (--from) and the destination (--to) directories before taking a snapshot"
            );
        }

        Ok(())
    }
}

/// Options accepted in the TOML configuration file. All of them are optional.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
pub struct Config {
    pub dest_dir: Option<String>,
    pub source_dir: Option<String>,
    pub snapshot_prefix: Option<String>,
    pub snapshot_kind: Option<String>,
    pub database_file: Option<String>,
    pub keep_only: Option<usize>,
    pub timeout: Option<usize>,
    pub machine: Option<String>,
    pub replicate: Option<Vec<ReplicateConfig>>,
}

/// One `[[replicate]]` section: where to send the snapshots with `--send`.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
pub struct ReplicateConfig {
    /// Absolute path or `ssh://[user@]host[:port]/path`.
    pub target: String,
    /// Replicas to keep per kind at the target. Unset means never delete anything there.
    pub keep: Option<usize>,
    /// Extra `ssh` options, for example `["-i", "/root/.ssh/backup_key"]`.
    #[serde(default)]
    pub ssh_options: Vec<String>,
    /// Paths inside the snapshot to leave out of the replicas, relative to the snapshot root.
    #[serde(default)]
    pub exclude: Vec<String>,
}

impl Config {
    pub const KNOWN_KEYS: [&'static str; 9] = [
        "dest_dir",
        "source_dir",
        "snapshot_prefix",
        "snapshot_kind",
        "database_file",
        "keep_only",
        "timeout",
        "machine",
        "replicate",
    ];

    /// Read and parse a configuration file.
    ///
    /// # Errors
    ///
    /// Fails if the file can't be read or is not valid.
    pub fn from_file(path: &str) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read the configuration file {path}"))?;
        Self::parse(&content).with_context(|| format!("invalid configuration file {path}"))
    }

    /// Parse the TOML content. Unknown keys are reported on stderr and ignored.
    ///
    /// # Errors
    ///
    /// Fails if the content is not valid TOML or a value has the wrong type.
    pub fn parse(content: &str) -> Result<Self> {
        let table: toml::Table = toml::from_str(content)?;
        for key in table.keys() {
            if !Self::KNOWN_KEYS.contains(&key.as_str()) {
                eprintln!(
                    "Warning: unknown option '{key}' in the configuration file, ignoring it."
                );
            }
        }

        Ok(toml::from_str(content)?)
    }

    /// Copy the values present in the file into `args`, except for the options where
    /// `is_explicit(<field name>)` is true (the user already set them on the command line).
    pub fn merge_into(self, args: &mut Args, is_explicit: impl Fn(&str) -> bool) {
        macro_rules! apply {
            ($field:ident) => {
                if let Some(value) = self.$field {
                    if !is_explicit(stringify!($field)) {
                        args.$field = value;
                    }
                }
            };
        }
        apply!(dest_dir);
        apply!(source_dir);
        apply!(snapshot_prefix);
        apply!(snapshot_kind);
        apply!(database_file);
        apply!(keep_only);
        apply!(timeout);
        apply!(machine);
        apply!(replicate);
    }
}

#[cfg(test)]
mod tests {
    use {
        super::{Args, Config, ReplicateConfig},
        clap::CommandFactory,
    };

    const FULL: &str = r#"
dest_dir = "/.snapshots"
source_dir = "/"
database_file = "/.snapshots/rusnapshot.db"
snapshot_prefix = "root"
snapshot_kind = "weekly"
keep_only = 5
timeout = 2000
machine = "Oribos"

[[replicate]]
target = "ssh://nas/srv/backups"
keep = 7
"#;

    #[test]
    fn parse_typed_values() {
        let config = Config::parse(FULL).unwrap();
        assert_eq!(config.dest_dir.as_deref(), Some("/.snapshots"));
        assert_eq!(config.source_dir.as_deref(), Some("/"));
        assert_eq!(
            config.database_file.as_deref(),
            Some("/.snapshots/rusnapshot.db")
        );
        assert_eq!(config.snapshot_prefix.as_deref(), Some("root"));
        assert_eq!(config.snapshot_kind.as_deref(), Some("weekly"));
        assert_eq!(config.keep_only, Some(5));
        assert_eq!(config.timeout, Some(2000));
        // The value must not carry the TOML quotes.
        assert_eq!(config.machine.as_deref(), Some("Oribos"));
    }

    #[test]
    fn parse_replicate_sections() {
        let config = Config::parse(
            r#"
snapshot_prefix = "root"

[[replicate]]
target = "ssh://backup@nas:2222/srv/backups/behemoth"
keep = 30
ssh_options = ["-i", "/root/.ssh/backup_key"]
exclude = ["edu4rdshl/.cache", "edu4rdshl/Games"]

[[replicate]]
target = "/mnt/usb/backups"
"#,
        )
        .unwrap();
        let targets = config.replicate.clone().unwrap();
        assert_eq!(targets.len(), 2);
        assert_eq!(
            targets[0].target,
            "ssh://backup@nas:2222/srv/backups/behemoth"
        );
        assert_eq!(targets[0].keep, Some(30));
        assert_eq!(targets[0].ssh_options, ["-i", "/root/.ssh/backup_key"]);
        assert_eq!(targets[0].exclude, ["edu4rdshl/.cache", "edu4rdshl/Games"]);
        assert!(targets[1].exclude.is_empty());
        assert_eq!(targets[1].target, "/mnt/usb/backups");
        assert_eq!(targets[1].keep, None);
        assert!(targets[1].ssh_options.is_empty());

        let mut args = Args::default();
        config.merge_into(&mut args, |_| false);
        assert_eq!(args.replicate.len(), 2);

        assert!(
            Config::parse(
                "[[replicate]]
keep = 3
"
            )
            .is_err(),
            "target is mandatory"
        );
        assert!(
            Config::parse(
                "[[replicate]]
target = 3
"
            )
            .is_err()
        );
    }

    #[test]
    fn normalize_validates_replication_targets() {
        let mut args = Args {
            snapshot_prefix: "root".into(),
            database_file: "/db".into(),
            machine: "m".into(),
            replicate: vec![ReplicateConfig {
                target: "nas:/srv".into(),
                ..ReplicateConfig::default()
            }],
            ..Args::default()
        };
        assert!(args.normalize().is_err());
        args.replicate[0].target = "ssh://nas/srv".into();
        args.replicate[0].keep = Some(0);
        assert!(args.normalize().is_err());
        args.replicate[0].keep = Some(1);
        args.normalize().unwrap();
    }

    #[test]
    fn parse_partial_and_unknown_keys() {
        let config = Config::parse("snapshot_prefix = \"home\"\nsomething_else = 1\n").unwrap();
        assert_eq!(config.snapshot_prefix.as_deref(), Some("home"));
        assert_eq!(config.keep_only, None);
        assert_eq!(Config::parse("").unwrap(), Config::default());
    }

    #[test]
    fn parse_rejects_wrong_types() {
        let err = Config::parse("keep_only = \"3\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("keep_only"), "{err}");
        assert!(Config::parse("machine = 3\n").is_err());
        assert!(Config::parse("this is not toml").is_err());
    }

    #[test]
    fn merge_fills_only_non_explicit_options() {
        let config = Config::parse(FULL).unwrap();
        let mut args = Args {
            snapshot_kind: "daily".into(),
            keep_only: 3,
            machine: "cli".into(),
            ..Args::default()
        };
        config.merge_into(&mut args, |id| matches!(id, "snapshot_kind" | "machine"));

        assert_eq!(args.snapshot_kind, "daily");
        assert_eq!(args.machine, "cli");
        assert_eq!(args.keep_only, 5);
        assert_eq!(args.timeout, 2000);
        assert_eq!(args.dest_dir, "/.snapshots");
        assert_eq!(args.source_dir, "/");
        assert_eq!(args.snapshot_prefix, "root");
        assert_eq!(args.database_file, "/.snapshots/rusnapshot.db");
    }

    #[test]
    fn merge_keeps_args_when_file_lacks_the_option() {
        let config = Config::parse("snapshot_prefix = \"home\"\n").unwrap();
        let mut args = Args {
            snapshot_kind: "daily".into(),
            ..Args::default()
        };
        config.merge_into(&mut args, |_| false);
        assert_eq!(args.snapshot_prefix, "home");
        assert_eq!(args.snapshot_kind, "daily");
    }

    #[test]
    fn normalize_validates_prefix_and_database_file() {
        let mut args = Args {
            snapshot_prefix: String::new(),
            database_file: "/db".into(),
            machine: "m".into(),
            ..Args::default()
        };
        assert!(args.normalize().is_err());
        args.snapshot_prefix = "a/b".into();
        assert!(args.normalize().is_err());
        args.snapshot_prefix = "root".into();
        args.database_file = String::new();
        assert!(args.normalize().is_err());
        args.database_file = "/db".into();
        args.source_dir = "/home".into();
        args.normalize().unwrap();
        assert_eq!(args.source_dir, "/home/");
        assert_eq!(args.dest_dir, "");
    }

    /// The precedence logic relies on the clap argument ids being the field names, for every
    /// option that can also come from the configuration file.
    #[test]
    fn from_matches_applies_precedence_for_every_option() {
        let dir = std::env::temp_dir().join(format!("rusnapshot-args-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("config.toml");
        std::fs::write(&file, FULL).unwrap();
        let file = file.to_str().unwrap();

        // Nothing explicit: everything comes from the file.
        let matches = Args::command()
            .try_get_matches_from(["rusnapshot", "-c", file, "--list"])
            .unwrap();
        let args = Args::from_matches(&matches).unwrap();
        assert_eq!(args.dest_dir, "/.snapshots");
        assert_eq!(args.source_dir, "/");
        assert_eq!(args.snapshot_prefix, "root");
        assert_eq!(args.snapshot_kind, "weekly");
        assert_eq!(args.keep_only, 5);
        assert_eq!(args.timeout, 2000);
        assert_eq!(args.machine, "Oribos");
        assert_eq!(args.replicate.len(), 1);
        assert_eq!(args.replicate[0].target, "ssh://nas/srv/backups");
        assert_eq!(args.replicate[0].keep, Some(7));
        // An environment variable counts as explicit, so only check the file value without it.
        if std::env::var_os("RUSNAPSHOT_DB_FILE").is_none() {
            assert_eq!(args.database_file, "/.snapshots/rusnapshot.db");
        }

        // Everything given on the command line wins over the file.
        let matches = Args::command()
            .try_get_matches_from([
                "rusnapshot",
                "-c",
                file,
                "--create",
                "--to",
                "/d",
                "--from",
                "/s",
                "-p",
                "p",
                "--kind",
                "k",
                "-d",
                "/db",
                "-k",
                "9",
                "--timeout",
                "1",
                "-m",
                "m",
            ])
            .unwrap();
        let args = Args::from_matches(&matches).unwrap();
        assert_eq!(args.dest_dir, "/d");
        assert_eq!(args.source_dir, "/s");
        assert_eq!(args.snapshot_prefix, "p");
        assert_eq!(args.snapshot_kind, "k");
        assert_eq!(args.database_file, "/db");
        assert_eq!(args.keep_only, 9);
        assert_eq!(args.timeout, 1);
        assert_eq!(args.machine, "m");
        assert_eq!(args.replicate.len(), 1, "the file targets stay");

        // --target replaces the file targets.
        let matches = Args::command()
            .try_get_matches_from(["rusnapshot", "-c", file, "--send", "--target", "/mnt/usb"])
            .unwrap();
        let args = Args::from_matches(&matches).unwrap();
        assert_eq!(args.replicate.len(), 1);
        assert_eq!(args.replicate[0].target, "/mnt/usb");
        assert_eq!(args.replicate[0].keep, None);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn creation_requires_both_directories() {
        let mut args = Args::default();
        assert!(args.check_creation_requirements().is_err());
        args.source_dir = "/".into();
        assert!(args.check_creation_requirements().is_err());
        args.dest_dir = "/.snapshots/".into();
        args.check_creation_requirements().unwrap();
    }
}
