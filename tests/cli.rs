//! End-to-end tests. They run the real binary against a fake `btrfs` script that simulates
//! `subvolume create/snapshot/delete` with plain directories, so no root or btrfs filesystem
//! is needed.

use {
    rusnapshot::{database, structs::SnapshotRecord},
    std::{
        fs,
        os::unix::fs::PermissionsExt,
        path::{Path, PathBuf},
        process::{Command, Output},
    },
};

const FAKE_BTRFS: &str = r#"#!/usr/bin/env bash
# Fake btrfs: logs every invocation and simulates subvolumes with plain directories.
LOG="${FAKE_BTRFS_LOG:?}"
echo "btrfs $*" >> "$LOG"
if [ -n "$FAKE_BTRFS_FAIL" ]; then echo "fake btrfs: forced failure" >&2; exit 1; fi
sub="$1"; op="$2"; shift 2
case "$sub/$op" in
  subvolume/create)
    [ -d "$(dirname "$1")" ] || { echo "ERROR: cannot access '$1'" >&2; exit 1; }
    mkdir "$1" ;;
  subvolume/snapshot)
    pos=()
    for a in "$@"; do [ "$a" = "-r" ] || pos+=("$a"); done
    src="${pos[0]}"; dst="${pos[1]}"
    [ -d "$src" ] || { echo "ERROR: cannot access '$src'" >&2; exit 1; }
    if [ -d "$dst" ]; then dst="$dst/$(basename "$src")"; fi
    [ -d "$(dirname "$dst")" ] || { echo "ERROR: cannot access '$dst': No such file or directory" >&2; exit 1; }
    mkdir "$dst" ;;
  subvolume/delete)
    [ -d "$1" ] || { echo "ERROR: Not a Btrfs subvolume: $1" >&2; exit 1; }
    rm -rf "$1" ;;
  *) echo "fake btrfs: unsupported command: $sub $op $*" >&2; exit 1 ;;
esac
"#;

struct Sandbox {
    dir: PathBuf,
    src: PathBuf,
    snaps: PathBuf,
    db: PathBuf,
    log: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("rusnapshot-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("bin")).unwrap();
        fs::create_dir_all(dir.join("src")).unwrap();
        let fake = dir.join("bin/btrfs");
        fs::write(&fake, FAKE_BTRFS).unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
        Self {
            src: dir.join("src"),
            snaps: dir.join("snaps"),
            db: dir.join("snaps/db.sqlite"),
            log: dir.join("btrfs.log"),
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
            .filter(|n| !n.starts_with("db.sqlite"))
            .collect();
        names.sort();
        names
    }

    fn log(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
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
fn create_fails_loudly_when_btrfs_fails() {
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
    // With --dry-run there is simply nothing to show.
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
