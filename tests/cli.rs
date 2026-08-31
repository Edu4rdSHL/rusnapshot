//! End-to-end tests. They run the real binary against a fake `btrfs` script that simulates
//! `subvolume create/snapshot/delete` with plain directories, so no root or btrfs filesystem
//! is needed.

use {
    rusnapshot::{
        database,
        structs::{ReplicaRecord, SnapshotRecord},
    },
    std::{
        fs,
        os::unix::fs::PermissionsExt,
        path::{Path, PathBuf},
        process::{Command, Output},
    },
};

const FAKE_BTRFS: &str = r#"#!/usr/bin/env bash
# Fake btrfs: logs every invocation and simulates subvolumes with plain directories.
# A subvolume is a directory holding a .uuid file; a received one also holds .received_uuid,
# a read-only one holds .ro. Snapshots copy the content of sandbox paths.
LOG="${FAKE_BTRFS_LOG:?}"
echo "btrfs $*" >> "$LOG"
if [ -n "$FAKE_BTRFS_FAIL" ]; then echo "fake btrfs: forced failure" >&2; exit 1; fi
newuuid() { od -An -N16 -tx1 /dev/urandom | tr -d ' \n' | sed 's/\(.\{8\}\)\(.\{4\}\)\(.\{4\}\)\(.\{4\}\)\(.\{12\}\)/\1-\2-\3-\4-\5/'; }
datasize() { find "$1" -type f ! -name .uuid ! -name .received_uuid ! -name .ro -printf '%s\n' | awk '{s+=$1} END {print s+0}'; }
sub="$1"; op="$2"; shift 2
case "$sub/$op" in
  subvolume/create)
    [ -d "$(dirname "$1")" ] || { echo "ERROR: cannot access '$1'" >&2; exit 1; }
    mkdir "$1" && newuuid > "$1/.uuid" ;;
  subvolume/snapshot)
    ro=0; pos=()
    for a in "$@"; do if [ "$a" = "-r" ]; then ro=1; else pos+=("$a"); fi; done
    src="${pos[0]}"; dst="${pos[1]}"
    [ -d "$src" ] || { echo "ERROR: cannot access '$src'" >&2; exit 1; }
    if [ -d "$dst" ]; then dst="$dst/$(basename "$src")"; fi
    [ -d "$(dirname "$dst")" ] || { echo "ERROR: cannot access '$dst': No such file or directory" >&2; exit 1; }
    mkdir "$dst" || exit 1
    case "$src" in "${FAKE_BTRFS_SANDBOX:-/nonexistent}"/*) cp -a "$src/." "$dst/" ;; esac
    rm -f "$dst/.received_uuid" "$dst/.ro"
    newuuid > "$dst/.uuid"
    if [ "$ro" = 1 ]; then touch "$dst/.ro"; fi
    exit 0 ;;
  subvolume/delete)
    [ -d "$1" ] || { echo "ERROR: Not a Btrfs subvolume: $1" >&2; exit 1; }
    rm -rf "$1" ;;
  filesystem/sync)
    [ -d "$1" ] || { echo "ERROR: not a directory: $1" >&2; exit 1; } ;;
  subvolume/show)
    [ -f "$1/.uuid" ] || { echo "ERROR: Not a Btrfs subvolume: $1" >&2; exit 1; }
    recv="-"; [ -f "$1/.received_uuid" ] && recv=$(cat "$1/.received_uuid")
    printf '%s\n\tName: \t\t\t%s\n\tUUID: \t\t\t%s\n\tParent UUID: \t\t-\n\tReceived UUID: \t\t%s\n\tFlags: \t\t\treadonly\n' "$1" "$(basename "$1")" "$(cat "$1/.uuid")" "$recv" ;;
  property/set)
    [ "$1" = "-ts" ] && shift
    [ -f "$1/.uuid" ] || { echo "ERROR: not a subvolume: $1" >&2; exit 1; }
    if [ "$2" = "ro" ]; then if [ "$3" = "true" ]; then touch "$1/.ro"; else rm -f "$1/.ro"; fi; fi ;;
  property/get)
    [ "$1" = "-ts" ] && shift
    [ -f "$1/.uuid" ] || { echo "ERROR: not a subvolume: $1" >&2; exit 1; }
    if [ -f "$1/.ro" ]; then echo "ro=true"; else echo "ro=false"; fi ;;
  send/*)
    set -- "$op" "$@"
    parent="-"
    if [ "$1" = "-p" ]; then parent=$(cat "$2/.uuid"); shift 2; fi
    path="$1"
    [ -f "$path/.uuid" ] || { echo "ERROR: not a subvolume: $path" >&2; exit 1; }
    printf 'FAKE-BTRFS-STREAM name=%s uuid=%s parent=%s\n' "$(basename "$path")" "$(cat "$path/.uuid")" "$parent"
    head -c "${FAKE_BTRFS_PAYLOAD:-$(datasize "$path")}" /dev/zero ;;
  receive/*)
    dir="$op"
    header=$(head -n1); cat > /dev/null
    name=${header#*name=}; name=${name%% *}
    uuid=${header#*uuid=}; uuid=${uuid%% *}
    parent=${header#*parent=}
    [ -d "$dir" ] || { echo "ERROR: cannot access '$dir'" >&2; exit 1; }
    [ -e "$dir/$name" ] && { echo "ERROR: creating snapshot $name: File exists" >&2; exit 1; }
    if [ "$parent" != "-" ]; then
      found=0
      for d in "$dir"/*/; do [ -f "$d/.received_uuid" ] && [ "$(cat "$d/.received_uuid")" = "$parent" ] && found=1; done
      [ $found = 1 ] || { echo "ERROR: cannot find parent subvolume" >&2; exit 1; }
    fi
    mkdir "$dir/$name" && newuuid > "$dir/$name/.uuid"
    if [ -n "$FAKE_BTRFS_RECEIVE_FAIL" ]; then echo "ERROR: fake receive failure" >&2; exit 1; fi
    echo "$uuid" > "$dir/$name/.received_uuid" ;;
  *) echo "fake btrfs: unsupported command: $sub $op $*" >&2; exit 1 ;;
esac
"#;

const FAKE_SSH: &str = r#"#!/usr/bin/env bash
# Fake ssh: logs the host and runs the remote command line locally.
LOG="${FAKE_BTRFS_LOG:?}"
host=""; cmd=""
while [ $# -gt 0 ]; do
  case "$1" in
    --) shift; cmd="$*"; break ;;
    -o|-p|-i|-F|-l) shift 2 ;;
    -*) shift ;;
    *) host="$1"; shift ;;
  esac
done
echo "ssh $host: $cmd" >> "$LOG"
exec bash -c "$cmd"
"#;

const FAKE_SUDO: &str = r#"#!/usr/bin/env bash
# Fake sudo: runs the command as is.
[ "$1" = "-n" ] && shift
exec "$@"
"#;

struct Sandbox {
    dir: PathBuf,
    src: PathBuf,
    snaps: PathBuf,
    db: PathBuf,
    log: PathBuf,
    /// Simulated replication target directory.
    target: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("rusnapshot-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("bin")).unwrap();
        fs::create_dir_all(dir.join("src")).unwrap();
        for (name, script) in [
            ("btrfs", FAKE_BTRFS),
            ("ssh", FAKE_SSH),
            ("sudo", FAKE_SUDO),
        ] {
            let fake = dir.join("bin").join(name);
            fs::write(&fake, script).unwrap();
            fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
        }
        fs::create_dir_all(dir.join("target")).unwrap();
        Self {
            src: dir.join("src"),
            snaps: dir.join("snaps"),
            db: dir.join("snaps/db.sqlite"),
            log: dir.join("btrfs.log"),
            target: dir.join("target"),
            dir,
        }
    }

