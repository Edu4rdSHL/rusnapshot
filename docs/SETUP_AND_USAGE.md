# Dependencies

- [btrfs-progs](https://github.com/kdave/btrfs-progs)
- [sqlite](https://www.sqlite.org/download.html) >= 3

# Installation

- Install [Rust](https://www.rust-lang.org/tools/install).
- Clone the repo.
- Run `cargo build --release`
- `cp target/release/rusnapshot /usr/bin/`

# Usage

- Create a snapshot using a [config file](https://github.com/Edu4rdSHL/rusnapshot/tree/master/examples/config-templates):

`sudo rusnapshot --config {{path/to/config.toml}} --create`

- List created snapshots:

`sudo rusnapshot -c {{path/to/config.toml}} --list`

- Delete a snapshot by ID or the name of the snapshot:

`sudo rusnapshot -c {{path/to/config.toml}} --del --id {{snapshot_id}}`

- Keep only the last 3 `daily` snapshots (see the notes about `--clean` below):

`sudo rusnapshot -c {{path/to/config.toml}} --clean --keep 3 --kind daily`

- See what `--clean` would delete without deleting anything:

`sudo rusnapshot -c {{path/to/config.toml}} --clean --keep 3 --kind daily --dry-run`

- Delete all `hourly` snapshots and list what is left:

`sudo rusnapshot -c {{path/to/config.toml}} --clean --keep 0 --kind hourly --list`

- Create a read-write snapshot:

`sudo rusnapshot -c {{path/to/config.toml}} --create --rw`

- Restore a snapshot to the directory it was taken from:

`sudo rusnapshot --id {{snapshot_id}} --restore`

- Restore a snapshot to a different directory:

`sudo rusnapshot --id {{snapshot_id}} --restore --to {{/path/to/restore}}`

# Notes

## Configuration file and precedence

- Every option in the [config example](https://github.com/Edu4rdSHL/rusnapshot/blob/master/examples/config-templates/config-all.toml) is optional. Options given on the command line take precedence over the configuration file, so `-c config.toml --kind daily` uses `daily` even if the file sets `snapshot_kind = "weekly"`.
- The database file can also be given with the `RUSNAPSHOT_DB_FILE` environment variable, which takes precedence over the configuration file but not over `-d/--dfile`.
- The `--kind` option can have any value and allow you to have different "kinds" of snapshots for the same directory, see the [services/timers examples](https://github.com/Edu4rdSHL/rusnapshot/tree/master/examples/services) for more info.
- The `--prefix` option is used to declare the first part of the snapshot name. Snapshots are named `<prefix>-<UTC date and time>`.
- The `-m/--machine` option defaults to the hostname. It is stored with every snapshot and used by `--clean` to select the snapshots to delete, so several machines can share a database without interfering with each other.

## Cleaning

`--clean` deletes every snapshot beyond the last `--keep` ones among the snapshots that:

- have a name starting with `--prefix` (a `prefix%` match: prefix `home` also covers snapshots named `home-data-...`, keep that in mind when choosing prefixes),
- have the same `--kind`,
- were created by the same `-m/--machine`,
- and have the same mode: read-only by default, read-write if `--rw` is given.

Use `--dry-run` to print the list of snapshots that would be deleted. When several operations are combined, `--list` runs last so it shows the resulting state.

If a snapshot recorded in the database no longer exists on disk (for example because it was deleted by hand), `--del` and `--clean` remove it from the database with a warning instead of failing.

## Restoring

Restores create a read-write snapshot of the stored snapshot at the target directory. The target must not exist, so to restore a subvolume in place move the current one out of the way first:

```
sudo mv /home /home.old
sudo rusnapshot --id {{snapshot_id}} --restore
```

Use `--to` to restore somewhere else. Restoring the root subvolume in place requires booting from another system or mounting the top-level subvolume, since `/` can't be moved while in use.

## Exit codes

`rusnapshot` exits with a non-zero status whenever a snapshot can't be created, deleted or restored, so failures are visible to systemd and scripts.
