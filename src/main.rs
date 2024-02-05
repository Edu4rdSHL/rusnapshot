use std::path::Path;

use anyhow::Result;
use chrono::Utc;
use clap::Parser;
use rusnapshot::{args, controller, structs::ExtraArgs};

fn try_run() -> Result<()> {
    let mut arguments = args::Args::parse();

    if arguments.config_file.is_some() {
        arguments.from_config_file()?;
    }

    let mut extra_args = ExtraArgs {
        snapshot_name: format!(
            "{}-{}",
            arguments.snapshot_prefix,
            Utc::now().format("%Y-%m-%d-%H-%M-%S-%6f")
        ),
        database_connection: arguments.database_connection(),
    };

    arguments.init(&extra_args)?;

    if arguments.create_snapshot {
        // It's required to have a trailing slash for the source and destination directories.
        // Otherwise, when we retrieve the snapshot data from the database, we won't be able to
        // restore/delete the snapshot because the source/destination paths won't match.
        if !arguments.source_dir.is_empty() && !arguments.source_dir.ends_with('/') {
            arguments.source_dir += "/";
        }
        if !arguments.dest_dir.is_empty() && !arguments.dest_dir.ends_with('/') {
            arguments.dest_dir += "/";
        }

        // Now we need to make sure that the paths for source and destination are full paths.
        // If they are not, we will use the current working directory to build the full path.
        if !Path::new(&arguments.source_dir).is_absolute() {
            arguments.source_dir = std::env::current_dir()?
                .join(&arguments.source_dir)
                .to_str()
                .expect("Failed to get the source directory")
                .to_string();
        }

        if !Path::new(&arguments.dest_dir).is_absolute() {
            arguments.dest_dir = std::env::current_dir()?
                .join(&arguments.dest_dir)
                .to_str()
                .expect("Failed to get the destination directory")
                .to_string();
        }
    }

    if arguments.create_snapshot {
        controller::manage_creation(&mut arguments, &extra_args)?;
    }
    if arguments.delete_snapshot {
        controller::manage_deletion(&arguments, &mut extra_args)?;
    }
    if arguments.list_snapshots {
        controller::manage_listing(&extra_args.database_connection)?;
    }
    if arguments.clean_snapshots {
        controller::keep_only_x(&mut arguments, &mut extra_args)?;
    }
    if arguments.restore_snapshot {
        controller::manage_restoring(&mut arguments, &mut extra_args)?;
    }
    Ok(())
}

fn main() {
    if let Err(err) = try_run() {
        eprintln!("\nError: {err}");
        std::process::exit(1);
    }
}