    fn run_env(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let path = format!(
            "{}:{}",
            self.dir.join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut command = Command::new(env!("CARGO_BIN_EXE_rusnapshot"));
        command
            .args(args)
            .current_dir(&self.dir)
            .env("PATH", path)
            .env("FAKE_BTRFS_LOG", &self.log)
            .env("FAKE_BTRFS_SANDBOX", &self.dir)
            .env_remove("RUSNAPSHOT_DB_FILE")
            .env_remove("POSIXLY_CORRECT");
        for (key, value) in env {
            command.env(key, value);
        }
        command.output().unwrap()
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_env(args, &[])
    }

    /// `--create` from `src` into `snaps` with the sandbox database and machine `test`
    /// (unless `extra` sets `--machine`).
    fn create(&self, extra: &[&str]) -> Output {
        let mut args = vec![
            "--create",
            "--from",
            self.src.to_str().unwrap(),
            "--to",
            self.snaps.to_str().unwrap(),
            "-d",
            self.db.to_str().unwrap(),
        ];
        if !extra.contains(&"--machine") {
            args.extend_from_slice(&["--machine", "test"]);
        }
        args.extend_from_slice(extra);
        let output = self.run(&args);
        assert!(output.status.success(), "create failed: {}", text(&output));
        output
    }

    fn db_path(&self) -> &str {
        self.db.to_str().unwrap()
    }

    fn records(&self) -> Vec<SnapshotRecord> {
        records_in(&self.db)
    }

    fn on_disk(&self) -> Vec<String> {
        let Ok(entries) = fs::read_dir(&self.snaps) else {
            return Vec::new();
        };
        let mut names: Vec<String> = entries
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| !n.starts_with("db.sqlite") && !n.starts_with('.'))
            .collect();
        names.sort();
        names
    }

    fn log(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }

    fn replicas(&self) -> Vec<ReplicaRecord> {
        let connection = sqlite::open(&self.db).unwrap();
        database::list_replicas(&connection).unwrap()
    }

    fn target_str(&self) -> &str {
        self.target.to_str().unwrap()
    }

    /// `--send` to the sandbox target directory as machine `test`.
    fn send(&self, extra: &[&str]) -> Output {
        let mut args = vec![
            "--send",
            "--target",
            self.target_str(),
            "-m",
            "test",
            "-d",
            self.db_path(),
        ];
        args.extend_from_slice(extra);
        self.run(&args)
    }

