use {
    crate::{args::Args, database, operations, structs::ExtraArgs},
    anyhow::Result,
    md5,
    prettytable::{row, Table},
    sqlite::Connection,
};

pub fn manage_creation(args: &mut Args, extra_args: &ExtraArgs) -> Result<()> {
    args.snapshot_id = format!("{:?}", md5::compute(&extra_args.snapshot_name));
    if operations::take_snapshot(args, &extra_args.snapshot_name) {
        database::commit_to_database(args, extra_args)?;
    }

    Ok(())
}

pub fn manage_deletion(args: &Args, extra_args: &mut ExtraArgs) -> Result<()> {
    let snapshot_data = database::return_snapshot_data(&extra_args.database_connection, args)?;
    if snapshot_data.snap_id.is_empty() {
        eprintln!(
            "Snapshot ID {} does not returned any data. Please double check the ID.",
            args.snapshot_id
        );
        std::process::exit(1);
    } else {
        extra_args.snapshot_name = snapshot_data.destination + &snapshot_data.name;
    }
    if !extra_args.snapshot_name.is_empty() && operations::del_snapshot(&extra_args.snapshot_name) {
        database::delete_from_database(&extra_args.database_connection, args)?;
    }

    Ok(())
}

pub fn manage_restoring(args: &mut Args, extra_args: &mut ExtraArgs) -> Result<()> {
    let snapshot_data = database::return_snapshot_data(&extra_args.database_connection, args)?;
    if snapshot_data.snap_id.is_empty() {
        eprintln!(
            "Snapshot ID {} does not returned any data. Please double check the ID.",
            args.snapshot_id
        );
        std::process::exit(1);
    } else {
        extra_args.snapshot_name = snapshot_data.destination + &snapshot_data.name;
        if args.source_dir.is_empty() {
            args.source_dir = snapshot_data.source;
        }
    }
    println!("Restoring the snapshot with ID {}", args.snapshot_id);
    println!("Name of the snapshot: {}", extra_args.snapshot_name);
    println!("Source directory: {}", args.source_dir);

    if !extra_args.snapshot_name.is_empty()
        && operations::restore_snapshot(args, &extra_args.snapshot_name)
    {
        println!(
            "The snapshot with ID {} was successfully restored to {}",
            args.snapshot_id, args.source_dir
        );
    }

    Ok(())
}

pub fn manage_listing(database_connection: &Connection) -> Result<()> {
    let snaps_data = database::return_all_data(database_connection)?;

    let mut table = Table::new();
    table.set_titles(row![
        bcFg => "NAME",
       "ID",
       "KIND",
       "SOURCE DIR",
       "DESTINATION DIR",
       "RO/RW",
       "DATE"
    ]);

    for data in &snaps_data {
        table.add_row(row![ d =>
            data.name,
            data.snap_id,
            data.kind,
            data.source,
            data.destination,
            data.ro_rw,
            data.date,
        ]);
    }
    table.printstd();

    Ok(())
}

pub fn keep_only_x(args: &mut Args, extra_args: &mut ExtraArgs) -> Result<()> {
    let snaps_data = database::return_only_x_items(&extra_args.database_connection, args)?;

    for data in &snaps_data {
        extra_args.snapshot_name = data.destination.clone() + &data.name;
        args.snapshot_id = data.snap_id.clone();

        manage_deletion(args, extra_args)?;
    }

    Ok(())
}
