//! SFTP desktop mounter
//!
//! Owns the sshfs/fusermount subprocess boundary for the SFTP plugin.
//! Mirrors the upstream KDE Connect shape (kdeconnect-kde
//! `plugins/sftp/mounter.cpp:72` spawns `sshfs`, `:105` passes the password
//! via `-o password_stdin`, `:114` writes it to the child's stdin —
//! the password never appears in argv). Unmount runs `fusermount3 -u`
//! (modern FUSE 3), with `fusermount` as a fallback
//! (kdeconnect-kde `mounter.cpp:204`).
//!
//! The command builder and runner are injectable so tests can substitute
//! fake binaries. See the harness in `tests/fake_binary_shim.rs`.
//!
//! Under the packaged systemd unit the daemon has a private mount
//! namespace, and a mount made there never reaches the desktop that
//! browses it. There sshfs runs as a transient unit of the user manager
//! (`SystemCommandRunner::run_mount`): the mount lands in the session's
//! namespace, propagates back into ours, and stopping the unit unmounts it.
//!
//! Safety properties enforced here:
//! - The password is NEVER an argv element, env var, or file write.
//! - The runner only knows about paths and binary names; the caller passes
//!   the password separately and the runner pipes it on stdin.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::utils::errors::{Error, Result};

/// The information the mounter needs to spawn sshfs. Mirrors the subset of
/// `SftpConnectionInfo` the mount actually consumes — the password is
/// carried separately so callers cannot accidentally pass it via a path
/// or a `String` that ends up logged.
#[derive(Debug, Clone)]
pub struct MountRequest {
    pub ip: String,
    pub port: u16,
    pub user: String,
    pub path: String,
    /// Server-determined path under `data_dir`. The mounter creates the
    /// directory if it does not exist.
    pub mount_point: PathBuf,
}

/// Outcome of a mount attempt. The plugin maps this to its state machine
/// and to the `PluginEvent::SftpUpdate` broadcast.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MountOutcome {
    Mounted,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnmountOutcome {
    Unmounted,
    /// Unmount could not release the mount (still listed in /proc/mounts or
    /// the unmount tool returned non-zero). Plugin can retry or surface
    /// to the API caller.
    Failed(String),
}

/// Pluggable command-runner boundary. Production code uses
/// `SystemCommandRunner`; tests inject a fake.
pub trait CommandRunner: Send + Sync {
    /// Locate a binary on `PATH`. Returns the absolute path if found.
    fn which(&self, name: &str) -> Option<PathBuf>;

    /// Spawn `program args...`, write `stdin_payload` to the child's
    /// stdin (with a trailing newline), wait for it to exit, and return
    /// the exit status + combined stdout/stderr.
    fn run_with_stdin(
        &self,
        program: &Path,
        args: &[OsString],
        stdin_payload: Option<&str>,
    ) -> Result<CommandOutput>;

    /// Run sshfs so its mount reaches the desktop, as `unit` where the
    /// runner keeps mounts in units. `mounted` reports when the mount point
    /// is live. By default sshfs runs directly, as it always has.
    fn run_mount(
        &self,
        unit: &str,
        program: &Path,
        args: &[OsString],
        password: &str,
        mounted: &dyn Fn() -> bool,
    ) -> Result<CommandOutput> {
        let _ = (unit, mounted);
        self.run_with_stdin(program, args, Some(password))
    }

    /// Release a mount made by `run_mount` under the same `unit`.
    fn run_unmount(&self, unit: &str, program: &Path, args: &[OsString]) -> Result<CommandOutput> {
        let _ = unit;
        self.run_with_stdin(program, args, None)
    }
}

/// How long a session mount may take, from the start, before it counts as
/// failed. Under the plugin's 10 second `MOUNT_TIMEOUT`, so a slow mount is
/// stopped here before the plugin gives up and reports it failed.
const SESSION_MOUNT_WAIT: std::time::Duration = std::time::Duration::from_secs(8);

/// How long the stop of an abandoned mount's unit may take to be queued,
/// after `SESSION_MOUNT_WAIT` has run out. Together they stay under the
/// plugin's `MOUNT_TIMEOUT`.
const STOP_QUEUE_WAIT: std::time::Duration = std::time::Duration::from_secs(1);

#[derive(Debug, Clone)]
pub struct CommandOutput {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Production runner: uses `which` and `std::process::Command`.
pub struct SystemCommandRunner {
    path: OsString,
    /// Set when systemd started the daemon, so its mounts belong in units
    /// of the user manager (see the module docs).
    session_units: bool,
}

impl SystemCommandRunner {
    pub fn new() -> Self {
        Self {
            path: std::env::var_os("PATH").unwrap_or_default(),
            session_units: std::env::var_os("INVOCATION_ID").is_some(),
        }
    }

