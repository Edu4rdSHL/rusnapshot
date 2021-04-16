use {
    crate::{
        errors::*,
        structs::{Args, Database},
    },
    sqlite::{Connection, State},
};

pub fn test_database(connection_str: &str) -> Result<()> {
    let connection = sqlite::open(&connection_str)?;
    connection.execute("PRAGMA journal_mode=WAL")?;
    drop(connection);

    Ok(())
}

pub fn setup_initial_database(connection: &Connection) -> Result<()> {
    connection.execute("PRAGMA journal_mode=WAL")?;
    connection.execute("CREATE TABLE IF NOT EXISTS snapshots (name TEXT NOT NULL, snap_id TEXT NOT NULL, kind TEXT NOT NULL, source TEXT NOT NULL, destination TEXT NOT NULL, ro_rw TEXT NOT NULL, date TEXT DEFAULT CURRENT_TIMESTAMP, PRIMARY KEY(name, snap_id))")?;
    Ok(())
}

pub fn commit_to_database(connection: &Connection, args: &Args) -> Result<()> {
    let statement = format!(
        "INSERT INTO snapshots (name, snap_id, kind, source, destination, ro_rw) VALUES ('{}', '{}', '{}', '{}', '{}', '{}')",
        args.snapshot_name, args.snapshot_id, args.snapshot_kind, args.source_dir, args.dest_dir, args.snapshot_ro_rw
    );
    connection.execute(&statement)?;

    Ok(())
}

pub fn delete_from_database(connection: &Connection, args: &Args) -> Result<()> {
    let statement = format!(
        "DELETE FROM snapshots WHERE name = '{}' OR snap_id = '{}'",
        args.snapshot_id, args.snapshot_id
    );
    connection.execute(&statement)?;

    Ok(())
}

pub fn return_snapshot_data(connection: &Connection, args: &Args) -> Result<Database> {
    let mut statement = connection.prepare(format!(
        "SELECT * FROM snapshots WHERE name = '{}' OR snap_id = '{}'",
        args.snapshot_id, args.snapshot_id
    ))?;
    let mut snap_data = Database::default();

    while let State::Row = statement.next()? {
        snap_data.name = statement.read::<String>(0).unwrap();
        snap_data.snap_id = statement.read::<String>(1).unwrap();
        snap_data.kind = statement.read::<String>(2).unwrap();
        snap_data.source = statement.read::<String>(3).unwrap();
        snap_data.destination = statement.read::<String>(4).unwrap();
        snap_data.ro_rw = statement.read::<String>(5).unwrap();
        snap_data.date = statement.read::<String>(6).unwrap();
    }

    Ok(snap_data)
}

pub fn return_all_data(connection: &Connection) -> Result<Vec<Database>> {
    let mut snapshots_data: Vec<Database> = Vec::new();

    let mut statement = connection.prepare("SELECT * FROM snapshots ORDER BY date DESC")?;

    while let State::Row = statement.next()? {
        let mut db_struct = Database::default();

        db_struct.name = statement.read::<String>(0).unwrap();
        db_struct.snap_id = statement.read::<String>(1).unwrap();
        db_struct.kind = statement.read::<String>(2).unwrap();
        db_struct.source = statement.read::<String>(3).unwrap();
        db_struct.destination = statement.read::<String>(4).unwrap();
        db_struct.ro_rw = statement.read::<String>(5).unwrap();
        db_struct.date = statement.read::<String>(6).unwrap();

        snapshots_data.push(db_struct);
    }

    Ok(snapshots_data)
}

pub fn return_only_x_items(connection: &Connection, args: &Args) -> Result<Vec<Database>> {
    let mut snapshots_data: Vec<Database> = Vec::new();

    let mut statement = connection.prepare(&format!("SELECT name,snap_id,kind,source,destination,ro_rw,date FROM (SELECT row_number() over(ORDER BY date DESC) n,* from snapshots WHERE name like '{}%' AND kind = '{}' AND ro_rw = '{}') WHERE n > {}", args.snapshot_prefix, args.snapshot_kind, args.snapshot_ro_rw, args.keep_only))?;

    while let State::Row = statement.next()? {
        let mut db_struct = Database::default();

        db_struct.name = statement.read::<String>(0).unwrap();
        db_struct.snap_id = statement.read::<String>(1).unwrap();
        db_struct.kind = statement.read::<String>(2).unwrap();
        db_struct.source = statement.read::<String>(3).unwrap();
        db_struct.destination = statement.read::<String>(4).unwrap();
        db_struct.ro_rw = statement.read::<String>(5).unwrap();
        db_struct.date = statement.read::<String>(6).unwrap();

        snapshots_data.push(db_struct);
    }

    Ok(snapshots_data)
}