    /// Names of the replicas present in the target directory.
    fn replicas_on_disk(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(&self.target)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn write(&self, name: &str, content: &str) -> String {
        let path = self.dir.join(name);
        fs::write(&path, content).unwrap();
        path.to_str().unwrap().to_string()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn read_trimmed(path: &Path) -> String {
    fs::read_to_string(path).unwrap().trim().to_string()
}

/// The replica at `target/name` must have been received from the snapshot at `snaps/name`.
fn assert_replica_matches(sb: &Sandbox, name: &str) {
    let source_uuid = read_trimmed(&sb.snaps.join(name).join(".uuid"));
    let received = read_trimmed(&sb.target.join(name).join(".received_uuid"));
    assert_eq!(received, source_uuid, "{name}");
}

fn records_in(db: &Path) -> Vec<SnapshotRecord> {
    let connection = sqlite::open(db).unwrap();
    database::list_all(&connection).unwrap()
}

fn text(output: &Output) -> String {
    format!(
        "status: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn assert_failed(output: &Output, needle: &str) {
    assert!(
        !output.status.success(),
        "expected failure: {}",
        text(output)
    );
    assert!(
        stderr(output).contains(needle),
        "stderr lacks {needle:?}: {}",
        text(output)
    );
}

#[test]
fn create_sets_up_destination_and_database_on_first_run() {
    let sb = Sandbox::new("first-run");
    assert!(!sb.snaps.exists());

    let output = sb.create(&["--prefix", "root", "--kind", "daily"]);
    assert!(
        stdout(&output).contains("Snapshot root-"),
        "{}",
        text(&output)
    );

    let disk = sb.on_disk();
    assert_eq!(disk.len(), 1, "{disk:?}");
    assert!(disk[0].starts_with("root-"));

    let records = sb.records();
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert_eq!(record.name, disk[0]);
    assert_eq!(record.kind, "daily");
    assert_eq!(record.machine, "test");
    assert_eq!(record.ro_rw, "false");
    assert_eq!(record.source, format!("{}/", sb.src.display()));
    assert_eq!(record.destination, format!("{}/", sb.snaps.display()));
    assert_eq!(record.snap_id, format!("{:x}", md5::compute(&record.name)));
    assert!(Path::new(&record.path()).is_dir());

    // The subvolume is created (and awaited) before the snapshot, -r goes before the paths and
    // there is no double slash in the target path.
    let log = sb.log();
    let create_line = format!("btrfs subvolume create {}\n", sb.snaps.display());
    let snapshot_line = format!(
        "btrfs subvolume snapshot -r {} {}\n",
        sb.src.display(),
        record.path()
    );
    assert_eq!(log, format!("{create_line}{snapshot_line}"));
}

#[test]
fn create_with_relative_paths_and_rw() {
    let sb = Sandbox::new("relative");
    let output = sb.run(&[
        "--create",
        "--from",
        "src",
        "--to",
        "snaps",
        "-d",
        "snaps/db.sqlite",
        "--rw",
        "-m",
        "test",
    ]);
    assert!(output.status.success(), "{}", text(&output));
    let records = sb.records();
    assert_eq!(records[0].source, format!("{}/", sb.src.display()));
    assert_eq!(records[0].destination, format!("{}/", sb.snaps.display()));
    assert_eq!(records[0].ro_rw, "true");
    assert!(sb.log().contains("btrfs subvolume snapshot ") && !sb.log().contains(" -r "));
}

#[test]
fn create_fails_with_nonzero_exit_when_btrfs_fails() {
    let sb = Sandbox::new("btrfs-fails");
    fs::create_dir_all(&sb.snaps).unwrap();
    let output = sb.run_env(
        &[
            "--create",
            "--from",
            sb.src.to_str().unwrap(),
            "--to",
            sb.snaps.to_str().unwrap(),
            "-d",
            sb.db_path(),
            "-m",
            "test",
        ],
        &[("FAKE_BTRFS_FAIL", "1")],
    );
    assert_failed(&output, "btrfs subvolume snapshot");
    assert!(sb.records().is_empty());
}

#[test]
fn create_fails_cleanly_without_btrfs_binary() {
    let sb = Sandbox::new("no-btrfs");
    fs::create_dir_all(&sb.snaps).unwrap();
    let output = sb.run_env(
        &[
            "--create",
            "--from",
            sb.src.to_str().unwrap(),
            "--to",
            sb.snaps.to_str().unwrap(),
            "-d",
            sb.db_path(),
            "-m",
            "test",
        ],
        &[("PATH", "/nonexistent")],
    );
    assert_failed(&output, "btrfs-progs");
    assert!(!stderr(&output).contains("panicked"), "{}", text(&output));
}

#[test]
fn create_requires_both_directories() {
    let sb = Sandbox::new("missing-dirs");
    let output = sb.run(&[
        "--create",
        "--from",
        sb.src.to_str().unwrap(),
        "-d",
        sb.db_path(),
    ]);
    assert_failed(&output, "--to");
    assert!(!sb.db.exists());
}

#[test]
fn create_dry_run_touches_nothing() {
    let sb = Sandbox::new("create-dry-run");
    let output = sb.run(&[
        "--create",
        "--from",
        sb.src.to_str().unwrap(),
        "--to",
        sb.snaps.to_str().unwrap(),
        "-d",
        sb.db_path(),
        "-m",
        "test",
        "--dry-run",
    ]);
    assert!(output.status.success(), "{}", text(&output));
    let out = stdout(&output);
    assert!(out.contains("would run: btrfs subvolume create"), "{out}");
    assert!(
        out.contains("would run: btrfs subvolume snapshot -r"),
        "{out}"
    );
    assert!(!sb.snaps.exists());
    assert!(!sb.db.exists());
    assert!(sb.log().is_empty());
}

#[test]
fn command_line_takes_precedence_over_config_file() {
    let sb = Sandbox::new("precedence");
    let config = sb.write(
        "config.toml",
        &format!(
            "dest_dir = \"{}\"\nsource_dir = \"{}\"\ndatabase_file = \"{}\"\nsnapshot_prefix = \"cfg\"\nsnapshot_kind = \"weekly\"\nkeep_only = 3\nmachine = \"Oribos\"\n",
            sb.snaps.display(),
            sb.src.display(),
            sb.db.display()
        ),
    );

    let output = sb.run(&["-c", &config, "--create"]);
    assert!(output.status.success(), "{}", text(&output));
    let output = sb.run(&[
        "-c",
        &config,
        "--create",
        "--kind",
        "daily",
        "--prefix",
        "cli",
        "--machine",
        "clihost",
    ]);
    assert!(output.status.success(), "{}", text(&output));

    let mut records = sb.records();
    records.sort_by(|a, b| a.name.cmp(&b.name));
    assert_eq!(records.len(), 2);
    assert!(records[0].name.starts_with("cfg-"));
    assert_eq!(records[0].kind, "weekly");
    // Stored without the TOML quotes.
    assert_eq!(records[0].machine, "Oribos");
    assert!(records[1].name.starts_with("cli-"));
    assert_eq!(records[1].kind, "daily");
    assert_eq!(records[1].machine, "clihost");
}

#[test]
fn environment_variable_takes_precedence_over_config_file() {
    let sb = Sandbox::new("env-precedence");
    let other_db = sb.dir.join("other.sqlite");
    let config = sb.write(
        "config.toml",
        &format!(
            "dest_dir = \"{}\"\nsource_dir = \"{}\"\ndatabase_file = \"{}\"\n",
            sb.snaps.display(),
            sb.src.display(),
            sb.db.display()
        ),
    );
    let output = sb.run_env(
        &["-c", &config, "--create", "-m", "test"],
        &[("RUSNAPSHOT_DB_FILE", other_db.to_str().unwrap())],
    );
    assert!(output.status.success(), "{}", text(&output));
    assert!(other_db.exists());
    assert!(!sb.db.exists());
}

#[test]
fn config_file_errors_are_reported() {
    let sb = Sandbox::new("bad-config");
    let config = sb.write("config.toml", "keep_only = \"3\"\n");
    let output = sb.run(&["-c", &config, "--list", "-d", sb.db_path()]);
    assert_failed(&output, "keep_only");
    assert!(!stderr(&output).contains("panicked"), "{}", text(&output));

    let output = sb.run(&[
        "-c",
        "/nonexistent/config.toml",
        "--list",
        "-d",
        sb.db_path(),
    ]);
    assert_failed(&output, "/nonexistent/config.toml");
}

#[test]
fn clean_keeps_the_last_n_and_dry_run_only_reports() {
    let sb = Sandbox::new("clean");
    for _ in 0..5 {
        sb.create(&["--prefix", "p", "--kind", "k"]);
    }
    let mut names = sb.on_disk();
    assert_eq!(names.len(), 5);
    names.sort();
    let (oldest, newest) = names.split_at(2);

    let output = sb.run(&[
        "--clean",
        "--dry-run",
        "--prefix",
        "p",
        "--kind",
        "k",
        "--keep",
        "3",
        "-m",
        "test",
        "-d",
        sb.db_path(),
    ]);
    assert!(output.status.success(), "{}", text(&output));
    let out = stdout(&output);
    assert!(out.contains("Would delete 2 snapshot(s)"), "{out}");
    for name in oldest {
        assert!(out.contains(name), "{out}");
    }
    for name in newest {
        assert!(!out.contains(name), "{out}");
    }
    assert!(out.contains("nothing was deleted"), "{out}");
    assert_eq!(sb.on_disk().len(), 5);
    assert_eq!(sb.records().len(), 5);

    let output = sb.run(&[
        "--clean",
        "--prefix",
        "p",
        "--kind",
        "k",
        "--keep",
        "3",
        "-m",
        "test",
        "-d",
        sb.db_path(),
    ]);
    assert!(output.status.success(), "{}", text(&output));
    assert_eq!(sb.on_disk(), newest);
    let mut remaining: Vec<String> = sb.records().into_iter().map(|r| r.name).collect();
    remaining.sort();
    assert_eq!(remaining, newest);

    let output = sb.run(&[
        "--clean",
        "--prefix",
        "p",
        "--kind",
        "k",
        "--keep",
        "3",
        "-m",
        "test",
        "-d",
        sb.db_path(),
    ]);
    assert!(output.status.success(), "{}", text(&output));
    assert!(
        stdout(&output).contains("Nothing to clean"),
        "{}",
        text(&output)
    );
}

#[test]
fn clean_only_touches_snapshots_of_the_given_machine() {
    let sb = Sandbox::new("clean-machine");
    sb.create(&["--prefix", "p", "--kind", "k", "--machine", "A"]);
    sb.create(&["--prefix", "p", "--kind", "k", "--machine", "A"]);
    sb.create(&["--prefix", "p", "--kind", "k", "--machine", "B"]);
    sb.create(&["--prefix", "p", "--kind", "k", "--machine", "B"]);

    let output = sb.run(&[
        "--clean",
        "--prefix",
        "p",
        "--kind",
        "k",
        "--keep",
        "2",
        "-m",
        "A",
        "-d",
        sb.db_path(),
    ]);
    assert!(output.status.success(), "{}", text(&output));
    assert_eq!(
        sb.records().len(),
        4,
        "machine B's newer snapshots must not evict A's"
    );

    let output = sb.run(&[
        "--clean",
        "--prefix",
        "p",
        "--kind",
        "k",
        "--keep",
        "1",
        "-m",
        "A",
        "-d",
        sb.db_path(),
    ]);
    assert!(output.status.success(), "{}", text(&output));
    let records = sb.records();
    assert_eq!(records.len(), 3);
    assert_eq!(records.iter().filter(|r| r.machine == "A").count(), 1);
    assert_eq!(records.iter().filter(|r| r.machine == "B").count(), 2);
    assert_eq!(sb.on_disk().len(), 3);
}

#[test]
fn clean_prefix_is_a_like_pattern() {
    // Intentional: cleaning `home` also covers `home-data`.
    let sb = Sandbox::new("clean-like");
    sb.create(&["--prefix", "home", "--kind", "k"]);
    sb.create(&["--prefix", "home-data", "--kind", "k"]);
    sb.create(&["--prefix", "root", "--kind", "k"]);
    let output = sb.run(&[
        "--clean",
        "--prefix",
        "home",
        "--kind",
        "k",
        "--keep",
        "0",
        "-m",
        "test",
        "-d",
        sb.db_path(),
    ]);
    assert!(output.status.success(), "{}", text(&output));
    let disk = sb.on_disk();
    assert_eq!(disk.len(), 1);
    assert!(disk[0].starts_with("root-"));
}

#[test]
fn clean_uses_the_rw_flag_to_select_the_mode() {
    let sb = Sandbox::new("clean-rw");
    sb.create(&["--prefix", "p", "--kind", "k", "--rw"]);
    sb.create(&["--prefix", "p", "--kind", "k"]);
    let output = sb.run(&[
        "--clean",
        "--prefix",
        "p",
        "--kind",
        "k",
        "--keep",
        "0",
        "-m",
        "test",
        "-d",
        sb.db_path(),
    ]);
    assert!(output.status.success(), "{}", text(&output));
    let records = sb.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].ro_rw, "true");
    let output = sb.run(&[
        "--clean",
        "--prefix",
        "p",
        "--kind",
        "k",
        "--keep",
        "0",
        "-m",
        "test",
        "--rw",
        "-d",
        sb.db_path(),
    ]);
    assert!(output.status.success(), "{}", text(&output));
    assert!(sb.records().is_empty());
}

#[test]
fn clean_and_list_together_show_the_resulting_state() {
    let sb = Sandbox::new("clean-list");
    sb.create(&["--prefix", "p", "--kind", "k"]);
    sb.create(&["--prefix", "p", "--kind", "k"]);
    let output = sb.run(&[
        "--list",
        "--clean",
        "--prefix",
        "p",
        "--kind",
        "k",
        "--keep",
        "0",
        "-m",
        "test",
        "-d",
        sb.db_path(),
    ]);
    assert!(output.status.success(), "{}", text(&output));
    assert!(
        stdout(&output).contains("No snapshots are tracked"),
        "{}",
        text(&output)
    );
}

#[test]
fn delete_by_name_and_by_id() {
    let sb = Sandbox::new("delete");
    sb.create(&[]);
    sb.create(&[]);
    let records = sb.records();

    let output = sb.run(&["--del", "--id", &records[0].name, "-d", sb.db_path()]);
    assert!(output.status.success(), "{}", text(&output));
    let output = sb.run(&["--del", "--id", &records[1].snap_id, "-d", sb.db_path()]);
    assert!(output.status.success(), "{}", text(&output));
    assert!(sb.records().is_empty());
    assert!(sb.on_disk().is_empty());
    assert!(
        sb.log()
            .contains(&format!("btrfs subvolume delete {}\n", records[0].path()))
    );

    let output = sb.run(&["--del", "--id", "nope", "-d", sb.db_path()]);
    assert_failed(&output, "no snapshot found");
}

#[test]
fn delete_dry_run_and_btrfs_failure() {
    let sb = Sandbox::new("delete-failure");
    sb.create(&[]);
    let record = sb.records().remove(0);

    let output = sb.run(&[
        "--del",
        "--id",
        &record.snap_id,
        "--dry-run",
        "-d",
        sb.db_path(),
    ]);
    assert!(output.status.success(), "{}", text(&output));
    assert!(
        stdout(&output).contains("would delete"),
        "{}",
        text(&output)
    );
    assert_eq!(sb.records().len(), 1);
    assert_eq!(sb.on_disk().len(), 1);

    let output = sb.run_env(
        &["--del", "--id", &record.snap_id, "-d", sb.db_path()],
        &[("FAKE_BTRFS_FAIL", "1")],
    );
    assert_failed(&output, "btrfs subvolume delete");
    assert_eq!(sb.records().len(), 1, "the row must stay when btrfs fails");
}

#[test]
fn delete_forgets_rows_whose_subvolume_is_gone_from_this_machine() {
    let sb = Sandbox::new("delete-gone");
    sb.create(&["--machine", "test"]);
    sb.create(&["--machine", "other"]);
    let records = sb.records();
    let mine = records.iter().find(|r| r.machine == "test").unwrap();
    let theirs = records.iter().find(|r| r.machine == "other").unwrap();
    fs::remove_dir_all(mine.path()).unwrap();
    fs::remove_dir_all(theirs.path()).unwrap();

    let output = sb.run(&[
        "--del",
        "--id",
        &mine.snap_id,
        "-m",
        "test",
        "-d",
        sb.db_path(),
    ]);
    assert!(output.status.success(), "{}", text(&output));
    assert!(
        stderr(&output).contains("no longer exists"),
        "{}",
        text(&output)
    );
    assert_eq!(sb.records().len(), 1);

    let output = sb.run(&[
        "--del",
        "--id",
        &theirs.snap_id,
        "-m",
        "test",
        "-d",
        sb.db_path(),
    ]);
    assert_failed(&output, "created on machine 'other'");
    assert_eq!(sb.records().len(), 1);
    assert!(!sb.log().contains("subvolume delete"));
}

#[test]
fn clean_reports_failures_but_keeps_going() {
    let sb = Sandbox::new("clean-partial");
    sb.create(&["--prefix", "p", "--kind", "k", "--machine", "test"]);
    sb.create(&["--prefix", "p", "--kind", "k", "--machine", "test"]);
    // A row of another machine with the same prefix/kind must never be selected.
    sb.create(&["--prefix", "p", "--kind", "k", "--machine", "other"]);
    let gone = sb
        .records()
        .into_iter()
        .find(|r| r.machine == "test")
        .unwrap();
    fs::remove_dir_all(gone.path()).unwrap();

    let output = sb.run(&[
        "--clean",
        "--prefix",
        "p",
        "--kind",
        "k",
        "--keep",
        "0",
        "-m",
        "test",
        "-d",
        sb.db_path(),
    ]);
    assert!(output.status.success(), "{}", text(&output));
    assert!(
        stderr(&output).contains("no longer exists"),
        "{}",
        text(&output)
    );
    let records = sb.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].machine, "other");
}

