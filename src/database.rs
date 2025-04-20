use {
    crate::{
        args::Args,
        structs::{Database, ExtraArgs},
    },
    anyhow::Result,
    sqlite::{Connection, State, Statement},
};

pub fn setup_initial_database(connection: &Connection) -> Result<()> {
    connection.execute("CREATE TABLE IF NOT EXISTS snapshots (name TEXT NOT NULL, snap_id TEXT NOT NULL, kind TEXT NOT NULL, source TEXT NOT NULL, destination TEXT NOT NULL, machine TEXT NOT NULL, ro_rw TEXT NOT NULL, date TEXT DEFAULT CURRENT_TIMESTAMP, PRIMARY KEY(name, snap_id))")?;
    connection.execute("PRAGMA journal_mode=WAL")?;

    Ok(())
}

pub fn commit_to_database(args: &Args, extra_args: &ExtraArgs) -> Result<()> {
    let statement = format!(
        "INSERT INTO snapshots (name, snap_id, kind, source, destination, machine, ro_rw) VALUES ('{}', '{}', '{}', '{}', '{}', '{}', '{}')",
        &extra_args.snapshot_name, args.snapshot_id, args.snapshot_kind, args.source_dir, args.dest_dir, args.machine, args.read_write
    );
    extra_args.database_connection.execute(statement)?;

    Ok(())
}

pub fn delete_from_database(connection: &Connection, args: &Args) -> Result<()> {
    let statement = format!(
        "DELETE FROM snapshots WHERE name = '{}' OR snap_id = '{}'",
        args.snapshot_id, args.snapshot_id
    );
    connection.execute(statement)?;

    Ok(())
}

pub fn return_snapshot_data(connection: &Connection, args: &Args) -> Result<Database> {
    let mut statement = connection.prepare(format!(
        "SELECT * FROM snapshots WHERE name = '{}' OR snap_id = '{}'",
        args.snapshot_id, args.snapshot_id
    ))?;
    let mut snap_data = Database::default();

    while statement.next()? == State::Row {
        snap_data = populate_db_struct(&statement, snap_data)?;
    }

    Ok(snap_data)
}

pub fn return_all_data(connection: &Connection) -> Result<Vec<Database>> {
    let mut snapshots_data: Vec<Database> = Vec::new();

    let mut statement = connection
        .prepare("SELECT name,snap_id,kind,source,destination,machine,ro_rw,datetime(date, 'localtime') FROM snapshots ORDER BY date DESC")?;

    while statement.next()? == State::Row {
        let db_struct = Database::default();
        snapshots_data.push(populate_db_struct(&statement, db_struct)?);
    }

    Ok(snapshots_data)
}

pub fn return_only_x_items(connection: &Connection, args: &Args) -> Result<Vec<Database>> {
    let mut snapshots_data: Vec<Database> = Vec::new();

    let mut statement = connection.prepare(format!("SELECT name,snap_id,kind,source,destination,machine,ro_rw,date FROM (SELECT row_number() over(ORDER BY date DESC) n,* from snapshots WHERE name like '{}%' AND kind = '{}' AND ro_rw = '{}') WHERE n > {}", args.snapshot_prefix, args.snapshot_kind, args.read_write, args.keep_only))?;

    while statement.next()? == State::Row {
        let db_struct = Database::default();
        snapshots_data.push(populate_db_struct(&statement, db_struct)?);
    }

    Ok(snapshots_data)
}

fn populate_db_struct(row: &Statement, mut db_struct: Database) -> Result<Database> {
    // I'll improve this later.

    db_struct.name = row.read(0)?;
    db_struct.snap_id = row.read(1)?;
    db_struct.kind = row.read(2)?;
    db_struct.source = row.read(3)?;
    db_struct.destination = row.read(4)?;
    db_struct.machine = row.read(5)?;
    db_struct.ro_rw = row.read(6)?;
    db_struct.date = row.read(7)?;

    Ok(db_struct)
}
