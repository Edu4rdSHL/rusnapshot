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

`sudo rusnapshot --config {{path/to/config.toml}} --cr`

- List created snapshots:

`sudo rusnapshot -c {{path/to/config.toml}} --list`

- Delete a snapshot by ID or the name of the snapshot:

`sudo rusnapshot -c {{path/to/config.toml}} --del --id {{snapshot_id}}`

- Delete all `hourly` snapshots:

`sudo rusnapshot -c {{path/to/config.toml}} --list --keep {{0}} --clean --kind {{hourly}}`

- Create a read-write snapshot:

`sudo rusnapshot -c {{path/to/config.toml}} --cr --rw`

- Restore a snapshot:

`sudo rusnapshot -c {{path/to/config.toml}} --id {{snapshot_id}} --restore`

# Notes

- You can use system variables to work with rusnapshot, the variables needs to be set with the `RUSNAPSHOT_` prefix followed by oned of the strings avilable in the [config example](https://github.com/Edu4rdSHL/rusnapshot/blob/master/examples/config-templates/config-all.toml), for example `RUSNAPSHOT_DATABASE_FILE=/path/to/database.sqlite`
- The `--kind` option can have any value and allow you to have different "kinds" of snapshots for the same directory, see the [services/timers examples](https://github.com/Edu4rdSHL/rusnapshot/tree/master/examples/services) for more info.
- The `--prefix` option is used to declare the first part of the snapshot name.