#[test]
fn restore_refuses_existing_target_and_explains() {
    let sb = Sandbox::new("restore-existing");
    sb.create(&[]);
    let record = sb.records().remove(0);
    let output = sb.run(&["--restore", "--id", &record.snap_id, "-d", sb.db_path()]);
    assert_failed(&output, "already exists");
    assert!(stderr(&output).contains("mv "), "{}", text(&output));
    // Only the snapshot taken by create, no restore attempt.
    assert_eq!(sb.log().matches("subvolume snapshot").count(), 1);
}

#[test]
fn restore_of_the_root_subvolume_in_place_is_refused() {
    let sb = Sandbox::new("restore-root");
    let output = sb.run(&[
        "--create",
        "--from",
        "/",
        "--to",
        sb.snaps.to_str().unwrap(),
        "-d",
        sb.db_path(),
        "-m",
        "test",
    ]);
    assert!(output.status.success(), "{}", text(&output));
    let record = sb.records().remove(0);
    assert_eq!(record.source, "/");
    let output = sb.run(&["--restore", "--id", &record.snap_id, "-d", sb.db_path()]);
    assert_failed(&output, "root subvolume");
    assert!(!stderr(&output).contains("mv "), "{}", text(&output));
}

#[test]
fn rows_are_kept_when_the_snapshots_directory_is_missing() {
    // Simulates a snapshots subvolume that is not mounted while the database lives elsewhere.
    let sb = Sandbox::new("unmounted");
    let db = sb.dir.join("outside.sqlite");
    let db_str = db.to_str().unwrap();
    for _ in 0..2 {
        let output = sb.run(&[
            "--create",
            "--from",
            sb.src.to_str().unwrap(),
            "--to",
            sb.snaps.to_str().unwrap(),
            "-d",
            db_str,
            "-m",
            "test",
        ]);
        assert!(output.status.success(), "{}", text(&output));
    }
    let record = records_in(&db).remove(0);
    fs::remove_dir_all(&sb.snaps).unwrap();

    let output = sb.run(&["--del", "--id", &record.snap_id, "-m", "test", "-d", db_str]);
    assert_failed(&output, "mounted");
    let output = sb.run(&["--clean", "--keep", "0", "-m", "test", "-d", db_str]);
    assert_failed(&output, "2 of 2 snapshot(s) could not be deleted");
    assert_eq!(records_in(&db).len(), 2);
    assert!(!sb.log().contains("subvolume delete"));
}

#[test]
fn restore_to_original_location_after_moving_it_away() {
    let sb = Sandbox::new("restore-original");
    sb.create(&[]);
    let record = sb.records().remove(0);
    let moved = sb.dir.join("src.old");
    fs::rename(&sb.src, &moved).unwrap();

    let output = sb.run(&["--restore", "--id", &record.snap_id, "-d", sb.db_path()]);
    assert!(output.status.success(), "{}", text(&output));
    assert!(sb.src.is_dir());
    assert!(
        sb.log().ends_with(&format!(
            "btrfs subvolume snapshot {} {}\n",
            record.path(),
            sb.src.display()
        )),
        "{}",
        sb.log()
    );
}

#[test]
fn restore_to_another_directory_with_to_and_dry_run() {
    let sb = Sandbox::new("restore-to");
    sb.create(&[]);
    let record = sb.records().remove(0);
    let target = sb.dir.join("restored");

    let output = sb.run(&[
        "--restore",
        "--id",
        &record.name,
        "--to",
        target.to_str().unwrap(),
        "--dry-run",
        "-d",
        sb.db_path(),
    ]);
    assert!(output.status.success(), "{}", text(&output));
    assert!(
        stdout(&output).contains("would run: btrfs subvolume snapshot"),
        "{}",
        text(&output)
    );
    assert!(!target.exists());

    let output = sb.run(&[
        "--restore",
        "--id",
        &record.name,
        "--to",
        target.to_str().unwrap(),
        "-d",
        sb.db_path(),
    ]);
    assert!(output.status.success(), "{}", text(&output));
    assert!(target.is_dir());
    assert!(sb.log().contains(&format!(
        "btrfs subvolume snapshot {} {}\n",
        record.path(),
        target.display()
    )));
}