    /// Stop a session mount's unit, waiting no later than `deadline`.
    /// `queue` only hands systemd the stop instead of waiting for the unit to
    /// go. `None` when systemctl is missing or did not finish in time.
    fn stop_unit(
        &self,
        unit: &str,
        queue: bool,
        deadline: std::time::Instant,
    ) -> Option<CommandOutput> {
        let systemctl = self.which("systemctl")?;
        let mut args = vec![OsString::from("--user"), OsString::from("stop")];
        if queue {
            args.push(OsString::from("--no-block"));
        }
        args.push(OsString::from(unit));
        self.run_until(&systemctl, &args, deadline)
    }

    /// Run a command with no stdin until it exits or `deadline` passes, when
    /// it is killed. `None` when it could not start or ran out of time.
    fn run_until(
        &self,
        program: &Path,
        args: &[OsString],
        deadline: std::time::Instant,
    ) -> Option<CommandOutput> {
        let mut child = self.make_command(program, args).spawn().ok()?;
        drop(child.stdin.take());
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
            }
        }
        let output = child.wait_with_output().ok()?;
        Some(CommandOutput {
            status: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    /// Build a Command whose argv and env are pre-validated. The caller
    /// supplies the program as an absolute path discovered by `which` so
    /// we never shell out via a shell.
    fn make_command(&self, program: &Path, args: &[OsString]) -> Command {
        let mut cmd = Command::new(program);
        cmd.args(args);
        // Belt-and-suspenders: clear the inherited env (PATH is needed for
        // sshfs's own helpers in some installs, so we keep it but document
        // the intent). The plugin MUST NOT put the password in env.
        // We do not set `cmd.env_clear()` here because some sshfs/FUSE
        // helpers rely on HOME / USER being present; the plugin-level
        // invariant is "no password in env", enforced by arg construction.
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd.stdin(Stdio::piped());
        cmd
    }
}

impl Default for SystemCommandRunner {
    fn default() -> Self {
        Self::new()
    }
}

impl CommandRunner for SystemCommandRunner {
    fn which(&self, name: &str) -> Option<PathBuf> {
        // Mirror `which` semantics: walk PATH and check executable bit.
        // Avoids pulling in the `which` crate.
        for dir in std::env::split_paths(&self.path) {
            let candidate = dir.join(name);
            if candidate.is_file() {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if let Ok(meta) = std::fs::metadata(&candidate) {
                        if meta.permissions().mode() & 0o111 != 0 {
                            return Some(candidate);
                        }
                    }
                }
                #[cfg(not(unix))]
                {
                    return Some(candidate);
                }
            }
        }
        None
    }

    fn run_with_stdin(
        &self,
        program: &Path,
        args: &[OsString],
        stdin_payload: Option<&str>,
    ) -> Result<CommandOutput> {
        let mut cmd = self.make_command(program, args);
        let mut child = cmd.spawn().map_err(|e| {
            Error::Internal(format!("Failed to spawn {}: {}", program.display(), e))
        })?;

        if let Some(payload) = stdin_payload {
            use std::io::Write;
            if let Some(mut stdin) = child.stdin.take() {
                stdin
                    .write_all(payload.as_bytes())
                    .and_then(|_| stdin.write_all(b"\n"))
                    .map_err(|e| Error::Internal(format!("Failed to write stdin: {}", e)))?;
                // Close stdin so the child sees EOF.
                drop(stdin);
            }
        } else if child.stdin.take().is_some() {
            // Close stdin even if no payload.
        }

        let output = child
            .wait_with_output()
            .map_err(|e| Error::Internal(format!("Failed to wait on child: {}", e)))?;
        Ok(CommandOutput {
            status: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    fn run_mount(
        &self,
        unit: &str,
        program: &Path,
        args: &[OsString],
        password: &str,
        mounted: &dyn Fn() -> bool,
    ) -> Result<CommandOutput> {
        let Some(systemd_run) = self
            .session_units
            .then(|| self.which("systemd-run"))
            .flatten()
        else {
            return self.run_with_stdin(program, args, Some(password));
        };
        let deadline = std::time::Instant::now() + SESSION_MOUNT_WAIT;
        // A unit left by an earlier run would still hold the name, so it has
        // to be gone before the new one starts, within the same budget.
        if self.which("systemctl").is_some() && self.stop_unit(unit, false, deadline).is_none() {
            return Ok(CommandOutput {
                status: -1,
                stdout: String::new(),
                stderr: "an earlier mount did not stop in time".to_string(),
            });
        }

        let mut argv: Vec<OsString> = ["--user", "--quiet", "--collect", "--pipe"]
            .map(OsString::from)
            .into();
        argv.push(OsString::from(format!("--unit={unit}")));
        argv.push(OsString::from("--"));
        argv.push(program.as_os_str().to_owned());
        argv.extend(args.iter().cloned());
        // In the foreground the unit lasts exactly as long as the mount.
        argv.push(OsString::from("-f"));

        let mut child = self
            .make_command(&systemd_run, &argv)
            .spawn()
            .map_err(|e| {
                Error::Internal(format!("Failed to spawn {}: {}", systemd_run.display(), e))
            })?;
        if let Some(mut stdin) = child.stdin.take() {
            use std::io::Write;
            stdin
                .write_all(password.as_bytes())
                .and_then(|_| stdin.write_all(b"\n"))
                .map_err(|e| Error::Internal(format!("Failed to write stdin: {}", e)))?;
        }

        // The client stays attached until the unit stops, so it is reaped
        // off-thread once the mount is up or abandoned.
        let reap = |child: std::process::Child| {
            std::thread::spawn(move || {
                let _ = child.wait_with_output();
            });
        };
        loop {
            if mounted() {
                reap(child);
                return Ok(CommandOutput {
                    status: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                });
            }
            let exited = child
                .try_wait()
                .map_err(|e| Error::Internal(format!("Failed to poll child: {}", e)))?;
            if exited.is_some() {
                let output = child
                    .wait_with_output()
                    .map_err(|e| Error::Internal(format!("Failed to wait on child: {}", e)))?;
                // sshfs leaving without a mount is a failure even on status 0.
                let status = match output.status.code() {
                    Some(0) => 1,
                    Some(code) => code,
                    None => -1,
                };
                return Ok(CommandOutput {
                    status,
                    stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                    stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                });
            }
            if std::time::Instant::now() >= deadline {
                // The next mount waits for the unit to be gone before it
                // starts another, so the stop only has to be queued here.
                self.stop_unit(unit, true, std::time::Instant::now() + STOP_QUEUE_WAIT);
                reap(child);
                return Ok(CommandOutput {
                    status: -1,
                    stdout: String::new(),
                    stderr: "sshfs did not mount in time".to_string(),
                });
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }

    fn run_unmount(&self, unit: &str, program: &Path, args: &[OsString]) -> Result<CommandOutput> {
        // Stopping the unit ends sshfs, which unmounts in the session and so
        // here too. A mount from before units (or a unit already gone) falls
        // through to fusermount.
        if self.session_units {
            let deadline = std::time::Instant::now() + SESSION_MOUNT_WAIT;
            if let Some(output) = self.stop_unit(unit, false, deadline) {
                if output.status == 0 {
                    return Ok(output);
                }
            }
        }
        self.run_with_stdin(program, args, None)
    }
}

/// The transient unit that holds a session mount, named after its mount
/// point so a later run can find and stop it.
fn session_unit_for(mount_point: &Path) -> String {
    let name: String = mount_point
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("rust-connect-{name}")
}

/// Whether `path` is a mount point in this process's mount namespace.
pub(crate) fn is_mount_point(path: &Path) -> bool {
    #[cfg(target_os = "linux")]
    {
        let Ok(mountinfo) = std::fs::read_to_string("/proc/self/mountinfo") else {
            return false;
        };
        use std::os::unix::ffi::OsStrExt;
        let target = path.as_os_str().as_bytes();
        mountinfo.lines().any(|line| {
            line.split_whitespace()
                .nth(4)
                .is_some_and(|mount_point| unescape_mount_path(mount_point) == target)
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        path.exists()
    }
}

/// A mountinfo path with the kernel's octal escapes (`\040` for a space,
/// `\011`, `\012`, `\134`) turned back into bytes.
fn unescape_mount_path(field: &str) -> Vec<u8> {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let octal = bytes
            .get(i + 1..i + 4)
            .filter(|d| bytes[i] == b'\\' && d.iter().all(|b| (b'0'..=b'7').contains(b)));
        match octal {
            Some(digits) => {
                out.push(
                    digits
                        .iter()
                        .fold(0u8, |n, d| n.wrapping_mul(8).wrapping_add(d - b'0')),
                );
                i += 4;
            }
            None => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    out
}

/// Build the argv vector sshfs expects. The password is NOT in here —
/// it travels separately on stdin (kdeconnect-kde mounter.cpp:105, 114).
///
/// Hardening opts chosen and why:
/// - `password_stdin` — pass the password on stdin (kdeconnect-kde mounter.cpp:105).
///   The only safe option on a shared host; prevents `ps` exposure.
/// - `StrictHostKeyChecking=no` + `UserKnownHostsFile=/dev/null` — the
///   phone regenerates its host key on every reconnect, so storing it
///   is pointless; we deliberately do not prompt for confirmation.
///   (kdeconnect-kde mounter.cpp:99-100.)
/// - `reconnect` + `ServerAliveInterval=30` — tolerate brief network
///   blips without remounting. (kdeconnect-kde mounter.cpp:103-104.)
/// - `IdentityFile=` is intentionally OMITTED — kdeconnect-kde uses the
///   daemon's identity key as the SSH identity (mounter.cpp:98), but
///   in our case the phone's sshd accepts the password we just received
///   and we have no per-device SSH key to offer. The password is the
///   only auth credential the phone sends.
/// - `allow_root` is NOT set — sshfs refuses to run as root by default;
///   the daemon runs as the desktop user.
/// - `uid=` / `gid=` are NOT set — the daemon's process uid/gid are
///   correct by inheritance; setting them opens a TOCTOU window
///   (mounter.cpp:101-102 is kdeconnect-kde's only protection).
pub fn build_sshfs_args(req: &MountRequest) -> Vec<OsString> {
    let user_host_path = format!("{}@{}:{}", req.user, req.ip, req.path);
    vec![
        OsString::from(user_host_path),
        OsString::from(req.mount_point.as_os_str()),
        OsString::from("-p"),
        OsString::from(req.port.to_string()),
        OsString::from("-o"),
        OsString::from("password_stdin"),
        OsString::from("-o"),
        OsString::from("StrictHostKeyChecking=no"),
        OsString::from("-o"),
        OsString::from("UserKnownHostsFile=/dev/null"),
        OsString::from("-o"),
        OsString::from("reconnect"),
        OsString::from("-o"),
        OsString::from("ServerAliveInterval=30"),
    ]
}

/// The mounter holds no state of its own beyond its command runner and
/// the sshfs binary path. Mount state (current mount per device) lives
/// in the plugin — the mounter is stateless on purpose.
///
/// The runner is stored as an `Arc<dyn CommandRunner>` — production gets
/// `SystemCommandRunner`, tests inject a fake. The mounter is therefore
/// not generic; the previous generic shape needed blanket impls for
/// `Box<T>`/`Arc<T>` that were easy to trip over.
pub struct Mounter {
    runner: std::sync::Arc<dyn CommandRunner>,
    sshfs_path: Option<PathBuf>,
    /// First fusermount3 found; falls back to fusermount when absent.
    fusermount_path: Option<PathBuf>,
    fusermount3_path: Option<PathBuf>,
}

impl Mounter {
    pub fn new(runner: std::sync::Arc<dyn CommandRunner>) -> Self {
        let sshfs_path = runner.which("sshfs");
        let fusermount3_path = runner.which("fusermount3");
        let fusermount_path = runner.which("fusermount");
        Self {
            runner,
            sshfs_path,
            fusermount3_path,
            fusermount_path,
        }
    }

    /// Backend-availability gate. Both sshfs and at least one fusermount
    /// must be present. `is_backend_available()` on the plugin asks this.
    pub fn is_available(&self) -> bool {
        self.sshfs_path.is_some()
            && (self.fusermount3_path.is_some() || self.fusermount_path.is_some())
    }

    /// sshfs path if discovered, for diagnostics / tests.
    pub fn sshfs_path(&self) -> Option<&Path> {
        self.sshfs_path.as_deref()
    }

    /// Try to mount. The mounter creates the mount point directory if
    /// absent. Returns `Mounted` only when sshfs exits 0 and the mount
    /// is listed by the kernel; otherwise `Failed` with a short message.
    pub fn mount(&self, req: &MountRequest, password: &str) -> Result<MountOutcome> {
        let sshfs = self
            .sshfs_path
            .as_deref()
            .ok_or_else(|| Error::PluginError {
                plugin: "sftp".to_string(),
                message: "sshfs binary not found on PATH".to_string(),
            })?;

        if let Err(e) = std::fs::create_dir_all(&req.mount_point) {
            return Ok(MountOutcome::Failed(format!(
                "failed to create mount point {}: {}",
                req.mount_point.display(),
                e
            )));
        }

        // A mount left here, by an older daemon or a unit that is gone, would
        // read as this one succeeding while the new sshfs fails.
        if is_mount_point(&req.mount_point) {
            let _ = self.unmount(&req.mount_point);
            if is_mount_point(&req.mount_point) {
                return Ok(MountOutcome::Failed(format!(
                    "mount point {} is still in use by an earlier mount",
                    req.mount_point.display()
                )));
            }
        }

        let args = build_sshfs_args(req);
        let output = self.runner.run_mount(
            &session_unit_for(&req.mount_point),
            sshfs,
            &args,
            password,
            &|| is_mount_point(&req.mount_point),
        )?;
        if output.status != 0 {
            // stderr first; sshfs occasionally reports on stdout only.
            // Both streams are password-free (the password travels on
            // stdin and sshfs never echoes it).
            let detail = format_truncated(&output.stderr);
            let detail = if detail.is_empty() {
                format_truncated(&output.stdout)
            } else {
                detail
            };
            tracing::warn!(
                mount_point = %req.mount_point.display(),
                status = output.status,
                stderr = %format_truncated(&output.stderr),
                stdout = %format_truncated(&output.stdout),
                event = "sftp_mount_failed",
                "sshfs exited non-zero"
            );
            if detail.is_empty() {
                return Ok(MountOutcome::Failed(format!(
                    "sshfs exited with status {}",
                    output.status
                )));
            }
            return Ok(MountOutcome::Failed(detail));
        }

        // Upstream relies on the sshfs exit code as the success signal
        // (kdeconnect-kde mounter.cpp:118-122 emits `mounted` on
        // QProcess::started). We use the exit code too; a 10s timeout is
        // an upstream choice (mounter.cpp:32) — for a desktop file
        // browser, returning on the exit code (or a process-start signal)
        // is the right granularity here.
        Ok(MountOutcome::Mounted)
    }

    /// Try to unmount. Uses fusermount3 if available, else fusermount.
    /// 0 exit means unmounted. Non-zero becomes `Failed(stderr)`.
    pub fn unmount(&self, mount_point: &Path) -> Result<UnmountOutcome> {
        let program = self
            .fusermount3_path
            .as_deref()
            .or(self.fusermount_path.as_deref())
            .ok_or_else(|| Error::PluginError {
                plugin: "sftp".to_string(),
                message: "fusermount / fusermount3 not found on PATH".to_string(),
            })?;

        let args = [
            OsString::from("-u"),
            OsString::from(mount_point.as_os_str()),
        ];
        let output = self
            .runner
            .run_unmount(&session_unit_for(mount_point), program, &args)?;
        if output.status == 0 {
            Ok(UnmountOutcome::Unmounted)
        } else {
            Ok(UnmountOutcome::Failed(format_truncated(&output.stderr)))
        }
    }
}

fn format_truncated(s: &str) -> String {
    // Cap error message length to keep the API response bounded.
    const LIMIT: usize = 512;
    if s.len() <= LIMIT {
        s.trim().to_string()
    } else {
        let mut end = LIMIT;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", s[..end].trim())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    /// Record argv + stdin to sidecar files. Honors an optional
    /// `.fail` sidecar file containing an exit code so tests can
    /// simulate sshfs crashes without using process-global env vars
    /// (parallel test execution makes env-var shims race-prone).
    /// Exits as soon as the captures are flushed; the caller (the
    /// runner's `wait_with_output`) blocks on our exit, so a "stay
    /// alive" sleep is unnecessary and only widens the race window
    /// for parallel tests.
    fn write_fake_sshfs(dir: &Path, record_path: &Path) -> PathBuf {
        let path = dir.join("sshfs");
        let argv_log = record_path.to_string_lossy().into_owned();
        let stdin_log = argv_log.replace(".argv", ".stdin");
        let fail_flag = argv_log.replace(".argv", ".fail");
        let script = format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$0\" \"$@\" > {argv_log:?}\n\
             cat > {stdin_log:?} <&0\n\
             if [ -f {fail_flag:?} ]; then exit \"$(cat {fail_flag:?})\"; fi\n\
             exit 0\n",
        );
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o755)
            .open(&path)
            .expect("create fake sshfs");
        f.write_all(script.as_bytes()).expect("write fake sshfs");
        path
    }

    fn write_fake_fusermount(dir: &Path, record_path: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        let record = record_path.to_string_lossy().into_owned();
        let fail_flag = record.replace(".argv", ".fail");
        let script = format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$0\" \"$@\" > {record:?}\n\
             if [ -f {fail_flag:?} ]; then exit \"$(cat {fail_flag:?})\"; fi\n\
             exit 0\n",
        );
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o755)
            .open(&path)
            .expect("create fake fusermount");
        f.write_all(script.as_bytes())
            .expect("write fake fusermount");
        path
    }

    /// Runner that searches a single temp dir for binaries.
    struct SingleDirRunner {
        dir: PathBuf,
    }
    impl CommandRunner for SingleDirRunner {
        fn which(&self, name: &str) -> Option<PathBuf> {
            let p = self.dir.join(name);
            if p.is_file() {
                Some(p)
            } else {
                None
            }
        }
        fn run_with_stdin(
            &self,
            program: &Path,
            args: &[OsString],
            stdin_payload: Option<&str>,
        ) -> Result<CommandOutput> {
            let mut cmd = Command::new(program);
            cmd.args(args);
            cmd.stdin(Stdio::piped());
            cmd.stdout(Stdio::piped());
            cmd.stderr(Stdio::piped());
            // Observed under parallel test execution (integrator gate,
            // 2026-08-06): spawn can hit ETXTBSY ("Text file busy") when
            // the freshly written fake script is exec'd while another
            // test's write of its own fake is in flight. Each test uses a
            // unique TempDir, so the window is kernel-side and rare; retry
            // briefly instead of flaking the suite.
            let mut child = {
                let mut attempts = 0;
                loop {
                    match cmd.spawn() {
                        Ok(c) => break c,
                        Err(e) if e.raw_os_error() == Some(26) && attempts < 5 => {
                            attempts += 1;
                            std::thread::sleep(std::time::Duration::from_millis(20));
                        }
                        Err(e) => panic!("spawn fake {}: {e}", program.display()),
                    }
                }
            };
            if let Some(payload) = stdin_payload {
                use std::io::Write;
                if let Some(mut stdin) = child.stdin.take() {
                    stdin.write_all(payload.as_bytes()).unwrap();
                    stdin.write_all(b"\n").unwrap();
                }
            }
            let out = child.wait_with_output().expect("wait fake");
            Ok(CommandOutput {
                status: out.status.code().unwrap_or(-1),
                stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            })
        }
    }

    fn make_request() -> MountRequest {
        MountRequest {
            ip: "192.168.1.10".to_string(),
            port: 1740,
            user: "kdeconnect".to_string(),
            path: "/storage/emulated/0".to_string(),
            mount_point: PathBuf::from("/tmp/sftp-test-mount"),
        }
    }

    #[test]
    fn mount_paths_with_spaces_match_the_kernels_escaped_form() {
        assert_eq!(unescape_mount_path(r"/home/a\040b/mnt"), b"/home/a b/mnt");
        assert_eq!(unescape_mount_path(r"/x\134y\011z"), b"/x\\y\tz");
        assert_eq!(unescape_mount_path(r"/plain\path"), b"/plain\\path");
    }

    #[test]
    fn build_sshfs_args_puts_password_stdin_opt_in_argv_but_not_password() {
        let req = make_request();
        let args = build_sshfs_args(&req);
        let argv_str: Vec<String> = args
            .iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();

        // The hardening opts from upstream are present.
        assert!(argv_str
            .windows(2)
            .any(|w| w[0] == "-o" && w[1] == "password_stdin"));
        assert!(argv_str
            .windows(2)
            .any(|w| w[0] == "-o" && w[1] == "StrictHostKeyChecking=no"));
        assert!(argv_str
            .windows(2)
            .any(|w| w[0] == "-o" && w[1] == "UserKnownHostsFile=/dev/null"));
        assert!(argv_str
            .windows(2)
            .any(|w| w[0] == "-o" && w[1] == "reconnect"));
        assert!(argv_str
            .windows(2)
            .any(|w| w[0] == "-o" && w[1].starts_with("ServerAliveInterval=")));

        // The password (or any marker thereof) is NOT in argv. The argv
        // is the surface `ps` reads.
        let secret = "SUPERSECRET-PWD-9f8e7d";
        for arg in &argv_str {
            assert_ne!(arg, secret, "password leaked into argv: {arg}");
        }
        // The user@host:path form is the credential surface; it must
        // not embed the password in the path component either.
        let user_host = format!("{}@{}:{}", req.user, req.ip, req.path);
        assert_eq!(argv_str[0], user_host);
    }

    #[test]
    fn a_mount_point_still_in_use_is_not_reported_as_mounted() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let bin_dir = dir.path().join("bin");
        std::fs::create_dir_all(&bin_dir).expect("bin dir");
        let log = dir.path().join("sshfs.argv");
        write_fake_sshfs(&bin_dir, &log);
        write_fake_fusermount(
            &bin_dir,
            &dir.path().join("fusermount3.argv"),
            "fusermount3",
        );
        let mounter = Mounter::new(std::sync::Arc::new(SingleDirRunner { dir: bin_dir }));
        // /proc is always mounted, as a stale sshfs mount would be.
        let req = MountRequest {
            mount_point: PathBuf::from("/proc"),
            ..make_request()
        };
        match mounter.mount(&req, "secret").expect("mount call") {
            MountOutcome::Failed(error) => assert!(error.contains("still in use"), "{error}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert!(!log.exists(), "sshfs must not run over a live mount");
    }

    #[test]
    fn mounter_writes_password_to_stdin_only_not_argv() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let bin_dir = dir.path().join("bin");
        std::fs::create_dir_all(&bin_dir).expect("bin dir");
        let log = dir.path().join("sshfs.argv");
        let um_log = dir.path().join("fusermount3.argv");
        let secret = "verysecret-1q2w3e4r";
        write_fake_sshfs(&bin_dir, &log);
        write_fake_fusermount(&bin_dir, &um_log, "fusermount3");

        let runner = SingleDirRunner {
            dir: bin_dir.clone(),
        };
        let mounter = Mounter::new(std::sync::Arc::new(runner));
        assert!(
            mounter.is_available(),
            "fake sshfs + fusermount must make the mounter available"
        );

        let req = MountRequest {
            mount_point: dir.path().join("mnt"),
            ..make_request()
        };
        let outcome = mounter.mount(&req, secret).expect("mount call");
        assert_eq!(outcome, MountOutcome::Mounted);

        let argv_text = std::fs::read_to_string(&log).expect("read argv log");
        assert!(
            !argv_text.contains(secret),
            "password MUST NOT appear in argv (captured: {argv_text})"
        );
        let stdin_text =
            std::fs::read_to_string(dir.path().join("sshfs.stdin")).expect("read stdin log");
        assert!(
            stdin_text.contains(secret),
            "password MUST be written to stdin (captured: {stdin_text})"
        );
    }

    #[test]
    fn unmount_calls_fusermount3_with_tracked_mountpoint() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let bin_dir = dir.path().join("bin");
        std::fs::create_dir_all(&bin_dir).expect("bin dir");
        let um_log = dir.path().join("fusermount3.argv");
        write_fake_sshfs(&bin_dir, &dir.path().join("sshfs.argv"));
        write_fake_fusermount(&bin_dir, &um_log, "fusermount3");

        let runner = SingleDirRunner {
            dir: bin_dir.clone(),
        };
        let mounter = Mounter::new(std::sync::Arc::new(runner));
        assert!(mounter.is_available());

        let mount_point = dir.path().join("mnt");
        let outcome = mounter.unmount(&mount_point).expect("unmount call");
        assert_eq!(outcome, UnmountOutcome::Unmounted);

        let argv_text = std::fs::read_to_string(&um_log).expect("read unmount log");
        // The argv log records one program + one arg per line, in order.
        // The mount point path MUST appear as a line, and `-u` MUST appear
        // as a line.
        let lines: Vec<&str> = argv_text.lines().collect();
        assert!(
            lines.contains(&"-u"),
            "unmount did not include -u flag: {argv_text}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l == &mount_point.to_string_lossy().as_ref()),
            "unmount did not include mount point in args: {argv_text}"
        );
    }

    #[test]
    fn unmount_falls_back_to_fusermount_when_fusermount3_missing() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let bin_dir = dir.path().join("bin");
        std::fs::create_dir_all(&bin_dir).expect("bin dir");
        let um_log = dir.path().join("fusermount.argv");
        write_fake_sshfs(&bin_dir, &dir.path().join("sshfs.argv"));
        // Only the legacy fusermount is on the bin path; fusermount3 absent.
        write_fake_fusermount(&bin_dir, &um_log, "fusermount");

        let runner = SingleDirRunner {
            dir: bin_dir.clone(),
        };
        let mounter = Mounter::new(std::sync::Arc::new(runner));
        assert!(
            mounter.is_available(),
            "fusermount fallback must keep the mounter available"
        );

        let mount_point = dir.path().join("mnt");
        let outcome = mounter.unmount(&mount_point).expect("unmount call");
        assert_eq!(outcome, UnmountOutcome::Unmounted);
        // The legacy binary was the one that ran.
        let argv_text = std::fs::read_to_string(&um_log).expect("read um log");
        assert!(argv_text.contains("fusermount\n") || argv_text.ends_with("fusermount"));
    }

    #[test]
    fn mounter_unavailable_when_sshfs_missing() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let bin_dir = dir.path().join("bin");
        std::fs::create_dir_all(&bin_dir).expect("bin dir");
        // Only fusermount, no sshfs.
        let um_log = dir.path().join("fusermount3.argv");
        write_fake_fusermount(&bin_dir, &um_log, "fusermount3");

        let runner = SingleDirRunner { dir: bin_dir };
        let mounter = Mounter::new(std::sync::Arc::new(runner));
        assert!(
            !mounter.is_available(),
            "missing sshfs must make mounter unavailable"
        );

        let req = make_request();
        let err = mounter.mount(&req, "anypwd").expect_err("mount must error");
        match err {
            Error::PluginError { plugin, message } => {
                assert_eq!(plugin, "sftp");
                assert!(message.contains("sshfs"), "message: {message}");
            }
            other => panic!("expected PluginError, got {other:?}"),
        }
    }

    #[test]
    fn mounter_returns_failed_when_sshfs_exits_nonzero() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let bin_dir = dir.path().join("bin");
        std::fs::create_dir_all(&bin_dir).expect("bin dir");
        let log = dir.path().join("sshfs.argv");
        write_fake_sshfs(&bin_dir, &log);
        write_fake_fusermount(
            &bin_dir,
            &dir.path().join("fusermount3.argv"),
            "fusermount3",
        );

        // Force the fake sshfs to fail with exit 13. The fake reads the
        // fail code from a sidecar file (NOT an env var) so parallel
        // tests don't race on process-global state.
        std::fs::write(dir.path().join("sshfs.fail"), "13").expect("write fail flag");
        let runner = SingleDirRunner { dir: bin_dir };
        let mounter = Mounter::new(std::sync::Arc::new(runner));
        let req = MountRequest {
            mount_point: dir.path().join("mnt"),
            ..make_request()
        };
        let outcome = mounter.mount(&req, "secret").expect("mount call");
        match outcome {
            MountOutcome::Failed(msg) => {
                assert!(!msg.is_empty(), "failure must carry a message");
            }
            MountOutcome::Mounted => panic!("non-zero exit must surface as Failed"),
        }
    }

    /// Fake `systemd-run`, `systemctl` and `fusermount3` for the session-unit
    /// path. `systemd-run` records its argv and stdin, then either fails (a
    /// `.fail` sidecar holds the message) or creates `mounted` and lingers
    /// like the real client attached to a running unit.
    fn session_fakes(dir: &Path) -> PathBuf {
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).expect("bin dir");
        let d = dir.to_string_lossy().into_owned();
        let scripts = [
            (
                "systemd-run",
                format!(
                    "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{d}/run.argv'\ncat > '{d}/run.stdin'\n\
                     if [ -f '{d}/run.fail' ]; then cat '{d}/run.fail' >&2; exit 1; fi\n\
                     touch '{d}/mounted'\nsleep 1\n"
                ),
            ),
            (
                "systemctl",
                format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{d}/systemctl.log'\n"),
            ),
            (
                "fusermount3",
                format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{d}/fusermount.log'\n"),
            ),
        ];
        for (name, script) in scripts {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o755)
                .open(bin.join(name))
                .expect("create fake");
            f.write_all(script.as_bytes()).expect("write fake");
        }
        // Let any fork that inherited a write handle exec first (ETXTBSY).
        std::thread::sleep(std::time::Duration::from_millis(50));
        bin
    }

