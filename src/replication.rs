//! Replication of snapshots to another disk or host with `btrfs send` / `btrfs receive`.
//!
//! A replication target is a directory on a btrfs filesystem, local (an external disk) or on
//! another machine through ssh. Every replica that completed and was verified is recorded in the
//! `replicas` table, which is what makes incremental sends possible: the parent of a send is the
//! newest replica of the same source subvolume that still exists on both sides.

use {
    crate::{
        args::{Args, ReplicateConfig},
        database,
        structs::{ReplicaRecord, SnapshotRecord},
        utils::strip_trailing_slash,
    },
    anyhow::{Context, Result, anyhow, bail},
    sqlite::Connection,
    std::{
        io::{IsTerminal, Read, Write},
        path::Path,
        process::{Command, Stdio},
        time::{Duration, Instant},
    },
};

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
            println!(
                "[dry-run] would send {} to {} ({})",
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
    let source = local_subvolume_info(path)?;
    let remote_path = replication.subvolume_path(&snapshot.name);

    if replication.exists(&remote_path)? {
        let existing = replication.subvolume_info(&remote_path)?;
        if existing.received_uuid.as_deref() == Some(source.uuid.as_str()) {
            println!(
                "{} is already present at {}, recording it",
                snapshot.name, replication.url
            );
            database::insert_replica(connection, &record(snapshot, path, replication, None))?;
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
    println!("Sending {} to {} ({mode})", snapshot.name, replication.url);

    let transfer = match send_and_receive(
        path,
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
            path,
            replication,
            parent.map(|parent| parent.name),
        ),
    )?;
    let seconds = transfer.elapsed.as_secs_f64().max(0.001);
    println!(
        "Sent {} to {}: {} in {seconds:.1}s ({}/s), {mode}",
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
