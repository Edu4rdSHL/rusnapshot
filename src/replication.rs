//! Replication of snapshots to another disk or host with `btrfs send` / `btrfs receive`.
//!
//! A replication target is a directory on a btrfs filesystem, local (an external disk) or on
//! another machine through ssh. Every replica that completed and was verified is recorded in the
//! `replicas` table, which is what makes incremental sends possible: the parent of a send is the
//! newest replica of the same source subvolume that still exists on both sides.

use {
    crate::{
        args::{Args, ReplicateConfig},
        database, operations,
        structs::{ReplicaRecord, SnapshotRecord},
        utils::strip_trailing_slash,
    },
    anyhow::{Context, Result, anyhow, bail},
    sqlite::Connection,
    std::{
        ffi::CString,
        fs, io,
        io::{IsTerminal, Read, Write},
        os::{
            fd::{AsRawFd, FromRawFd, OwnedFd},
            unix::ffi::OsStrExt,
        },
        path::{Component, Path, PathBuf},
        process::{Command, Stdio},
        time::{Duration, Instant},
    },
};

/// `FS_IMMUTABLE_FL` from `linux/fs.h`: the file can't be written, deleted or renamed.
const FS_IMMUTABLE_FL: libc::c_int = 0x0000_0010;
/// `FS_APPEND_FL` from `linux/fs.h`: the file can only be opened for appending.
const FS_APPEND_FL: libc::c_int = 0x0000_0020;

/// Identity of a subvolume as reported by `btrfs subvolume show`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubvolumeInfo {
    pub uuid: String,
    /// UUID of the subvolume this one was received from, if any.
    pub received_uuid: Option<String>,
}

/// Where replicas go: a local directory or a directory on another host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Local {
        path: String,
    },
    Ssh {
        user_host: String,
        port: Option<u16>,
        path: String,
    },
}

impl Target {
    /// Parse an absolute path or an `ssh://[user@]host[:port]/path` URL.
    ///
    /// # Errors
    ///
    /// Fails if the value is neither of those.
    pub fn parse(url: &str) -> Result<Self> {
        if let Some(rest) = url.strip_prefix("ssh://") {
            let slash = rest.find('/').with_context(|| {
                format!("the ssh target '{url}' must include a path: ssh://[user@]host[:port]/path")
            })?;
            let (host_part, path) = rest.split_at(slash);
            let (user_host, port) = match host_part.rsplit_once(':') {
                Some((host, port)) if !port.is_empty() => {
                    let port = port
                        .parse::<u16>()
                        .with_context(|| format!("invalid port in the ssh target '{url}'"))?;
                    (host, Some(port))
                }
                _ => (host_part, None),
            };
            let path = strip_trailing_slash(path);
            if user_host.is_empty() || user_host.ends_with('@') || path == "/" {
                bail!("invalid ssh target '{url}': expected ssh://[user@]host[:port]/path");
            }
            return Ok(Self::Ssh {
                user_host: user_host.to_string(),
                port,
                path: path.to_string(),
            });
        }
        if url.starts_with('/') {
            let path = strip_trailing_slash(url);
            if path == "/" {
                bail!("the replication target can't be the root directory");
            }
            return Ok(Self::Local {
                path: path.to_string(),
            });
        }
        bail!(
            "unsupported replication target '{url}': use an absolute path or ssh://[user@]host[:port]/path"
        )
    }

    /// Directory where the replicas are stored at the target side.
    #[must_use]
    pub fn path(&self) -> &str {
        match self {
            Self::Local { path } | Self::Ssh { path, .. } => path,
        }
    }
}

/// A configured replication target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replication {
    /// The target as written in the configuration; it's the key used in the database.
    pub url: String,
    pub target: Target,
    /// Replicas to keep per kind at the target. `None` never deletes anything there.
    pub keep: Option<usize>,
    /// Extra options for `ssh`, such as `-i /root/.ssh/backup_key`.
    pub ssh_options: Vec<String>,
    /// Paths left out of the replicas, relative to the root of the source subvolume, normalized
    /// and sorted. Empty means the snapshots are sent as they are.
    pub exclude: Vec<String>,
}

/// Result of one send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transfer {
    pub bytes: u64,
    pub elapsed: Duration,
}

impl Replication {
    /// Build a replication from its configuration.
    ///
    /// # Errors
    ///
    /// Fails if the target can't be parsed or `keep` is zero.
    pub fn from_config(config: &ReplicateConfig) -> Result<Self> {
        let target = Target::parse(&config.target)?;
        if config.keep == Some(0) {
            bail!(
                "replicate.keep must be at least 1 for {} (the newest replica is the parent of the next incremental send)",
                config.target
            );
        }

        Ok(Self {
            url: config.target.clone(),
            target,
            keep: config.keep,
            ssh_options: config.ssh_options.clone(),
            exclude: normalize_excludes(&config.exclude)?,
        })
    }

    /// Program and arguments that run `command` at the target side.
    ///
    /// Locally the command runs as is (rusnapshot already runs as root). Over ssh, commands
    /// that need root (`as_root`) are run through `sudo -n`, so the ssh user must be able to
    /// sudo without a password; the rest run as the ssh user.
    #[must_use]
    pub fn command_line(&self, command: &[String], as_root: bool) -> Vec<String> {
        match &self.target {
            Target::Local { .. } => command.to_vec(),
            Target::Ssh {
                user_host, port, ..
            } => {
                let sudo = if as_root {
                    vec!["sudo".to_string(), "-n".to_string()]
                } else {
                    Vec::new()
                };
                let mut line = vec![
                    "ssh".to_string(),
                    "-o".to_string(),
                    "BatchMode=yes".to_string(),
                ];
                if let Some(port) = port {
                    line.push("-p".to_string());
                    line.push(port.to_string());
                }
                line.extend(self.ssh_options.iter().cloned());
                line.push(user_host.clone());
                line.push("--".to_string());
                let remote: Vec<String> = sudo
                    .into_iter()
                    .chain(command.iter().map(|arg| shell_quote(arg)))
                    .collect();
                line.push(remote.join(" "));

                line
            }
        }
    }