    fn session_runner(bin: &Path) -> SystemCommandRunner {
        SystemCommandRunner {
            path: bin.as_os_str().to_owned(),
            session_units: true,
        }
    }

    #[test]
    fn session_mount_runs_sshfs_in_a_user_unit_with_the_password_on_stdin() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let bin = session_fakes(dir.path());
        let runner = session_runner(&bin);
        let req = MountRequest {
            mount_point: dir.path().join("sftp-dev1"),
            ..make_request()
        };
        let unit = session_unit_for(&req.mount_point);
        assert_eq!(unit, "rust-connect-sftp-dev1");
        let marker = dir.path().join("mounted");
        let output = runner
            .run_mount(
                &unit,
                Path::new("/usr/bin/sshfs"),
                &build_sshfs_args(&req),
                "secret",
                &|| marker.exists(),
            )
            .expect("run_mount");
        assert_eq!(output.status, 0);

        let argv = std::fs::read_to_string(dir.path().join("run.argv")).expect("argv");
        let argv: Vec<&str> = argv.lines().collect();
        assert_eq!(
            argv[..6],
            [
                "--user",
                "--quiet",
                "--collect",
                "--pipe",
                "--unit=rust-connect-sftp-dev1",
                "--"
            ]
        );
        assert_eq!(argv[6], "/usr/bin/sshfs");
        assert_eq!(argv.last(), Some(&"-f"));
        assert!(!argv.contains(&"secret"), "the password must stay off argv");
        let stdin = std::fs::read_to_string(dir.path().join("run.stdin")).expect("stdin");
        assert_eq!(stdin, "secret\n");
        let stops = std::fs::read_to_string(dir.path().join("systemctl.log")).expect("stops");
        assert!(stops.contains("--user stop rust-connect-sftp-dev1"));
    }

    #[test]
    fn session_mount_reports_why_sshfs_left() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let bin = session_fakes(dir.path());
        std::fs::write(
            dir.path().join("run.fail"),
            "read: Connection reset by peer",
        )
        .expect("fail flag");
        let runner = session_runner(&bin);
        let marker = dir.path().join("mounted");
        let output = runner
            .run_mount(
                "rust-connect-sftp-dev1",
                Path::new("/usr/bin/sshfs"),
                &build_sshfs_args(&make_request()),
                "secret",
                &|| marker.exists(),
            )
            .expect("run_mount");
        assert_ne!(output.status, 0);
        assert!(output.stderr.contains("Connection reset by peer"));
    }

    /// Replace one of the `session_fakes` programs.
    fn replace_fake(bin: &Path, name: &str, body: &str) {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o755)
            .open(bin.join(name))
            .expect("create fake");
        f.write_all(format!("#!/bin/sh\n{body}\n").as_bytes())
            .expect("write fake");
        drop(f);
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    /// Every unit stop takes three seconds and sshfs never mounts: the
    /// attempt still ends before the plugin's own timeout gives up on it.
    #[test]
    fn session_mount_gives_up_within_the_plugins_timeout_when_unit_stops_are_slow() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let bin = session_fakes(dir.path());
        replace_fake(&bin, "systemctl", "exec sleep 3");
        replace_fake(&bin, "systemd-run", "cat > /dev/null\nexec sleep 9");
        let runner = session_runner(&bin);
        let start = std::time::Instant::now();
        let output = runner
            .run_mount(
                "rust-connect-sftp-dev1",
                Path::new("/usr/bin/sshfs"),
                &build_sshfs_args(&make_request()),
                "secret",
                &|| false,
            )
            .expect("run_mount");
        assert!(
            start.elapsed() < crate::plugins::sftp::MOUNT_TIMEOUT,
            "took {:?}",
            start.elapsed()
        );
        assert_eq!(output.stderr, "sshfs did not mount in time");
    }

    /// An earlier unit that will not stop within the budget means no new
    /// mount is started after the caller's time has already run out.
    #[test]
    fn session_mount_does_not_start_once_the_earlier_unit_used_up_its_time() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let bin = session_fakes(dir.path());
        replace_fake(&bin, "systemctl", "exec sleep 30");
        let runner = session_runner(&bin);
        let start = std::time::Instant::now();
        let output = runner
            .run_mount(
                "rust-connect-sftp-dev1",
                Path::new("/usr/bin/sshfs"),
                &build_sshfs_args(&make_request()),
                "secret",
                &|| false,
            )
            .expect("run_mount");
        assert!(start.elapsed() < crate::plugins::sftp::MOUNT_TIMEOUT);
        assert_ne!(output.status, 0);
        assert!(
            !dir.path().join("run.argv").exists(),
            "systemd-run must not be started"
        );
    }

    #[test]
    fn session_unmount_stops_the_unit_instead_of_fusermount() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let bin = session_fakes(dir.path());
        let runner = session_runner(&bin);
        let args = [OsString::from("-u"), OsString::from("/mnt/sftp-dev1")];
        let output = runner
            .run_unmount("rust-connect-sftp-dev1", &bin.join("fusermount3"), &args)
            .expect("run_unmount");
        assert_eq!(output.status, 0);
        let stops = std::fs::read_to_string(dir.path().join("systemctl.log")).expect("stops");
        assert!(stops.contains("--user stop rust-connect-sftp-dev1"));
        assert!(!dir.path().join("fusermount.log").exists());
    }
}
