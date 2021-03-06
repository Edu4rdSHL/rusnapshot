use {
    crate::{database, errors::*, operations, structs::Args},
    chrono::Utc,
    md5,
    prettytable::{cell, row, Table},
};

pub fn manage_creation(args: &mut Args) -> Result<()> {
    args.snapshot_name = format!(
        "{}-{}",
        args.snapshot_prefix,
        Utc::now().format("%Y-%m-%d-%H-%M-%S-%6f")
    );
    args.snapshot_id = format!("{:?}", md5::compute(&args.snapshot_name));
    if operations::take_snapshot(args) {
        database::commit_to_database(&args.database_connection, args)?
    }

    Ok(())
}

pub fn manage_deletion(args: &mut Args) -> Result<()> {
    let snapshot_data = database::return_snapshot_data(&args.database_connection, args)?;
    if !snapshot_data.snap_id.is_empty() {
        args.snapshot_name = snapshot_data.destination + &snapshot_data.name;
    } else {
        eprintln!(
            "Snapshot ID {} does not returned any data. Please double check the ID.",
            args.snapshot_id
        )
    }
    if !args.snapshot_name.is_empty() && operations::del_snapshot(args) {
        database::delete_from_database(&args.database_connection, args)?
    }

    Ok(())
}

pub fn manage_restoring(args: &mut Args) -> Result<()> {
    let snapshot_data = database::return_snapshot_data(&args.database_connection, args)?;
    if !snapshot_data.snap_id.is_empty() {
        args.snapshot_name = snapshot_data.destination + &snapshot_data.name;
        if args.source_dir.is_empty() {
            args.source_dir = snapshot_data.source;
        }
    } else {
        eprintln!(
            "Snapshot ID {} does not returned any data. Please double check the ID.",
            args.snapshot_id
        )
    }
    if !args.snapshot_name.is_empty() && operations::restore_snapshot(args) {
        println!(
            "The snapshot with ID {} was successfully restored to {}",
            args.snapshot_id, args.source_dir
        )
    }

    Ok(())
}

pub fn manage_listing(args: &mut Args) -> Result<()> {
    let snaps_data = database::return_all_data(&args.database_connection)?;

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

pub fn keep_only_x(args: &mut Args) -> Result<()> {
    let snaps_data = database::return_only_x_items(&args.database_connection, args)?;
    for data in &snaps_data {
        args.snapshot_name = data.destination.clone() + &data.name;
        args.snapshot_id = data.snap_id.clone();
        manage_deletion(args)?
    }

    Ok(())
}
