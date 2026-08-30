use {
    anyhow::Result,
    rusnapshot::{args::Args, controller, database, operations},
};

fn try_run() -> Result<()> {
    let mut args = Args::parse_with_config()?;
    args.normalize()?;

    if args.create_snapshot {
        args.check_creation_requirements()?;
        operations::setup_directory_structure(&args.dest_dir, args.dry_run)?;
    }

    let connection = database::open(&args)?;

    if args.create_snapshot {
        controller::manage_creation(&args, &connection)?;
    }
    if args.delete_snapshot {
        controller::manage_deletion(&args, &connection)?;
    }
    if args.clean_snapshots {
        controller::keep_only_x(&args, &connection)?;
    }
    if args.restore_snapshot {
        controller::manage_restoring(&args, &connection)?;
    }
    // Listing goes last so that combined with --clean it shows the resulting state.
    if args.list_snapshots {
        controller::manage_listing(&connection)?;
    }

    Ok(())
}

fn main() {
    if let Err(err) = try_run() {
        eprintln!("Error: {err:#}");
        std::process::exit(1);
    }
}