    fn command(&self, command: &[String], as_root: bool) -> Command {
        let line = self.command_line(command, as_root);
        let mut process = Command::new(&line[0]);
        process.args(&line[1..]);

        process
    }

    /// Path of the replica of `name` at the target.
    #[must_use]
    pub fn subvolume_path(&self, name: &str) -> String {
        format!("{}/{name}", self.target.path())
    }

    /// Whether `path` exists at the target.
    ///
    /// # Errors
    ///
    /// Fails if the target can't be reached.
    pub fn exists(&self, path: &str) -> Result<bool> {
        match &self.target {
            Target::Local { .. } => Ok(Path::new(path).exists()),
            Target::Ssh { .. } => {
                // Runs as the ssh user (no root needed to stat the directory). The exit code
                // of `test -e` alone can't be told apart from a broken ssh, so the result is
                // printed on stdout.
                let script = format!("test -e {} && echo yes || echo no", shell_quote(path));
                let output = self
                    .command(&strings(&["sh", "-c", &script]), false)
                    .stdin(Stdio::null())
                    .output()
                    .context("failed to execute 'ssh'")?;
                match String::from_utf8_lossy(&output.stdout).trim() {
                    "yes" => Ok(true),
                    "no" => Ok(false),
                    _ => bail!(
                        "could not check {path} at {}: {}",
                        self.url,
                        String::from_utf8_lossy(&output.stderr).trim()
                    ),
                }
            }
        }
    }