#[test]
fn list_on_missing_database_fails_without_creating_it() {
    let sb = Sandbox::new("list-missing");
    let output = sb.run(&["--list", "-d", sb.db_path()]);
    assert_failed(&output, "does not exist");
    assert!(!sb.db.exists());
    let output = sb.run(&["--clean", "-d", sb.db_path()]);
    assert_failed(&output, "does not exist");
    // With --dry-run there is nothing to show.
    let output = sb.run(&["--clean", "--dry-run", "-d", sb.db_path()]);
    assert!(output.status.success(), "{}", text(&output));
    assert!(!sb.db.exists());
}

#[test]
fn list_shows_tracked_snapshots() {
    let sb = Sandbox::new("list");
    sb.create(&["--prefix", "listed"]);
    let record = sb.records().remove(0);
    let output = sb.run(&["--list", "-d", sb.db_path()]);
    assert!(output.status.success(), "{}", text(&output));
    let out = stdout(&output);
    assert!(
        out.contains(&record.name) && out.contains(&record.snap_id) && out.contains("test"),
        "{out}"
    );
}

#[test]
fn invalid_flag_combinations_are_rejected() {
    let sb = Sandbox::new("flags");
    let cases: &[&[&str]] = &[
        &["--del", "--restore", "--id", "x"],
        &["--clean", "--del", "--id", "x"],
        &["--create", "--clean"],
        &["--create", "--list"],
        &["--del"],
        &["--restore"],
        &["--restore", "--id", "x", "--from", "/somewhere"],
        &["--del", "--id", "x", "--to", "/somewhere"],
        &["--target", "/x", "--list"],
        &["--send", "--create"],
        &["--send", "--clean"],
        &["--send", "--to", "/x"],
    ];
    for case in cases {
        let mut args = case.to_vec();
        args.extend_from_slice(&["-d", sb.db_path()]);
        let output = sb.run(&args);
        assert_eq!(output.status.code(), Some(2), "{case:?}: {}", text(&output));
    }
}

#[test]
fn paths_with_single_quotes_work() {
    let sb = Sandbox::new("quotes");
    let src = sb.dir.join("it's");
    fs::create_dir_all(&src).unwrap();
    let output = sb.run(&[
        "--create",
        "--from",
        src.to_str().unwrap(),
        "--to",
        sb.snaps.to_str().unwrap(),
        "-d",
        sb.db_path(),
        "-m",
        "o'host",
    ]);
    assert!(output.status.success(), "{}", text(&output));
    let record = sb.records().remove(0);
    assert_eq!(record.source, format!("{}/", src.display()));
    assert_eq!(record.machine, "o'host");
    let output = sb.run(&["--del", "--id", &record.snap_id, "-d", sb.db_path()]);
    assert!(output.status.success(), "{}", text(&output));
    assert!(sb.records().is_empty());
}

#[test]
fn quoted_machine_values_from_old_versions_are_migrated() {
    let sb = Sandbox::new("migration");
    sb.create(&["--prefix", "old", "--kind", "k"]);
    {
        let connection = sqlite::open(&sb.db).unwrap();
        connection
            .execute("UPDATE snapshots SET machine = '\"Behemoth\"'")
            .unwrap();
    }
    let output = sb.run(&["--list", "-d", sb.db_path()]);
    assert!(output.status.success(), "{}", text(&output));
    assert_eq!(sb.records()[0].machine, "Behemoth");

    let output = sb.run(&[
        "--clean",
        "--prefix",
        "old",
        "--kind",
        "k",
        "--keep",
        "0",
        "-m",
        "Behemoth",
        "-d",
        sb.db_path(),
    ]);
    assert!(output.status.success(), "{}", text(&output));
    assert!(sb.records().is_empty());
}

#[test]
fn hostname_is_used_when_no_machine_is_given() {
    let sb = Sandbox::new("hostname");
    let output = sb.run(&[
        "--create",
        "--from",
        sb.src.to_str().unwrap(),
        "--to",
        sb.snaps.to_str().unwrap(),
        "-d",
        sb.db_path(),
    ]);
    assert!(output.status.success(), "{}", text(&output));
    let expected = hostname::get().unwrap().to_string_lossy().into_owned();
    assert_eq!(sb.records()[0].machine, expected);
    let output = sb.run(&["--clean", "--keep", "0", "-d", sb.db_path()]);
    assert!(output.status.success(), "{}", text(&output));
    assert!(sb.records().is_empty());
}

#[test]
fn send_full_then_incremental_to_a_local_target() {
    let sb = Sandbox::new("send-local");
    sb.create(&["--prefix", "p", "--kind", "k"]);
    let first = sb.records().remove(0);

    let output = sb.send(&["--prefix", "p"]);
    assert!(output.status.success(), "{}", text(&output));
    let out = stdout(&output);
    assert!(
        out.contains(&format!(
            "Sending {} to {} (full send)",
            first.name,
            sb.target_str()
        )),
        "{out}"
    );
    assert!(out.contains("Sent "), "{out}");
    assert_replica_matches(&sb, &first.name);
    let replicas = sb.replicas();
    assert_eq!(replicas.len(), 1);
    assert_eq!(replicas[0].name, first.name);
    assert_eq!(replicas[0].target, sb.target_str());
    assert_eq!(replicas[0].parent_name, None);
    assert_eq!(replicas[0].kind, "k");
    assert_eq!(replicas[0].machine, "test");
    assert_eq!(replicas[0].local_path, first.path());
    assert!(
        sb.log().contains(&format!("btrfs send {}\n", first.path())),
        "{}",
        sb.log()
    );
    assert!(
        sb.log()
            .contains(&format!("btrfs receive {}\n", sb.target_str())),
        "{}",
        sb.log()
    );

    // Second snapshot: incremental from the first one. Nothing is sent twice.
    sb.create(&["--prefix", "p", "--kind", "k"]);
    let second = sb
        .records()
        .into_iter()
        .find(|r| r.name != first.name)
        .unwrap();
    let output = sb.send(&["--prefix", "p"]);
    assert!(output.status.success(), "{}", text(&output));
    assert!(
        stdout(&output).contains(&format!("incremental from {}", first.name)),
        "{}",
        text(&output)
    );
    assert_replica_matches(&sb, &second.name);
    assert!(
        sb.log().contains(&format!(
            "btrfs send -p {} {}\n",
            first.path(),
            second.path()
        )),
        "{}",
        sb.log()
    );
    assert_eq!(sb.log().matches("btrfs send").count(), 2);
    let replicas = sb.replicas();
    assert_eq!(replicas.len(), 2);
    let second_replica = replicas.iter().find(|r| r.name == second.name).unwrap();
    assert_eq!(
        second_replica.parent_name.as_deref(),
        Some(first.name.as_str())
    );

    // Nothing pending: no send at all.
    let output = sb.send(&["--prefix", "p"]);
    assert!(output.status.success(), "{}", text(&output));
    assert!(
        stdout(&output).contains("Nothing to send"),
        "{}",
        text(&output)
    );
    assert_eq!(sb.log().matches("btrfs send").count(), 2);
}

#[test]
fn send_over_ssh_with_port_runs_btrfs_through_sudo() {
    let sb = Sandbox::new("send-ssh");
    sb.create(&[]);
    let record = sb.records().remove(0);
    let url = format!("ssh://backup@nas:2222{}", sb.target_str());

    let output = sb.run(&["--send", "--target", &url, "-m", "test", "-d", sb.db_path()]);
    assert!(output.status.success(), "{}", text(&output));
    assert_replica_matches(&sb, &record.name);
    let log = sb.log();
    assert!(
        log.contains(&format!(
            "ssh backup@nas: sh -c 'test -e {} && echo yes || echo no'
",
            sb.target_str()
        )),
        "{log}"
    );
    assert!(
        log.contains(&format!(
            "ssh backup@nas: sudo -n btrfs receive {}\n",
            sb.target_str()
        )),
        "{log}"
    );
    assert!(
        log.contains(&format!(
            "ssh backup@nas: sudo -n btrfs subvolume show {}/{}\n",
            sb.target_str(),
            record.name
        )),
        "{log}"
    );
    assert_eq!(sb.replicas()[0].target, url);
}

