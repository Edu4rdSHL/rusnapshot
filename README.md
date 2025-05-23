# Description
Simple and handy definitions-based snapshoting tool for BTRFS. Supports unattended snapshots, tracking, restoring, automatic cleanup and more. Backed with SQLite.

# Documentation

See the [docs folder](https://github.com/Edu4rdSHL/rusnapshot/blob/master/docs/).

# Features

**The main feature of Rustnapshot is the ability to create snapshots of BTRFS subvolumes using simple TOML definitions. It is designed to be simple and easy to use, while still providing powerful features for managing snapshots. See [the examples folder](https://github.com/Edu4rdSHL/rusnapshot/tree/master/examples) for more information.**

Among the features of Rustnapshot are:

- Allows you to specify the origin and destination of snapshots at will of the user.
- Track snapshots using SQLite as backend database.
- Easy setup using small templates instead of confusing long files.
- Ability to create snapshots of the volumes you want simply by using different configuration templates.
- Ability to create read-only or read-write snapshots.
- Ability to use the same SQLite database for everything.
- Ability to specify the prefix of the name for the snapshots for better identification.
- Ability to specify a `kind` identifier to differentiate them in the database. Useful if you plan to have hourly, weekly, monthly or more "kind" of snapshots of the same subvolume(s).
- Ability to specify the maximum number of snapshots to keep for automatic cleanup.
- Supports restoration of snapshots in the original directory or a specific one.
- Supports machine name identification for better tracking when using the same database in multiple machines.
- Nice CLI output to see the status and details of snapshots.

# Known limitations

Due to SQLite limitations to handle concurrent database operations, we need to make use of SQLite's [busy_timeout](https://www.sqlite.org/c3ref/busy_timeout.html), which was initially implemented in [ce44bf6](https://github.com/Edu4rdSHL/rusnapshot/commit/ce44bf679c73d221811ac775561916a8c5761243). Without it you will get the error: `Error: database is locked (code 5)` if you try to make concurrent snapshots exactly at the same time. The default timeout is 5 seconds which is more than enough time so that no problem occurs even in the most demanding scenarios.

In summary, `busy_timeout` is the maximum time that SQLite will retry the failed transaction. Considering that Rusnapshot's database operations are small and shouldn't take long, 5 seconds is a decent time. If you continue to get the mentioned error, try increasing `--timeout`, remember that the value must be in milliseconds.

# Screenshots

## List snapshots

![2021-04-23_11-11](https://user-images.githubusercontent.com/32582878/115900337-59bf1a00-a44f-11eb-8834-cbd52bbe9a6b.png)

## Create and restore snapshot

![2021-04-23_11-15](https://user-images.githubusercontent.com/32582878/115900363-63488200-a44f-11eb-8f62-9796d782c786.png)

## Delete snapshot

![2021-04-23_11-17](https://user-images.githubusercontent.com/32582878/115900425-79564280-a44f-11eb-960d-1a2a95a95f57.png)

