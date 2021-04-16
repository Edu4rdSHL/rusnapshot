use {
    rusnapshot::{args, controller, database, errors::*},
    sqlite,
};

fn run() -> Result<()> {
    let mut arguments = args::get_args();
    database::test_database(&arguments.database_file)?;
    database::setup_initial_database(&sqlite::open(&arguments.database_file)?)?;

    if arguments.create_snapshot {
        controller::manage_creation(&mut arguments)?
    }
    if arguments.delete_snapshot {
        controller::manage_deletion(&mut arguments)?
    }
    if arguments.list_snapshots {
        controller::manage_listing(&mut arguments)?
    }
    if arguments.clean_snapshots {
        controller::keep_only_x(&mut arguments)?
    }
    if arguments.restore_snapshot {
        controller::manage_restoring(&mut arguments)?
    }
    Ok(())
}

fn main() {
    if let Err(err) = run() {
        eprintln!("\nError: {}", err);
        for cause in err.iter_chain().skip(1) {
            eprintln!("Error description: {}", cause);
        }
        std::process::exit(1);
    }
}