#[test]
fn send_dry_run_touches_nothing() {
    let sb = Sandbox::new("send-dry-run");
    sb.create(&["--prefix", "p"]);
    let record = sb.records().remove(0);
    let output = sb.send(&["--prefix", "p", "--dry-run"]);
    assert!(output.status.success(), "{}", text(&output));
    assert!(
        stdout(&output).contains(&format!(
            "[dry-run] would send {} to {} (full send)",
            record.name,
            sb.target_str()
        )),
        "{}",
        text(&output)
    );
    assert!(sb.replicas_on_disk().is_empty());
    assert!(sb.replicas().is_empty());
    assert!(!sb.log().contains("btrfs send"));
    // A missing target directory is fine for a dry run.
    let output = sb.run(&[
        "--send",
        "--target",
        "/nonexistent/target",
        "--dry-run",
        "-m",
        "test",
        "-d",
        sb.db_path(),
    ]);
    assert!(output.status.success(), "{}", text(&output));
}

#[test]
fn send_skips_rw_snapshots_and_other_machines() {
    let sb = Sandbox::new("send-rw");
    sb.create(&["--prefix", "p", "--rw"]);
    sb.create(&["--prefix", "p", "--machine", "other"]);
    sb.create(&["--prefix", "q"]);
    sb.create(&["--prefix", "p"]);
    let output = sb.send(&["--prefix", "p"]);
    assert!(output.status.success(), "{}", text(&output));
    let replicas = sb.replicas();
    assert_eq!(replicas.len(), 1, "{replicas:?}");
    assert_eq!(replicas[0].machine, "test");
    assert!(replicas[0].name.starts_with("p-"));
    let record = sb
        .records()
        .into_iter()
        .find(|r| r.name == replicas[0].name)
        .unwrap();
    assert_eq!(record.ro_rw, "false");
}

#[test]
fn send_failure_removes_the_partial_replica_and_a_retry_succeeds() {
    let sb = Sandbox::new("send-failure");
    sb.create(&["--prefix", "p"]);
    let record = sb.records().remove(0);

    let output = sb.run_env(
        &[
            "--send",
            "--target",
            sb.target_str(),
            "-m",
            "test",
            "-d",
            sb.db_path(),
            "--prefix",
            "p",
        ],
        &[("FAKE_BTRFS_RECEIVE_FAIL", "1")],
    );
    assert_failed(&output, "btrfs receive");
    assert!(
        stderr(&output).contains("Removing the incomplete replica"),
        "{}",
        text(&output)
    );
    assert!(
        sb.replicas_on_disk().is_empty(),
        "{:?}",
        sb.replicas_on_disk()
    );
    assert!(sb.replicas().is_empty());
    assert!(
        sb.log().contains(&format!(
            "btrfs subvolume delete {}/{}\n",
            sb.target_str(),
            record.name
        )),
        "{}",
        sb.log()
    );

    let output = sb.send(&["--prefix", "p"]);
    assert!(output.status.success(), "{}", text(&output));
    assert_replica_matches(&sb, &record.name);
    assert_eq!(sb.replicas().len(), 1);

    // btrfs send itself failing is reported too.
    sb.create(&["--prefix", "p"]);
    let output = sb.run_env(
        &[
            "--send",
            "--target",
            sb.target_str(),
            "-m",
            "test",
            "-d",
            sb.db_path(),
            "--prefix",
            "p",
        ],
        &[("FAKE_BTRFS_FAIL", "1")],
    );
    assert!(!output.status.success(), "{}", text(&output));
    assert_eq!(sb.replicas().len(), 1);
}

#[test]
fn send_adopts_replicas_already_present_at_the_target() {
    let sb = Sandbox::new("send-adopt");
    sb.create(&["--prefix", "p"]);
    let record = sb.records().remove(0);
    let output = sb.send(&["--prefix", "p"]);
    assert!(output.status.success(), "{}", text(&output));
    {
        let connection = sqlite::open(&sb.db).unwrap();
        connection.execute("DELETE FROM replicas").unwrap();
    }
    assert!(sb.replicas().is_empty());

    let output = sb.send(&["--prefix", "p"]);
    assert!(output.status.success(), "{}", text(&output));
    assert!(
        stdout(&output).contains(&format!(
            "{} is already present at {}",
            record.name,
            sb.target_str()
        )),
        "{}",
        text(&output)
    );
    assert_eq!(
        sb.log().matches("btrfs send").count(),
        1,
        "nothing must be sent again"
    );
    assert_eq!(sb.replicas().len(), 1);

    // A subvolume with the right name but the wrong content is replaced.
    fs::write(
        sb.target.join(&record.name).join(".received_uuid"),
        "not-the-source\n",
    )
    .unwrap();
    {
        let connection = sqlite::open(&sb.db).unwrap();
        connection.execute("DELETE FROM replicas").unwrap();
    }
    let output = sb.send(&["--prefix", "p"]);
    assert!(output.status.success(), "{}", text(&output));
    assert!(
        stderr(&output).contains("is not a complete replica"),
        "{}",
        text(&output)
    );
    assert_eq!(sb.log().matches("btrfs send").count(), 2);
    assert_replica_matches(&sb, &record.name);
}

#[test]
fn send_falls_back_to_an_older_parent_when_the_newest_is_gone_from_the_target() {
    let sb = Sandbox::new("send-parent-gone");
    sb.create(&["--prefix", "p"]);
    assert!(sb.send(&["--prefix", "p"]).status.success());
    sb.create(&["--prefix", "p"]);
    assert!(sb.send(&["--prefix", "p"]).status.success());
    let mut names: Vec<String> = sb.records().into_iter().map(|r| r.name).collect();
    names.sort();
    let (first, second) = (names[0].clone(), names[1].clone());
    fs::remove_dir_all(sb.target.join(&second)).unwrap();

    sb.create(&["--prefix", "p"]);
    let third = sb.records().into_iter().map(|r| r.name).max().unwrap();
    let output = sb.send(&["--prefix", "p"]);
    assert!(output.status.success(), "{}", text(&output));
    assert!(
        stderr(&output).contains(&format!("{second} is no longer present")),
        "{}",
        text(&output)
    );
    assert!(
        stdout(&output).contains(&format!("incremental from {first}")),
        "{}",
        text(&output)
    );
    assert!(
        sb.log().contains(&format!(
            "btrfs send -p {}/{first} {}/{third}\n",
            sb.snaps.display(),
            sb.snaps.display()
        )),
        "{}",
        sb.log()
    );
    let mut replicated: Vec<String> = sb.replicas().into_iter().map(|r| r.name).collect();
    replicated.sort();
    assert_eq!(replicated, [first.clone(), third.clone()]);

    // The forgotten one is pending again and gets re-sent on the next run.
    let output = sb.send(&["--prefix", "p"]);
    assert!(output.status.success(), "{}", text(&output));
    assert!(
        stdout(&output).contains(&format!("Sending {second}")),
        "{}",
        text(&output)
    );
    assert_eq!(sb.replicas().len(), 3);
    assert_replica_matches(&sb, &second);
}

