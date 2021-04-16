# rusnapshot
Simple and handy btrfs snapshoting tool. Supports scheduled snapshots and restoring. Backed with SQLite.

# Features

- Allows you to specify the origin and destination of snapshots at will of the user.
- Track snapshots using SQLite as backend database.
- You can create snapshots of the volumes you want simply by using different configuration templates.
- You can use the same database for everything.
- You can specify the prefix for each group of snapshots.
- Supports restoration of snapshots.
- Automatic snapshots cleanup.