use {
    crate::{database, errors::*, operations, structs::Args},
    chrono::Utc,
    md5,
    prettytable::{cell, row, Table},
};

pub fn manage_creation(args: &mut Args) -> Result<()> {
    let connection = sqlite::open(&args.database_file)?;
    connection.execute("PRAGMA journal_mode=WAL")?;
    args.snapshot_name = format!(
        "{}-{}",
        args.snapshot_prefix,
        Utc::now().format("%Y-%m-%d-%H-%M-%S")
    );
    args.snapshot_id = format!("{:?}", md5::compute(&args.snapshot_name));
    if operations::take_snapshot(args) {
        database::commit_to_database(&connection, args)?
    }
    drop(connection);

    Ok(())
}

pub fn manage_deletion(args: &mut Args) -> Result<()> {
    let connection = sqlite::open(&args.database_file)?;
    connection.execute("PRAGMA journal_mode=WAL")?;
    let snapshot_data = database::return_snapshot_data(&connection, args)?;
    if !snapshot_data.snap_id.is_empty() {
        args.snapshot_name = snapshot_data.destination + &snapshot_data.name;
    } else {
        eprintln!(
            "Snapshot ID {} does not returned any data. Please double check the ID.",
            args.snapshot_id
        )
    }
    if !args.snapshot_name.is_empty() && operations::del_snapshot(args) {
        database::delete_from_database(&connection, args)?
    }
    drop(connection);

    Ok(())
}

pub fn manage_restoring(args: &mut Args) -> Result<()> {
    let connection = sqlite::open(&args.database_file)?;
    connection.execute("PRAGMA journal_mode=WAL")?;
    let snapshot_data = database::return_snapshot_data(&connection, args)?;
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
    drop(connection);

    Ok(())
}

pub fn manage_listing(args: &mut Args) -> Result<()> {
    let connection = sqlite::open(&args.database_file)?;
    connection.execute("PRAGMA journal_mode=WAL")?;
    let snaps_data = database::return_all_data(&connection)?;

    let mut table = Table::new();
    table.set_titles(row![
        bcFg => "NAME",
       "ID",
       "SOURCE DIR",
       "DESTINATION DIR",
       "DATE"
    ]);

    for data in &snaps_data {
        table.add_row(row![ d =>
            data.name,
            data.snap_id,
            data.source,
            data.destination,
            data.date,
        ]);
    }
    table.printstd();
    drop(connection);

    Ok(())
}

pub fn keep_only_x(args: &mut Args) -> Result<()> {
    let connection = sqlite::open(&args.database_file)?;
    connection.execute("PRAGMA journal_mode=WAL")?;
    let snaps_data = database::return_only_x_items(&connection, args)?;
    for data in &snaps_data {
        args.snapshot_name = data.destination.clone() + &data.name;
        args.snapshot_id = data.snap_id.clone();
        manage_deletion(args)?
    }

    Ok(())
}