#[test]
fn send_prunes_the_target_per_kind_with_keep_from_the_config() {
    let sb = Sandbox::new("send-prune");
    let config = sb.write(
        "config.toml",
        &format!(
            "dest_dir = \"{}\"\nsource_dir = \"{}\"\ndatabase_file = \"{}\"\nsnapshot_prefix = \"p\"\nmachine = \"test\"\n\n[[replicate]]\ntarget = \"{}\"\nkeep = 2\n",
            sb.snaps.display(),
            sb.src.display(),
            sb.db.display(),
            sb.target.display()
        ),
    );
    for _ in 0..3 {
        let output = sb.run(&["-c", &config, "--create", "--kind", "k"]);
        assert!(output.status.success(), "{}", text(&output));
    }
    let output = sb.run(&["-c", &config, "--create", "--kind", "other"]);
    assert!(output.status.success(), "{}", text(&output));
    let mut k_names: Vec<String> = sb
        .records()
        .into_iter()
        .filter(|r| r.kind == "k")
        .map(|r| r.name)
        .collect();
    k_names.sort();

    let output = sb.run(&["-c", &config, "--send", "--dry-run"]);
    assert!(output.status.success(), "{}", text(&output));
    assert_eq!(
        stdout(&output).matches("would send").count(),
        4,
        "{}",
        text(&output)
    );
    assert!(sb.replicas_on_disk().is_empty());

    let output = sb.run(&["-c", &config, "--send"]);
    assert!(output.status.success(), "{}", text(&output));
    let out = stdout(&output);
    assert_eq!(out.matches("Sent ").count(), 4, "{out}");
    assert!(
        out.contains(&format!(
            "Deleted replica {} at {} (keeping the last 2 'k' replicas)",
            k_names[0],
            sb.target_str()
        )),
        "{out}"
    );
    let mut expected: Vec<String> = k_names[1..].to_vec();
    expected.push(
        sb.records()
            .into_iter()
            .find(|r| r.kind == "other")
            .unwrap()
            .name,
    );
    expected.sort();
    assert_eq!(sb.replicas_on_disk(), expected);
    assert_eq!(sb.replicas().len(), 3);
    // Local snapshots are not touched by the remote retention.
    assert_eq!(sb.records().len(), 4);

    let output = sb.run(&["-c", &config, "--send"]);
    assert!(output.status.success(), "{}", text(&output));
    assert!(
        stdout(&output).contains("Nothing to send"),
        "{}",
        text(&output)
    );
    assert!(
        !stdout(&output).contains("Deleted replica"),
        "{}",
        text(&output)
    );
}

#[test]
fn send_requires_a_target_and_an_existing_target_directory() {
    let sb = Sandbox::new("send-errors");
    sb.create(&[]);
    let output = sb.run(&["--send", "-m", "test", "-d", sb.db_path()]);
    assert_failed(&output, "no replication target");
    let output = sb.run(&[
        "--send",
        "--target",
        "/nonexistent/target",
        "-m",
        "test",
        "-d",
        sb.db_path(),
    ]);
    assert_failed(&output, "does not exist");
    let output = sb.run(&[
        "--send",
        "--target",
        "relative/dir",
        "-m",
        "test",
        "-d",
        sb.db_path(),
    ]);
    assert_failed(&output, "unsupported replication target");
    assert!(sb.replicas().is_empty());
    assert!(!sb.log().contains("btrfs send"));
}

#[test]
fn list_shows_replicas() {
    let sb = Sandbox::new("list-replicas");
    sb.create(&["--prefix", "p"]);
    assert!(sb.send(&["--prefix", "p"]).status.success());
    let record = sb.records().remove(0);
    let output = sb.run(&["--list", "-d", sb.db_path()]);
    assert!(output.status.success(), "{}", text(&output));
    let out = stdout(&output);
    assert!(out.contains("Replicas:"), "{out}");
    assert!(out.contains(sb.target_str()), "{out}");
    assert_eq!(out.matches(&record.name).count(), 2, "{out}");
}

#[test]
fn send_syncs_the_target_filesystem_after_receiving() {
    let sb = Sandbox::new("send-sync");
    sb.create(&["--prefix", "p"]);
    let output = sb.send(&["--prefix", "p"]);
    assert!(output.status.success(), "{}", text(&output));
    let log = sb.log();
    let receive = log
        .find(&format!("btrfs receive {}\n", sb.target_str()))
        .unwrap();
    let sync = log
        .find(&format!("btrfs filesystem sync {}\n", sb.target_str()))
        .unwrap();
    assert!(receive < sync, "{log}");

    let url = format!("ssh://backup@nas{}", sb.target_str());
    sb.create(&["--prefix", "q"]);
    let output = sb.run(&[
        "--send",
        "--target",
        &url,
        "-m",
        "test",
        "-d",
        sb.db_path(),
        "--prefix",
        "q",
    ]);
    assert!(output.status.success(), "{}", text(&output));
    assert!(
        sb.log().contains(&format!(
            "ssh backup@nas: sudo -n btrfs filesystem sync {}\n",
            sb.target_str()
        )),
        "{}",
        sb.log()
    );
}

fn write_blob(path: &Path, mib: usize) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, vec![7u8; mib << 20]).unwrap();
}

/// Directories under `snaps/.staging` (one per distinct exclude list).
fn staging_dirs(sb: &Sandbox) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(sb.snaps.join(".staging")) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries.map(|e| e.unwrap().path()).collect();
    dirs.sort();
    dirs
}

fn exclude_config(sb: &Sandbox, target: &str, excludes: &str) -> String {
    sb.write(
        "config.toml",
        &format!(
            "dest_dir = \"{}\"\nsource_dir = \"{}\"\ndatabase_file = \"{}\"\nsnapshot_prefix = \"p\"\nmachine = \"test\"\n\n[[replicate]]\ntarget = \"{}\"\nexclude = [{}]\n",
            sb.snaps.display(),
            sb.src.display(),
            sb.db.display(),
            target,
            excludes
        ),
    )
}

#[test]
fn send_with_excludes_leaves_the_paths_out_of_the_replica() {
    let sb = Sandbox::new("send-exclude");
    write_blob(&sb.src.join("keep/a.bin"), 2);
    write_blob(&sb.src.join("cache/b.bin"), 3);
    let config = exclude_config(&sb, sb.target_str(), "\"cache\", \"missing/dir\"");

    assert!(sb.run(&["-c", &config, "--create"]).status.success());
    let first = sb.records().remove(0);
    let output = sb.run(&["-c", &config, "--send"]);
    assert!(output.status.success(), "{}", text(&output));
    let out = stdout(&output);
    assert!(out.contains("excluding 1 path(s), 3.0 MiB"), "{out}");
    assert!(
        out.contains("Sent ") && out.contains(": 2.0 MiB in"),
        "{out}"
    );

    let dirs = staging_dirs(&sb);
    assert_eq!(dirs.len(), 1, "{dirs:?}");
    let staging = dirs[0].join(&first.name);
    assert!(
        staging.join(".ro").exists(),
        "the filtered copy must be read-only"
    );
    assert!(!staging.join("cache").exists());
    assert!(staging.join("keep/a.bin").exists());
    // The snapshot itself is untouched.
    assert!(Path::new(&first.path()).join("cache/b.bin").exists());
    // The replica was sent from the filtered copy and is recorded as such.
    assert!(
        sb.log()
            .contains(&format!("btrfs send {}\n", staging.display())),
        "{}",
        sb.log()
    );
    let replicas = sb.replicas();
    assert_eq!(replicas[0].local_path, staging.to_str().unwrap());
    assert_eq!(
        read_trimmed(&sb.target.join(&first.name).join(".received_uuid")),
        read_trimmed(&staging.join(".uuid"))
    );
    let output = sb.run(&["--list", "-d", sb.db_path()]);
    assert!(
        stdout(&output).contains("FILTERED") && stdout(&output).contains("| yes"),
        "{}",
        text(&output)
    );

    // Incremental sends chain between filtered copies.
    write_blob(&sb.src.join("keep/c.bin"), 1);
    assert!(sb.run(&["-c", &config, "--create"]).status.success());
    let second = sb
        .records()
        .into_iter()
        .find(|r| r.name != first.name)
        .unwrap();
    let output = sb.run(&["-c", &config, "--send"]);
    assert!(output.status.success(), "{}", text(&output));
    assert!(
        stdout(&output).contains(&format!("incremental from {}", first.name)),
        "{}",
        text(&output)
    );
    let staging2 = dirs[0].join(&second.name);
    assert!(
        sb.log().contains(&format!(
            "btrfs send -p {} {}\n",
            staging.display(),
            staging2.display()
        )),
        "{}",
        sb.log()
    );
    assert!(!staging2.join("cache").exists());

    // Deleting a snapshot deletes its filtered copy.
    let output = sb.run(&[
        "--del",
        "--id",
        &first.snap_id,
        "-m",
        "test",
        "-d",
        sb.db_path(),
    ]);
    assert!(output.status.success(), "{}", text(&output));
    assert!(
        stdout(&output).contains("Deleted the filtered copy"),
        "{}",
        text(&output)
    );
    assert!(!staging.exists());
    assert!(staging2.exists());

    // Deleting the last snapshot of a list removes its now empty directory too.
    let output = sb.run(&[
        "--del",
        "--id",
        &second.snap_id,
        "-m",
        "test",
        "-d",
        sb.db_path(),
    ]);
    assert!(output.status.success(), "{}", text(&output));
    assert!(staging_dirs(&sb).is_empty(), "{:?}", staging_dirs(&sb));
}

