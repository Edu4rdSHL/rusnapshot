use {
    crate::structs::Args,
    clap::{load_yaml, value_t, App},
    std::{collections::HashMap, path::Path},
};

pub fn get_args() -> Args {
    let yaml = load_yaml!("cli.yml");
    let matches = App::from_yaml(yaml)
        .version(clap::crate_version!())
        .get_matches();
    let settings: HashMap<String, String> =
        return_settings(&matches, &mut config::Config::default());
    Args {
        create_snapshot: matches.is_present("create-snapshot"),
        delete_snapshot: matches.is_present("delete-snapshot"),
        list_snapshots: matches.is_present("list-snapshots"),
        clean_snapshots: matches.is_present("clean-snapshots") || matches.is_present("keep-only"),
        restore_snapshot: matches.is_present("restore-snapshot"),
        dest_dir: if matches.is_present("dest-dir") {
            value_t!(matches, "dest-dir", String).unwrap_or_else(|_| String::new())
        } else {
            return_value_or_default(&settings, "dest_dir", String::new())
        },
        source_dir: if matches.is_present("source-dir") {
            value_t!(matches, "source-dir", String).unwrap_or_else(|_| String::new())
        } else {
            return_value_or_default(&settings, "source_dir", String::new())
        },
        database_file: if matches.is_present("database-file") {
            value_t!(matches, "database-file", String).unwrap_or_else(|_| String::new())
        } else {
            return_value_or_default(&settings, "database_file", String::new())
        },
        snapshot_name: String::new(),
        snapshot_id: value_t!(matches, "snapshot-id", String).unwrap_or_else(|_| String::new()),
        snapshot_prefix: if matches.is_present("snapshot-prefix") {
            value_t!(matches, "snapshot-prefix", String)
                .unwrap_or_else(|_| String::from("snapshot"))
        } else {
            return_value_or_default(&settings, "snapshot_prefix", String::from("snapshot"))
        },
        keep_only: if matches.is_present("keep-only") {
            value_t!(matches, "keep-only", usize).unwrap_or_else(|_| 0)
        } else {
            return_value_or_default(&settings, "keep_only", 0.to_string())
                .parse()
                .unwrap_or_default()
        },
    }
}

fn return_settings(
    matches: &clap::ArgMatches,
    settings: &mut config::Config,
) -> HashMap<String, String> {
    if matches.is_present("config-file") {
        match settings.merge(config::File::with_name(
            &value_t!(matches, "config-file", String).unwrap(),
        )) {
            Ok(settings) => match settings.merge(config::Environment::with_prefix("RUSNAPSHOT")) {
                Ok(settings) => settings
                    .clone()
                    .try_into::<HashMap<String, String>>()
                    .unwrap(),
                Err(e) => {
                    eprintln!("Error merging environment variables into settings: {}", e);
                    std::process::exit(1)
                }
            },
            Err(e) => {
                eprintln!("Error reading config file: {}", e);
                std::process::exit(1)
            }
        }
    } else if Path::new("rusnapshot.toml").exists()
        || Path::new("rusnapshot.json").exists()
        || Path::new("rusnapshot.hjson").exists()
        || Path::new("rusnapshot.ini").exists()
        || Path::new("rusnapshot.yml").exists()
    {
        match settings.merge(config::File::with_name("rusnapshot")) {
            Ok(settings) => match settings.merge(config::Environment::with_prefix("RUSNAPSHOT")) {
                Ok(settings) => settings
                    .clone()
                    .try_into::<HashMap<String, String>>()
                    .unwrap(),
                Err(e) => {
                    eprintln!("Error merging environment variables into settings: {}", e);
                    std::process::exit(1)
                }
            },
            Err(e) => {
                eprintln!("Error reading config file: {}", e);
                std::process::exit(1)
            }
        }
    } else {
        match settings.merge(config::Environment::with_prefix("RUSNAPSHOT")) {
            Ok(settings) => settings
                .clone()
                .try_into::<HashMap<String, String>>()
                .unwrap(),
            Err(e) => {
                eprintln!("Error merging environment variables into settings: {}", e);
                std::process::exit(1)
            }
        }
    }
}

fn return_value_or_default(
    settings: &HashMap<String, String>,
    value: &str,
    default_value: String,
) -> String {
    settings.get(value).unwrap_or(&default_value).to_string()
}