    /// `btrfs subvolume show` at the target.
    ///
    /// # Errors
    ///
    /// Fails if the command fails or its output can't be parsed.
    pub fn subvolume_info(&self, path: &str) -> Result<SubvolumeInfo> {
        let output = self
            .command(&strings(&["btrfs", "subvolume", "show", path]), true)
            .stdin(Stdio::null())
            .output()
            .with_context(|| format!("failed to run 'btrfs subvolume show' at {}", self.url))?;
        if !output.status.success() {
            bail!(
                "'btrfs subvolume show {path}' failed at {}: {}",
                self.url,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }

        parse_subvolume_show(&String::from_utf8_lossy(&output.stdout))
    }

    /// `btrfs filesystem sync` at the target: writes the received data to disk before the
    /// replica is recorded.
    ///
    /// # Errors
    ///
    /// Fails if the command fails.
    pub fn sync_filesystem(&self) -> Result<()> {
        let status = self
            .command(
                &strings(&["btrfs", "filesystem", "sync", self.target.path()]),
                true,
            )
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .status()
            .with_context(|| format!("failed to run 'btrfs filesystem sync' at {}", self.url))?;
        if !status.success() {
            bail!(
                "'btrfs filesystem sync {}' failed at {} with {status}",
                self.target.path(),
                self.url
            );
        }

        Ok(())
    }

    /// `btrfs subvolume delete` at the target.
    ///
    /// # Errors
    ///
    /// Fails if the command fails.
    pub fn delete_subvolume(&self, path: &str) -> Result<()> {
        let status = self
            .command(&strings(&["btrfs", "subvolume", "delete", path]), true)
            .stdin(Stdio::null())
            .status()
            .with_context(|| format!("failed to run 'btrfs subvolume delete' at {}", self.url))?;
        if !status.success() {
            bail!(
                "'btrfs subvolume delete {path}' failed at {} with {status}",
                self.url
            );
        }

        Ok(())
    }
}

/// Quote an argument for a POSIX shell.
#[must_use]
pub fn shell_quote(arg: &str) -> String {
    let safe = |c: char| c.is_ascii_alphanumeric() || "-_./:=@%+,".contains(c);
    if !arg.is_empty() && arg.chars().all(safe) {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', "'\\''"))
    }
}

/// Extract the UUIDs from the output of `btrfs subvolume show`.
///
/// # Errors
///
/// Fails if the output has no `UUID:` line.
pub fn parse_subvolume_show(output: &str) -> Result<SubvolumeInfo> {
    let mut uuid = None;
    let mut received_uuid = None;
    for line in output.lines() {
        if let Some((key, value)) = line.split_once(':') {
            let value = value.trim();
            match key.trim() {
                "UUID" => uuid = Some(value.to_string()),
                "Received UUID" => {
                    received_uuid = (!value.is_empty() && value != "-").then(|| value.to_string());
                }
                _ => {}
            }
        }
    }

    Ok(SubvolumeInfo {
        uuid: uuid.context("could not find the UUID in the output of 'btrfs subvolume show'")?,
        received_uuid,
    })
}

/// `btrfs subvolume show` of a local subvolume.
///
/// # Errors
///
/// Fails if the command fails or its output can't be parsed.
pub fn local_subvolume_info(path: &str) -> Result<SubvolumeInfo> {
    let output = Command::new("btrfs")
        .args(["subvolume", "show", path])
        .stdin(Stdio::null())
        .output()
        .context("failed to execute 'btrfs', make sure btrfs-progs is installed and in PATH")?;
    if !output.status.success() {
        bail!(
            "'btrfs subvolume show {path}' failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    parse_subvolume_show(&String::from_utf8_lossy(&output.stdout))
}

/// Stream `btrfs send [-p parent] snapshot` into `btrfs receive` at the target, reporting
/// progress on stderr when it is a terminal, then sync the target filesystem so the reported
/// time covers the data actually reaching the medium.
///
/// # Errors
///
/// Fails if either side fails or the pipe breaks.
pub fn send_and_receive(
    snapshot: &str,
    parent: Option<&str>,
    replication: &Replication,
) -> Result<Transfer> {
    let mut send_args = vec!["send".to_string()];
    if let Some(parent) = parent {
        send_args.push("-p".to_string());
        send_args.push(parent.to_string());
    }
    send_args.push(snapshot.to_string());

    let mut send = Command::new("btrfs")
        .args(&send_args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .context("failed to execute 'btrfs send'")?;
    let mut receive = match replication
        .command(
            &strings(&["btrfs", "receive", replication.target.path()]),
            true,
        )
        .stdin(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(err) => {
            let _ = send.kill();
            let _ = send.wait();
            return Err(err).with_context(|| {
                format!("failed to start 'btrfs receive' at {}", replication.url)
            });
        }
    };

    let mut output = send.stdout.take().context("no stdout from 'btrfs send'")?;
    let mut input = receive
        .stdin
        .take()
        .context("no stdin for 'btrfs receive'")?;
    let mut buffer = vec![0u8; 1 << 20];
    let mut bytes = 0u64;
    let started = Instant::now();
    let mut last_report = started;
    let interactive = std::io::stderr().is_terminal();
    let mut read_error = None;
    let mut write_error = None;

    loop {
        let read = match output.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(err) => {
                read_error = Some(anyhow!(err).context("failed to read from 'btrfs send'"));
                break;
            }
        };
        if let Err(err) = input.write_all(&buffer[..read]) {
            write_error = Some(anyhow!(err).context("failed to write to 'btrfs receive'"));
            break;
        }
        bytes += read as u64;
        if interactive && last_report.elapsed() >= Duration::from_secs(1) {
            let rate = bytes as f64 / started.elapsed().as_secs_f64();
            eprint!(
                "\r  {} sent, {}/s      ",
                human_size(bytes),
                human_size(rate as u64)
            );
            last_report = Instant::now();
        }
    }
    drop(input);
    if interactive && bytes > 0 {
        eprint!("\r{}\r", " ".repeat(60));
    }
    if read_error.is_some() || write_error.is_some() {
        let _ = send.kill();
    }
    let send_status = send.wait().context("failed to wait for 'btrfs send'")?;
    let receive_status = receive
        .wait()
        .context("failed to wait for 'btrfs receive'")?;

    // When the receiving side died the send failure is only a consequence, report the cause.
    if write_error.is_some() && !receive_status.success() {
        bail!(
            "'btrfs receive' at {} failed with {receive_status}",
            replication.url
        );
    }
    if !send_status.success() {
        bail!("'btrfs {}' failed with {send_status}", send_args.join(" "));
    }
    if !receive_status.success() {
        bail!(
            "'btrfs receive' at {} failed with {receive_status}",
            replication.url
        );
    }
    if let Some(err) = write_error.or(read_error) {
        return Err(err);
    }
    if let Err(err) = replication.sync_filesystem() {
        eprintln!("Warning: {err:#}. The replica is complete but may still be in the page cache.");
    }

    Ok(Transfer {
        bytes,
        elapsed: started.elapsed(),
    })
}

/// Replicate the pending snapshots of this prefix and machine to every configured target, then
/// apply the remote retention of the targets that define `keep`.
///
/// # Errors
///
/// Fails if there are no targets or replication to any target failed (the others are still
/// processed).
pub fn manage_sending(args: &Args, connection: &Connection) -> Result<()> {
    if args.replicate.is_empty() {
        bail!(
            "no replication target: add a [[replicate]] section to the configuration file or pass --target"
        );
    }
    let pending = database::pending_replication(connection, &args.snapshot_prefix, &args.machine)?;

    let mut failures = 0;
    for config in &args.replicate {
        let replication = Replication::from_config(config)?;
        if let Err(err) = replicate_to(args, connection, &replication, &pending) {
            failures += 1;
            eprintln!("Error: {err:#}");
        }
    }
    if failures > 0 {
        bail!("replication to {failures} target(s) failed");
    }

    Ok(())
}

fn replicate_to(
    args: &Args,
    connection: &Connection,
    replication: &Replication,
    pending: &[SnapshotRecord],
) -> Result<()> {
    println!(
        "Replicating ro snapshots with prefix '{}' and machine '{}' to {}",
        args.snapshot_prefix, args.machine, replication.url
    );
    let done = database::replicated_names(connection, &replication.url)?;
    let todo: Vec<&SnapshotRecord> = pending
        .iter()
        .filter(|snapshot| !done.contains(&snapshot.name))
        .collect();

    if !args.dry_run && !replication.exists(replication.target.path())? {
        bail!(
            "the directory {} does not exist at {} (is the backup disk mounted?)",
            replication.target.path(),
            replication.url
        );
    }

    if todo.is_empty() {
        println!(
            "Nothing to send to {}: all {} snapshot(s) are already replicated",
            replication.url,
            pending.len()
        );
    }
    for snapshot in todo {
        let path = snapshot.path();
        if !Path::new(&path).exists() {
            eprintln!(
                "Warning: skipping {}, it no longer exists at {path}",
                snapshot.name
            );
            continue;
        }
        if args.dry_run {
            let parent = database::parent_candidates(
                connection,
                &replication.url,
                &snapshot.source,
                &snapshot.machine,
            )?
            .into_iter()
            .next();
            let filter = if replication.exclude.is_empty() {
                String::new()
            } else {
                match measure_excludes(&path, &replication.exclude) {
                    Ok(excluded) => format!(
                        ", excluding {} path(s), {}: {}",
                        excluded.paths,
                        human_size(excluded.bytes),
                        replication.exclude.join(", ")
                    ),
                    Err(err) => format!(
                        ", excluding {} (could not measure: {err:#})",
                        replication.exclude.join(", ")
                    ),
                }
            };
            println!(
                "[dry-run] would send {} to {} ({}{filter})",
                snapshot.name,
                replication.url,
                describe_parent(parent.as_ref())
            );
            continue;
        }
        replicate_one(connection, replication, snapshot, &path)?;
    }

    if let Some(keep) = replication.keep {
        prune_target(args, connection, replication, keep)?;
    }

    Ok(())
}

fn describe_parent(parent: Option<&ReplicaRecord>) -> String {
    parent.map_or_else(
        || "full send".to_string(),
        |parent| format!("incremental from {}", parent.name),
    )
}

fn replicate_one(
    connection: &Connection,
    replication: &Replication,
    snapshot: &SnapshotRecord,
    path: &str,
) -> Result<()> {
    // With excludes, what travels is a filtered copy of the snapshot, not the snapshot itself.
    let (send_path, excluded) = if replication.exclude.is_empty() {
        (path.to_string(), None)
    } else {
        prepare_staging(snapshot, replication)?
    };
    let source = local_subvolume_info(&send_path)?;
    let remote_path = replication.subvolume_path(&snapshot.name);

    if replication.exists(&remote_path)? {
        let existing = replication.subvolume_info(&remote_path)?;
        if existing.received_uuid.as_deref() == Some(source.uuid.as_str()) {
            println!(
                "{} is already present at {}, recording it",
                snapshot.name, replication.url
            );
            database::insert_replica(connection, &record(snapshot, &send_path, replication, None))?;
            return Ok(());
        }
        eprintln!(
            "Warning: {remote_path} at {} is not a complete replica of {}, replacing it",
            replication.url, snapshot.name
        );
        replication.delete_subvolume(&remote_path)?;
    }

    let parent = choose_parent(connection, replication, snapshot)?;
    let mode = describe_parent(parent.as_ref());
    let filter = describe_excluded(replication, excluded);
    println!(
        "Sending {} to {} ({mode}{filter})",
        snapshot.name, replication.url
    );

    let transfer = match send_and_receive(
        &send_path,
        parent.as_ref().map(|parent| parent.local_path.as_str()),
        replication,
    ) {
        Ok(transfer) => transfer,
        Err(err) => {
            remove_partial(replication, &remote_path);
            return Err(err).with_context(|| {
                format!("failed to send {} to {}", snapshot.name, replication.url)
            });
        }
    };

    let received = replication
        .subvolume_info(&remote_path)
        .with_context(|| format!("could not verify {} at {}", snapshot.name, replication.url))?;
    if received.received_uuid.as_deref() != Some(source.uuid.as_str()) {
        remove_partial(replication, &remote_path);
        bail!(
            "verification of {} at {} failed: received UUID {} does not match the source UUID {}",
            snapshot.name,
            replication.url,
            received.received_uuid.as_deref().unwrap_or("-"),
            source.uuid
        );
    }

    database::insert_replica(
        connection,
        &record(
            snapshot,
            &send_path,
            replication,
            parent.map(|parent| parent.name),
        ),
    )?;
    let seconds = transfer.elapsed.as_secs_f64().max(0.001);
    println!(
        "Sent {} to {}: {} in {seconds:.1}s ({}/s), {mode}{filter}",
        snapshot.name,
        replication.url,
        human_size(transfer.bytes),
        human_size((transfer.bytes as f64 / seconds) as u64)
    );

    Ok(())
}

/// Newest replica of the same source subvolume that still exists locally and at the target.
/// Replicas that no longer exist at the target are removed from the database while looking.
fn choose_parent(
    connection: &Connection,
    replication: &Replication,
    snapshot: &SnapshotRecord,
) -> Result<Option<ReplicaRecord>> {
    let candidates = database::parent_candidates(
        connection,
        &replication.url,
        &snapshot.source,
        &snapshot.machine,
    )?;
    for candidate in candidates {
        if candidate.name == snapshot.name || !Path::new(&candidate.local_path).exists() {
            continue;
        }
        if !replication.exists(&replication.subvolume_path(&candidate.name))? {
            eprintln!(
                "Warning: {} is no longer present at {}, forgetting that replica",
                candidate.name, replication.url
            );
            database::delete_replica(connection, &candidate)?;
            continue;
        }
        return Ok(Some(candidate));
    }

    Ok(None)
}

/// Best effort removal of a replica that didn't complete.
fn remove_partial(replication: &Replication, remote_path: &str) {
    match replication.exists(remote_path) {
        Ok(true) => {
            eprintln!(
                "Removing the incomplete replica {remote_path} at {}",
                replication.url
            );
            if let Err(err) = replication.delete_subvolume(remote_path) {
                eprintln!(
                    "Error: {err:#}. Remove it by hand before the next run or it will be replaced then."
                );
            }
        }
        Ok(false) => {}
        Err(err) => eprintln!("Error: {err:#}"),
    }
}

/// Delete, per kind, the replicas beyond the newest `keep` ones at the target. They stay
/// recorded as pruned so the local snapshots are not sent there again.
fn prune_target(
    args: &Args,
    connection: &Connection,
    replication: &Replication,
    keep: usize,
) -> Result<()> {
    let kinds = database::replica_kinds(
        connection,
        &replication.url,
        &args.snapshot_prefix,
        &args.machine,
    )?;
    for kind in kinds {
        let candidates = database::remote_prune_candidates(
            connection,
            &replication.url,
            &args.snapshot_prefix,
            &kind,
            &args.machine,
            keep,
        )?;
        for replica in candidates {
            let remote_path = replication.subvolume_path(&replica.name);
            if args.dry_run {
                println!(
                    "[dry-run] would delete {remote_path} at {} (keeping the last {keep} '{kind}' replicas)",
                    replication.url
                );
                continue;
            }
            if replication.exists(&remote_path)? {
                replication.delete_subvolume(&remote_path)?;
            } else {
                eprintln!(
                    "Warning: {remote_path} was already gone from {}",
                    replication.url
                );
            }
            database::mark_replica_pruned(connection, &replica)?;
            println!(
                "Deleted replica {} at {} (keeping the last {keep} '{kind}' replicas)",
                replica.name, replication.url
            );
        }
    }

    Ok(())
}

/// Normalize the `exclude` entries of a target: relative paths inside the snapshot, without
/// leading `./`, trailing `/`, `.` or `..` components, deduplicated and sorted.
///
/// # Errors
///
/// Fails on an empty, absolute or escaping entry.
pub fn normalize_excludes(excludes: &[String]) -> Result<Vec<String>> {
    let mut normalized: Vec<String> = Vec::new();
    for raw in excludes {
        let mut parts: Vec<String> = Vec::new();
        for component in Path::new(raw.trim()).components() {
            match component {
                Component::CurDir => {}
                Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
                _ => bail!(
                    "exclude '{raw}' must be a relative path inside the snapshot, without '..'"
                ),
            }
        }
        if parts.is_empty() {
            bail!("exclude entries must be paths inside the snapshot, got '{raw}'");
        }
        let entry = parts.join("/");
        if !normalized.contains(&entry) {
            normalized.push(entry);
        }
    }
    normalized.sort();

    Ok(normalized)
}

/// Short id of an exclude list, used to keep one filtered copy per distinct list.
#[must_use]
pub fn exclude_hash(excludes: &[String]) -> String {
    format!("{:x}", md5::compute(excludes.join("\n")))[..8].to_string()
}

/// Directory under the snapshots destination holding the filtered copies.
#[must_use]
pub fn staging_root(destination: &str) -> String {
    format!("{}/.staging", strip_trailing_slash(destination))
}

/// Path of the filtered copy of `name` for an exclude list.
#[must_use]
pub fn staging_path(destination: &str, excludes: &[String], name: &str) -> String {
    format!(
        "{}/{}/{name}",
        staging_root(destination),
        exclude_hash(excludes)
    )
}

/// What an exclude list removed (or would remove) from a snapshot.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Excluded {
    /// Entries that existed in the snapshot.
    pub paths: usize,
    /// Apparent size of the files under them.
    pub bytes: u64,
}

/// Size of the excluded paths inside a snapshot, touching nothing (for `--dry-run`).
///
/// # Errors
///
/// Fails if the tree can't be read.
pub fn measure_excludes(snapshot_path: &str, excludes: &[String]) -> Result<Excluded> {
    let mut excluded = Excluded::default();
    for entry in excludes {
        if let Some(path) = locate_exclude(snapshot_path, entry)? {
            excluded.paths += 1;
            excluded.bytes += tree_size(&path)?;
        }
    }

    Ok(excluded)
}

/// Locate an exclude entry inside a snapshot or filtered copy. The last component is not
/// followed (a symlink is removed as a link), and the entry is refused when its parent resolves
/// outside the root: a symlink inside the snapshot may point at the live system, and deleting
/// through it would destroy live data. Returns `None` when the entry does not exist.
fn locate_exclude(root: &str, entry: &str) -> Result<Option<PathBuf>> {
    let full = Path::new(root).join(entry);
    if full.symlink_metadata().is_err() {
        return Ok(None);
    }
    let real_root = Path::new(root)
        .canonicalize()
        .with_context(|| format!("failed to resolve {root}"))?;
    let parent = full.parent().context("exclude entry without a parent")?;
    let real_parent = parent
        .canonicalize()
        .with_context(|| format!("failed to resolve {}", parent.display()))?;
    if !real_parent.starts_with(&real_root) {
        bail!(
            "exclude '{entry}' resolves outside the snapshot ({}), refusing to touch it",
            real_parent.display()
        );
    }
    let name = full.file_name().context("exclude entry without a name")?;

    Ok(Some(real_parent.join(name)))
}

fn tree_size(path: &Path) -> Result<u64> {
    let metadata = path
        .symlink_metadata()
        .with_context(|| format!("failed to read {}", path.display()))?;
    if metadata.is_dir() {
        let mut total = 0;
        for entry in
            fs::read_dir(path).with_context(|| format!("failed to read {}", path.display()))?
        {
            total += tree_size(&entry?.path())?;
        }
        Ok(total)
    } else if metadata.is_file() {
        Ok(metadata.len())
    } else {
        Ok(0)
    }
}

/// Delete the excluded paths inside a filtered copy, returning what was removed. Entries that
/// don't exist in the snapshot are ignored. Sizes come from a read-only walk; the deletion itself
/// is done by the standard library, whose `remove_dir_all` never follows symlinks and is immune
/// to a directory being swapped for a symlink while it runs.
///
/// # Errors
///
/// Fails if something can't be removed or an entry resolves outside the copy.
pub fn remove_excludes(staging: &str, excludes: &[String]) -> Result<Excluded> {
    let mut excluded = Excluded::default();
    for entry in excludes {
        if let Some(path) = locate_exclude(staging, entry)? {
            excluded.paths += 1;
            excluded.bytes += tree_size(&path)?;
            let metadata = path
                .symlink_metadata()
                .with_context(|| format!("failed to read {}", path.display()))?;
            remove_excluded(&path, metadata.is_dir())
                .with_context(|| format!("failed to remove {}", path.display()))?;
        }
    }

    Ok(excluded)
}

/// Remove one excluded entry. The fast path is the standard library, which handles every ordinary
/// tree; when something below carries the immutable or append-only attribute the kernel refuses
/// the `unlink` with `EPERM` even for root, and the tree is walked again clearing the attribute
/// from whatever refuses to go away.
///
/// Clearing only ever happens inside the throwaway read-write copy under `.staging`: the snapshot
/// it was made from is read-only and the live filesystem is never reached, so a file protected
/// with `chattr +i` keeps its attribute everywhere it matters.
fn remove_excluded(path: &Path, is_dir: bool) -> io::Result<()> {
    let removed = if is_dir {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    };
    match removed {
        Err(err) if err.kind() == io::ErrorKind::PermissionDenied => force_remove(path),
        other => other,
    }
}

/// Remove `path` and everything under it, clearing attributes as needed. Symlinks are removed as
/// links: the type comes from `symlink_metadata`, so the walk never descends through one.
fn force_remove(path: &Path) -> io::Result<()> {
    let metadata = match path.symlink_metadata() {
        Ok(metadata) => metadata,
        // `remove_dir_all` deletes as it goes, so part of the tree is already gone.
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    if metadata.is_dir() {
        for entry in fs::read_dir(path)? {
            force_remove(&entry?.path())?;
        }
    }

    unlink(path, metadata.is_dir())
}

/// `unlink` or `rmdir`, retried once with the immutable and append-only attributes cleared from
/// the entry and from the directory holding it: either of the two is enough to stop a removal,
/// and an immutable directory is what stops an otherwise ordinary file from going away. Only the
/// paths that really carried an attribute are reported, and a failure to clear is ignored so the
/// caller sees the original removal error rather than a derived one.
fn unlink(path: &Path, is_dir: bool) -> io::Result<()> {
    let remove = || {
        if is_dir {
            fs::remove_dir(path)
        } else {
            fs::remove_file(path)
        }
    };
    match remove() {
        Err(err) if err.kind() == io::ErrorKind::PermissionDenied => {
            let mut cleared: Vec<&Path> = Vec::new();
            if clear_attributes(path).unwrap_or(false) {
                cleared.push(path);
            }
            if let Some(parent) = path.parent()
                && clear_attributes(parent).unwrap_or(false)
            {
                cleared.push(parent);
            }
            let result = remove();
            if result.is_ok() {
                for cleared in cleared {
                    eprintln!(
                        "Warning: cleared the immutable/append-only attribute of {} in the filtered copy",
                        cleared.display()
                    );
                }
            }
            result
        }
        other => other,
    }
}

/// Clear `FS_IMMUTABLE_FL` and `FS_APPEND_FL` on a path, reporting whether either of them was
/// actually set: the caller uses that to name what it really touched. `O_NOFOLLOW` means a
/// symlink inside the copy can never redirect this at something else.
fn clear_attributes(path: &Path) -> io::Result<bool> {
    let name = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    // SAFETY: `name` is a valid NUL-terminated string that outlives the call.
    let raw = unsafe {
        libc::open(
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `raw` was just opened here and is owned by nothing else.
    let file = unsafe { OwnedFd::from_raw_fd(raw) };

    let mut flags: libc::c_int = 0;
    // SAFETY: the descriptor is open and the kernel writes one `int` through the pointer.
    if unsafe { libc::ioctl(file.as_raw_fd(), libc::FS_IOC_GETFLAGS, &raw mut flags) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let wanted = flags & !(FS_IMMUTABLE_FL | FS_APPEND_FL);
    if wanted == flags {
        return Ok(false);
    }
    // SAFETY: the descriptor is open and the kernel reads one `int` through the pointer.
    if unsafe { libc::ioctl(file.as_raw_fd(), libc::FS_IOC_SETFLAGS, &raw const wanted) } < 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(true)
}

/// Build (or reuse) the filtered copy of a snapshot for a target: a read-write snapshot of the
/// read-only snapshot, minus the excluded paths, set read-only so it can be sent. Returns its
/// path and what was removed (`None` when an existing copy is reused).
///
/// # Errors
///
/// Fails if any btrfs command or deletion fails; a half-built copy is removed again.
pub fn prepare_staging(
    snapshot: &SnapshotRecord,
    replication: &Replication,
) -> Result<(String, Option<Excluded>)> {
    let staging = staging_path(&snapshot.destination, &replication.exclude, &snapshot.name);
    if Path::new(&staging).exists() {
        if is_read_only(&staging)? {
            return Ok((staging, None));
        }
        eprintln!("Warning: removing the incomplete filtered copy {staging}");
        operations::del_snapshot(&staging)?;
    }
    if let Some(parent) = Path::new(&staging).parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    operations::run_btrfs(&strings(&[
        "subvolume",
        "snapshot",
        &snapshot.path(),
        &staging,
    ]))?;
    let excluded = match remove_excludes(&staging, &replication.exclude) {
        Ok(excluded) => excluded,
        Err(err) => {
            let _ = operations::del_snapshot(&staging);
            return Err(err)
                .with_context(|| format!("failed to build the filtered copy {staging}"));
        }
    };
    if let Err(err) = operations::run_btrfs(&strings(&[
        "property", "set", "-ts", &staging, "ro", "true",
    ])) {
        let _ = operations::del_snapshot(&staging);
        return Err(err);
    }

    Ok((staging, Some(excluded)))
}

fn is_read_only(path: &str) -> Result<bool> {
    let output = Command::new("btrfs")
        .args(["property", "get", "-ts", path, "ro"])
        .stdin(Stdio::null())
        .output()
        .context("failed to execute 'btrfs', make sure btrfs-progs is installed and in PATH")?;
    if !output.status.success() {
        bail!(
            "'btrfs property get {path} ro' failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    Ok(String::from_utf8_lossy(&output.stdout).contains("ro=true"))
}

fn describe_excluded(replication: &Replication, excluded: Option<Excluded>) -> String {
    if replication.exclude.is_empty() {
        return String::new();
    }
    match excluded {
        Some(excluded) => format!(
            ", excluding {} path(s), {}",
            excluded.paths,
            human_size(excluded.bytes)
        ),
        None => ", filtered copy reused".to_string(),
    }
}

fn record(
    snapshot: &SnapshotRecord,
    path: &str,
    replication: &Replication,
    parent_name: Option<String>,
) -> ReplicaRecord {
    ReplicaRecord {
        name: snapshot.name.clone(),
        snap_id: snapshot.snap_id.clone(),
        target: replication.url.clone(),
        local_path: path.to_string(),
        source: snapshot.source.clone(),
        kind: snapshot.kind.clone(),
        machine: snapshot.machine.clone(),
        snapshot_date: snapshot.date.clone(),
        parent_name,
        date: String::new(),
    }
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_string()).collect()
}

/// Bytes as a short human readable size.
#[must_use]
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use {
        super::{Replication, Target, human_size, parse_subvolume_show, shell_quote},
        crate::args::ReplicateConfig,
    };

    const SHOW: &str = "@.snapshots/root-2026-08-29-06-10-15-049553
\tName: \t\t\troot-2026-08-29-06-10-15-049553
\tUUID: \t\t\t4062b04e-39b6-4d43-99c2-2df9ad015417
\tParent UUID: \t\t2054d50a-7a82-3f41-9dca-25c7b21adc26
\tReceived UUID: \t\t-
\tCreation time: \t\t2026-08-29 01:10:15 -0500
\tFlags: \t\t\treadonly
";

    #[test]
    fn parse_targets() {
        assert_eq!(
            Target::parse("/mnt/usb/backups/").unwrap(),
            Target::Local {
                path: "/mnt/usb/backups".into()
            }
        );
        assert_eq!(
            Target::parse("ssh://backup@nas:2222/srv/backups/behemoth").unwrap(),
            Target::Ssh {
                user_host: "backup@nas".into(),
                port: Some(2222),
                path: "/srv/backups/behemoth".into()
            }
        );
        assert_eq!(
            Target::parse("ssh://nas/srv/backups").unwrap(),
            Target::Ssh {
                user_host: "nas".into(),
                port: None,
                path: "/srv/backups".into()
            }
        );
        for bad in [
            "",
            "relative/dir",
            "/",
            "ssh://nas",
            "ssh://nas/",
            "ssh:///srv",
            "ssh://nas:abc/srv",
            "ssh://user@/srv",
            "http://nas/srv",
        ] {
            assert!(Target::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn from_config_rejects_keep_zero() {
        let config = ReplicateConfig {
            target: "/mnt/usb".into(),
            keep: Some(0),
            ..ReplicateConfig::default()
        };
        assert!(Replication::from_config(&config).is_err());
    }

    fn replication(target: &str, ssh_options: &[&str]) -> Replication {
        Replication::from_config(&ReplicateConfig {
            target: target.into(),
            keep: None,
            ssh_options: ssh_options.iter().map(|o| (*o).to_string()).collect(),
            exclude: Vec::new(),
        })
        .unwrap()
    }

    fn cmd(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| (*v).to_string()).collect()
    }

    #[test]
    fn command_lines() {
        // Local targets: rusnapshot is already root, nothing is added.
        let local = replication("/mnt/usb", &[]);
        assert_eq!(
            local.command_line(&cmd(&["btrfs", "receive", "/mnt/usb"]), true),
            ["btrfs", "receive", "/mnt/usb"]
        );
        assert_eq!(
            local.command_line(&cmd(&["sh", "-c", "test -e /mnt/usb"]), false),
            ["sh", "-c", "test -e /mnt/usb"]
        );

        // Over ssh, root commands go through `sudo -n`, the rest run as the ssh user.
        let ssh = replication("ssh://backup@nas:2222/srv/b", &["-i", "/root/.ssh/k"]);
        assert_eq!(
            ssh.command_line(&cmd(&["btrfs", "receive", "/srv/b"]), true),
            [
                "ssh",
                "-o",
                "BatchMode=yes",
                "-p",
                "2222",
                "-i",
                "/root/.ssh/k",
                "backup@nas",
                "--",
                "sudo -n btrfs receive /srv/b"
            ]
        );
        assert_eq!(
            ssh.command_line(
                &cmd(&["sh", "-c", "test -e /srv/b && echo yes || echo no"]),
                false
            ),
            [
                "ssh",
                "-o",
                "BatchMode=yes",
                "-p",
                "2222",
                "-i",
                "/root/.ssh/k",
                "backup@nas",
                "--",
                "sh -c 'test -e /srv/b && echo yes || echo no'"
            ]
        );
        let ssh = replication("ssh://nas/srv/my backups", &[]);
        assert_eq!(
            ssh.command_line(
                &cmd(&["btrfs", "subvolume", "show", "/srv/my backups/it's"]),
                true
            ),
            [
                "ssh",
                "-o",
                "BatchMode=yes",
                "nas",
                "--",
                "sudo -n btrfs subvolume show '/srv/my backups/it'\\''s'"
            ]
        );
        assert_eq!(ssh.subvolume_path("x"), "/srv/my backups/x");
    }

    #[test]
    fn quoting() {
        assert_eq!(
            shell_quote("/srv/backups/root-2026"),
            "/srv/backups/root-2026"
        );
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("$HOME"), "'$HOME'");
    }

    #[test]
    fn parse_show_output() {
        let info = parse_subvolume_show(SHOW).unwrap();
        assert_eq!(info.uuid, "4062b04e-39b6-4d43-99c2-2df9ad015417");
        assert_eq!(info.received_uuid, None);

        let received = SHOW.replace(
            "Received UUID: \t\t-",
            "Received UUID: \t\t4062b04e-39b6-4d43-99c2-2df9ad015417",
        );
        let info = parse_subvolume_show(&received).unwrap();
        assert_eq!(
            info.received_uuid.as_deref(),
            Some("4062b04e-39b6-4d43-99c2-2df9ad015417")
        );
        assert!(parse_subvolume_show("garbage").is_err());
    }

    #[test]
    fn sizes() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1024), "1.0 KiB");
        assert_eq!(human_size(1_572_864), "1.5 MiB");
        assert_eq!(human_size(3 << 30), "3.0 GiB");
    }
}

#[cfg(test)]
mod exclude_tests {
    use {
        super::{
            FS_IMMUTABLE_FL, clear_attributes, exclude_hash, measure_excludes, normalize_excludes,
            remove_excludes, staging_path,
        },
        std::{
            ffi::CString,
            os::{
                fd::{AsRawFd, FromRawFd, OwnedFd},
                unix::ffi::OsStrExt,
            },
            path::Path,
        },
    };

    fn list(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| (*v).to_string()).collect()
    }

    /// Set `FS_IMMUTABLE_FL` on a path. This needs `CAP_LINUX_IMMUTABLE` and a filesystem that
    /// supports the attribute, so it reports whether it worked and the test skips when it did
    /// not: an unprivileged `cargo test` has nothing to check here.
    fn set_immutable(path: &Path) -> bool {
        let Ok(name) = CString::new(path.as_os_str().as_bytes()) else {
            return false;
        };
        // SAFETY: `name` is a valid NUL-terminated string that outlives the call.
        let raw = unsafe { libc::open(name.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
        if raw < 0 {
            return false;
        }
        // SAFETY: `raw` was just opened here and is owned by nothing else.
        let file = unsafe { OwnedFd::from_raw_fd(raw) };
        let mut flags: libc::c_int = 0;
        // SAFETY: the descriptor is open and the kernel writes one `int` through the pointer.
        if unsafe { libc::ioctl(file.as_raw_fd(), libc::FS_IOC_GETFLAGS, &raw mut flags) } < 0 {
            return false;
        }
        let wanted = flags | FS_IMMUTABLE_FL;
        // SAFETY: the descriptor is open and the kernel reads one `int` through the pointer.
        let set =
            unsafe { libc::ioctl(file.as_raw_fd(), libc::FS_IOC_SETFLAGS, &raw const wanted) };

        set == 0
    }

    #[test]
    fn normalize() {
        assert_eq!(
            normalize_excludes(&list(&[
                "./cache/", "b", "cache", "a/b/", " vm ", "a/./b", "x//y"
            ]))
            .unwrap(),
            ["a/b", "b", "cache", "vm", "x/y"]
        );
        for bad in ["", " ", "/abs", "../x", "a/../b", ".", "./"] {
            assert!(normalize_excludes(&list(&[bad])).is_err(), "{bad:?}");
        }
        assert!(normalize_excludes(&[]).unwrap().is_empty());
    }

    #[test]
    fn hash_and_staging_path() {
        let a = normalize_excludes(&list(&["cache", "vm"])).unwrap();
        let b = normalize_excludes(&list(&["vm", "cache/"])).unwrap();
        assert_eq!(exclude_hash(&a), exclude_hash(&b));
        assert_eq!(exclude_hash(&a).len(), 8);
        assert_ne!(exclude_hash(&a), exclude_hash(&list(&["cache"])));
        assert_eq!(
            staging_path("/.snapshots/", &a, "root-1"),
            format!("/.snapshots/.staging/{}/root-1", exclude_hash(&a))
        );
    }

    #[test]
    fn measure_and_remove() {
        let dir = std::env::temp_dir().join(format!("rusnapshot-exclude-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for (path, size) in [
            ("keep/a", 100),
            ("cache/x/b", 300),
            ("cache/c", 200),
            ("top", 50),
        ] {
            let file = dir.join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(&file, vec![0u8; size]).unwrap();
        }
        let excludes = normalize_excludes(&list(&["cache", "top", "missing/dir"])).unwrap();
        let staging = dir.to_str().unwrap();

        let measured = measure_excludes(staging, &excludes).unwrap();
        assert_eq!((measured.paths, measured.bytes), (2, 550));
        assert!(dir.join("cache/c").exists(), "measuring must not delete");

        let removed = remove_excludes(staging, &excludes).unwrap();
        assert_eq!((removed.paths, removed.bytes), (2, 550));
        assert!(!dir.join("cache").exists());
        assert!(!dir.join("top").exists());
        assert!(dir.join("keep/a").exists());
        assert_eq!(remove_excludes(staging, &excludes).unwrap().paths, 0);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn symlinks_never_lead_outside_the_snapshot() {
        let base =
            std::env::temp_dir().join(format!("rusnapshot-exclude-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let outside = base.join("outside");
        let staging = base.join("staging");
        std::fs::create_dir_all(outside.join("sub")).unwrap();
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(outside.join("sub/x"), vec![0u8; 100]).unwrap();
        std::os::unix::fs::symlink(&outside, staging.join("link")).unwrap();
        let root = staging.to_str().unwrap();

        // Through the symlink: refused, nothing touched, nothing measured.
        let through = normalize_excludes(&list(&["link/sub"])).unwrap();
        let err = remove_excludes(root, &through).unwrap_err().to_string();
        assert!(err.contains("outside the snapshot"), "{err}");
        assert!(measure_excludes(root, &through).is_err());
        assert!(outside.join("sub/x").exists());

        // The symlink itself: removed as a link, the target is untouched.
        let link = normalize_excludes(&list(&["link"])).unwrap();
        let removed = remove_excludes(root, &link).unwrap();
        assert_eq!((removed.paths, removed.bytes), (1, 0));
        assert!(staging.join("link").symlink_metadata().is_err());
        assert!(outside.join("sub/x").exists());

        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn clearing_attributes_never_follows_a_symlink() {
        let base =
            std::env::temp_dir().join(format!("rusnapshot-exclude-attr-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let target = base.join("target");
        let link = base.join("link");
        std::fs::write(&target, b"x").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        // A file with neither attribute set: nothing to clear, and it says so.
        assert!(!clear_attributes(&target).unwrap());
        // Through a symlink: refused by O_NOFOLLOW instead of reaching whatever it points at.
        assert!(clear_attributes(&link).is_err());
        assert!(target.exists());

        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn removes_an_immutable_file_under_an_excluded_path() {
        let dir =
            std::env::temp_dir().join(format!("rusnapshot-exclude-imm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("cache/sub")).unwrap();
        std::fs::write(dir.join("cache/sub/locked"), vec![0u8; 100]).unwrap();
        std::fs::write(dir.join("keep"), vec![0u8; 10]).unwrap();

        if !set_immutable(&dir.join("cache/sub/locked")) {
            // Unprivileged run or a filesystem without the attribute: nothing to exercise.
            std::fs::remove_dir_all(&dir).unwrap();
            return;
        }

        let excludes = normalize_excludes(&list(&["cache"])).unwrap();
        let removed = remove_excludes(dir.to_str().unwrap(), &excludes).unwrap();
        assert_eq!((removed.paths, removed.bytes), (1, 100));
        assert!(!dir.join("cache").exists());
        assert!(dir.join("keep").exists(), "only the excluded path goes");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn removes_the_contents_of_an_immutable_directory() {
        let dir =
            std::env::temp_dir().join(format!("rusnapshot-exclude-dir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("cache/locked")).unwrap();
        std::fs::write(dir.join("cache/locked/inside"), vec![0u8; 40]).unwrap();
        std::fs::write(dir.join("keep"), vec![0u8; 10]).unwrap();

        // The file is ordinary here: what refuses the removal is the directory holding it.
        if !set_immutable(&dir.join("cache/locked")) {
            std::fs::remove_dir_all(&dir).unwrap();
            return;
        }

        let excludes = normalize_excludes(&list(&["cache"])).unwrap();
        let removed = remove_excludes(dir.to_str().unwrap(), &excludes).unwrap();
        assert_eq!((removed.paths, removed.bytes), (1, 40));
        assert!(!dir.join("cache").exists());
        assert!(dir.join("keep").exists(), "only the excluded path goes");

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
