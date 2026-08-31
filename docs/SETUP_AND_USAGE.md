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

- Replicate the snapshots to the targets defined in the config file (see Replication below):

`sudo rusnapshot -c {{path/to/config.toml}} --send`

- Replicate to a target given on the command line, an external disk or a host through ssh:

`sudo rusnapshot -c {{path/to/config.toml}} --send --target /mnt/usb/backups`

`sudo rusnapshot -c {{path/to/config.toml}} --send --target ssh://backup@nas/srv/backups/behemoth`

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

## Replication

`--send` replicates the snapshots with `btrfs send`/`btrfs receive` to a directory on another btrfs filesystem: an external disk, or another machine through ssh.

```toml
[[replicate]]
target = "ssh://backup@nas:2222/srv/backups/behemoth"
# Replicas to keep per kind at the target. Omit it to never delete anything there.
keep = 30
# Extra ssh options. rusnapshot usually runs as root, which does not see your
# ~/.ssh/config, so give it the key and the known_hosts file it should use.
ssh_options = ["-i", "/root/.ssh/backup_key", "-o", "UserKnownHostsFile=/root/.ssh/known_hosts"]

[[replicate]]
target = "/mnt/usb/backups"
```

Add `--send` to the units after `--create`:

```
ExecStart=/usr/bin/rusnapshot -c /etc/rusnapshot/config-root.toml --create --kind daily
ExecStart=/usr/bin/rusnapshot -c /etc/rusnapshot/config-root.toml --send
ExecStart=/usr/bin/rusnapshot -c /etc/rusnapshot/config-root.toml --clean --kind daily
```

How it works:

- `--send` replicates every read-only snapshot of the config's `--prefix` and `-m/--machine` that is not yet at the target, oldest first, whatever its kind. Read-write snapshots can't be sent by btrfs and are skipped.
- The first send of a subvolume is full. Every later one is incremental (`btrfs send -p`) from the newest replica of the same source subvolume that still exists on both sides; only the changes are sent. If that replica disappeared from the target, it is forgotten (and sent again later) and the next one is tried; with no usable parent the send is full again.
- After each transfer the target filesystem is synced (`btrfs filesystem sync`), so the data is written to the disk before the replica is recorded; the reported time includes that. Then the `Received UUID` at the target is compared with the UUID of the source snapshot. Only then it is recorded in the database. A transfer that fails is removed from the target and retried on the next run, and rusnapshot exits with a non-zero status.
- If the target already holds a matching replica that the database does not know about (for example after restoring the database), it is adopted instead of sent again.
- With `keep`, after sending, the replicas beyond the newest `keep` ones of each kind are deleted at the target. They are remembered as pruned so the local snapshots are not sent there again. `keep` must be at least 1: the newest replica is the parent of the next incremental send.
- Several targets can be configured; each one is processed independently. `--target` on the command line replaces the configured targets for that run.
- `--dry-run` prints what would be sent and pruned without contacting the target.

Requirements: the target directory must already exist on a btrfs filesystem (rusnapshot does not create it, so an unmounted backup disk is reported as an error instead of being written to the mount point); `btrfs-progs` at the target; for ssh, key based authentication that works non-interactively (`BatchMode=yes` is always set) and **passwordless sudo for the ssh user**: the `btrfs` commands at the target need root and rusnapshot always runs them as `sudo -n btrfs ...` (logging in as root works too). Only `btrfs` goes through sudo, so a rule such as `backup ALL=(root) NOPASSWD: /usr/bin/btrfs` is enough; check it with `ssh backup@nas sudo -n btrfs --version`. The ssh user must also be able to read the target directory. A host without a btrfs filesystem can still be a target with a loop-mounted image: `truncate -s 200G backups.img && mkfs.btrfs backups.img && mount -o loop backups.img /srv/backups`.

Keep the local retention (`keep_only`) at 2 or more when replicating: if `--clean` deletes the newest replicated snapshot before the next send, that send has no parent and is full again.

`--list` prints the replicas present at each target below the snapshots table.

## Exit codes

`rusnapshot` exits with a non-zero status whenever a snapshot can't be created, deleted, restored or replicated, so failures are visible to systemd and scripts.
