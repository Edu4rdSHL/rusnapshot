use anyhow::Result;
use chrono::Utc;
use clap::Parser;
use rusnapshot::{args, controller, structs::ExtraArgs, utils};

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

    if arguments.create_snapshot {
        utils::check_creation_requirements(&mut arguments, &extra_args)?;
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