#[test]
fn send_dry_run_reports_excludes_without_a_filtered_copy() {
    let sb = Sandbox::new("send-exclude-dry-run");
    write_blob(&sb.src.join("keep/a.bin"), 2);
    write_blob(&sb.src.join("cache/b.bin"), 3);
    sb.create(&["--prefix", "p"]);
    let output = sb.run(&[
        "--send",
        "--prefix",
        "p",
        "--target",
        sb.target_str(),
        "--exclude",
        "cache",
        "--exclude",
        "./keep/",
        "--dry-run",
        "-m",
        "test",
        "-d",
        sb.db_path(),
    ]);
    assert!(output.status.success(), "{}", text(&output));
    assert!(
        stdout(&output).contains("excluding 2 path(s), 5.0 MiB: cache, keep"),
        "{}",
        text(&output)
    );
    assert!(staging_dirs(&sb).is_empty());
    assert!(!sb.log().contains("property set"));
    assert!(sb.replicas().is_empty());
}

#[test]
fn exclude_entries_are_validated() {
    let sb = Sandbox::new("exclude-validation");
    sb.create(&["--prefix", "p"]);
    for bad in ["/abs/path", "../x", "a/../b", ".", ""] {
        let output = sb.run(&[
            "--send",
            "--target",
            sb.target_str(),
            "--exclude",
            bad,
            "-m",
            "test",
            "-d",
            sb.db_path(),
        ]);
        assert_failed(&output, "exclude");
    }
    let config = exclude_config(&sb, sb.target_str(), "\"/etc\"");
    let output = sb.run(&["-c", &config, "--send"]);
    assert_failed(&output, "exclude");
    assert!(sb.replicas().is_empty());
    assert!(!sb.log().contains("btrfs send"));
}

#[test]
fn send_with_excludes_over_ssh_via_flags() {
    let sb = Sandbox::new("send-exclude-ssh");
    write_blob(&sb.src.join("keep/a.bin"), 1);
    write_blob(&sb.src.join("cache/b.bin"), 1);
    sb.create(&["--prefix", "p"]);
    let record = sb.records().remove(0);
    let url = format!("ssh://backup@nas{}", sb.target_str());
    let output = sb.run(&[
        "--send",
        "--prefix",
        "p",
        "--target",
        &url,
        "--exclude",
        "cache",
        "-m",
        "test",
        "-d",
        sb.db_path(),
    ]);
    assert!(output.status.success(), "{}", text(&output));
    let staging = staging_dirs(&sb)[0].join(&record.name);
    assert_eq!(
        read_trimmed(&sb.target.join(&record.name).join(".received_uuid")),
        read_trimmed(&staging.join(".uuid"))
    );
    assert!(
        sb.log().contains(&format!(
            "ssh backup@nas: sudo -n btrfs receive {}\n",
            sb.target_str()
        )),
        "{}",
        sb.log()
    );
    assert_eq!(sb.replicas()[0].local_path, staging.to_str().unwrap());
}

#[test]
fn different_exclude_lists_get_their_own_filtered_copies() {
    let sb = Sandbox::new("send-exclude-multi");
    write_blob(&sb.src.join("keep/a.bin"), 1);
    write_blob(&sb.src.join("cache/b.bin"), 1);
    let t1 = sb.dir.join("t1");
    let t2 = sb.dir.join("t2");
    let t3 = sb.dir.join("t3");
    for t in [&t1, &t2, &t3] {
        fs::create_dir_all(t).unwrap();
    }
    let config = sb.write(
        "config.toml",
        &format!(
            "dest_dir = \"{}\"\nsource_dir = \"{}\"\ndatabase_file = \"{}\"\nsnapshot_prefix = \"p\"\nmachine = \"test\"\n\n[[replicate]]\ntarget = \"{}\"\nexclude = [\"cache\"]\n\n[[replicate]]\ntarget = \"{}\"\nexclude = [\"keep\"]\n\n[[replicate]]\ntarget = \"{}\"\nexclude = [\"cache/\"]\n",
            sb.snaps.display(), sb.src.display(), sb.db.display(), t1.display(), t2.display(), t3.display()
        ),
    );
    assert!(sb.run(&["-c", &config, "--create"]).status.success());
    let record = sb.records().remove(0);
    let output = sb.run(&["-c", &config, "--send"]);
    assert!(output.status.success(), "{}", text(&output));
    assert_eq!(
        stdout(&output).matches("Sent ").count(),
        3,
        "{}",
        text(&output)
    );
    assert!(
        stdout(&output).contains("filtered copy reused"),
        "{}",
        text(&output)
    );

    let dirs = staging_dirs(&sb);
    assert_eq!(dirs.len(), 2, "same list shares a copy: {dirs:?}");
    let replicas = sb.replicas();
    assert_eq!(replicas.len(), 3);
    let by_target = |t: &Path| {
        replicas
            .iter()
            .find(|r| r.target == t.to_str().unwrap())
            .unwrap()
    };
    assert_eq!(by_target(&t1).local_path, by_target(&t3).local_path);
    assert_ne!(by_target(&t1).local_path, by_target(&t2).local_path);
    assert!(!Path::new(&by_target(&t1).local_path).join("cache").exists());
    assert!(Path::new(&by_target(&t1).local_path).join("keep").exists());
    assert!(!Path::new(&by_target(&t2).local_path).join("keep").exists());
    assert!(Path::new(&by_target(&t2).local_path).join("cache").exists());
    for t in [&t1, &t2, &t3] {
        assert!(t.join(&record.name).exists());
    }
}

#[test]
fn an_incomplete_filtered_copy_is_rebuilt() {
    let sb = Sandbox::new("send-exclude-rebuild");
    write_blob(&sb.src.join("keep/a.bin"), 1);
    write_blob(&sb.src.join("cache/b.bin"), 1);
    sb.create(&["--prefix", "p"]);
    let record = sb.records().remove(0);
    assert!(
        sb.run(&[
            "--send",
            "--prefix",
            "p",
            "--target",
            sb.target_str(),
            "--exclude",
            "cache",
            "-m",
            "test",
            "-d",
            sb.db_path()
        ])
        .status
        .success()
    );
    let staging = staging_dirs(&sb)[0].join(&record.name);

    // Simulate a copy left half-built (never set read-only) and a lost replica record.
    fs::remove_file(staging.join(".ro")).unwrap();
    write_blob(&staging.join("cache/leftover.bin"), 1);
    fs::remove_dir_all(sb.target.join(&record.name)).unwrap();
    {
        let connection = sqlite::open(&sb.db).unwrap();
        connection.execute("DELETE FROM replicas").unwrap();
    }
    let output = sb.run(&[
        "--send",
        "--prefix",
        "p",
        "--target",
        sb.target_str(),
        "--exclude",
        "cache",
        "-m",
        "test",
        "-d",
        sb.db_path(),
    ]);
    assert!(output.status.success(), "{}", text(&output));
    assert!(
        stderr(&output).contains("incomplete filtered copy"),
        "{}",
        text(&output)
    );
    assert!(staging.join(".ro").exists());
    assert!(!staging.join("cache").exists());
    assert_eq!(staging_dirs(&sb).len(), 1);
    assert_eq!(sb.replicas().len(), 1);
}
