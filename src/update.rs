//! Update preference and cached release identity; never assignment provenance.

use serde::{Deserialize, Serialize};
use std::{
    env,
    ffi::OsStr,
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[cfg(test)]
#[allow(dead_code)]
#[path = "../tests/support/test_home.rs"]
mod test_home;

const DAY: u64 = 24 * 60 * 60;
const CURL_RESERVE: Duration = Duration::from_millis(50);

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ReleaseIdentity {
    pub version: String,
    pub tag: String,
    pub commit: String,
}

// Stable release tags have three numeric components, no prerelease/build suffix.
// Numeric parsing also rejects leading zeros and overflow like SemVer does.
fn stable_version(version: &str) -> Option<[u64; 3]> {
    let mut parts = version.split('.');
    let mut numbers = [0; 3];
    for number in &mut numbers {
        let part = parts.next()?;
        if part.is_empty()
            || !part.bytes().all(|byte| byte.is_ascii_digit())
            || (part.len() > 1 && part.starts_with('0'))
        {
            return None;
        }
        *number = part.parse().ok()?;
    }
    parts.next().is_none().then_some(numbers)
}

impl ReleaseIdentity {
    fn valid(&self) -> bool {
        stable_version(&self.version).is_some()
            && self.tag == format!("v{}", self.version)
            && self.commit.len() == 40
            && self
                .commit
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    }

    pub fn is_newer_than(&self, installed: &str) -> bool {
        match (stable_version(&self.version), stable_version(installed)) {
            (Some(latest), Some(installed)) => latest > installed,
            _ => false,
        }
    }
}

const REPOSITORY: &str = "https://github.com/baochunli/rustrace";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct InstallReceipt {
    schema_version: u32,
    method: String,
    path: PathBuf,
    tag: String,
    version: String,
    repository: String,
}

impl InstallReceipt {
    fn load(path: &Path) -> Option<Self> {
        let receipt: Self = read_cached_json(path)?;
        (receipt.schema_version == 1
            && stable_version(&receipt.version).is_some()
            && receipt.tag == format!("v{}", receipt.version)
            && !receipt.repository.is_empty())
        .then_some(receipt)
    }

    fn install_root(&self, executable: &Path) -> Option<PathBuf> {
        if self.method != "cargo-git"
            || !self.path.is_absolute()
            || self.path.file_name()? != "rustrace"
            || self.path.parent()?.file_name()? != "bin"
            || fs::canonicalize(&self.path).ok()? != fs::canonicalize(executable).ok()?
        {
            return None;
        }
        Some(self.path.parent()?.parent()?.to_path_buf())
    }
}

fn validate_manifest(bytes: &[u8]) -> Result<ReleaseIdentity, String> {
    #[derive(Deserialize)]
    struct Source {
        repository: String,
        tag: String,
    }
    #[derive(Deserialize)]
    struct Manifest {
        schema_version: u32,
        version: String,
        tag: String,
        commit: String,
        event_format: u64,
        package_format: u64,
        assignment_format: u64,
        source: Source,
        // Reserved by the source-release contract; still parse its object type.
        #[allow(dead_code)]
        targets: serde_json::Map<String, serde_json::Value>,
    }
    let manifest: Manifest =
        serde_json::from_slice(bytes).map_err(|_| "invalid release manifest".to_owned())?;
    let identity = ReleaseIdentity {
        version: manifest.version,
        tag: manifest.tag,
        commit: manifest.commit,
    };
    // Format versions describe the release, not a local compatibility gate.
    let _formats = (
        manifest.event_format,
        manifest.package_format,
        manifest.assignment_format,
    );
    if manifest.schema_version != 1
        || !identity.valid()
        || manifest.source.repository != REPOSITORY
        || manifest.source.tag != identity.tag
    {
        return Err("invalid release manifest".into());
    }
    Ok(identity)
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct UpdateState {
    pub schema_version: u32,
    pub checks_enabled: bool,
    pub last_attempt: Option<u64>,
    pub last_success: Option<u64>,
    pub next_eligible: Option<u64>,
    pub latest: Option<ReleaseIdentity>,
}

impl Default for UpdateState {
    fn default() -> Self {
        Self {
            schema_version: 1,
            checks_enabled: true,
            last_attempt: None,
            last_success: None,
            next_eligible: None,
            latest: None,
        }
    }
}

pub fn state_file_path(xdg: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    xdg.filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            home.filter(|path| !path.is_empty())
                .map(|path| PathBuf::from(path).join(".local/state"))
        })
        .map(|path| path.join("rustrace/update-state.json"))
}

pub fn state_path() -> Option<PathBuf> {
    state_file_path(
        env::var_os("XDG_STATE_HOME").as_deref(),
        env::var_os("HOME").as_deref(),
    )
}

// Menu reads are capped, and nonblocking opens cannot wait on a FIFO.
fn read_cached_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options.open(path).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > BODY_LIMIT as u64 {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(BODY_LIMIT as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > BODY_LIMIT {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}

impl UpdateState {
    /// Invalid and future-schema state is absent, never a startup failure.
    pub fn load(path: &Path) -> Self {
        read_cached_json::<Self>(path)
            .filter(|state| {
                state.schema_version == 1
                    && state.latest.as_ref().is_none_or(ReleaseIdentity::valid)
            })
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        save_json(self, path)
    }

    fn eligible(&mut self, now: u64) -> bool {
        let skew = self.last_attempt.is_some_and(|time| time > now)
            || self.last_success.is_some_and(|time| time > now)
            || self
                .next_eligible
                .is_some_and(|time| time > now.saturating_add(DAY));
        if skew {
            self.last_attempt = None;
            self.last_success = None;
            self.next_eligible = None;
        }
        self.checks_enabled && self.next_eligible.is_none_or(|time| now >= time)
    }
}

fn save_json(value: &impl Serialize, path: &Path) -> io::Result<()> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("state path has no parent"))?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(
        ".update-state-{}-{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    let result = (|| {
        serde_json::to_writer(&mut file, value)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

pub const ENDPOINT: &str =
    "https://github.com/baochunli/rustrace/releases/latest/download/latest.json";
const BODY_LIMIT: usize = 65_536;

#[derive(Debug, Eq, PartialEq)]
enum CheckResult {
    Known(ReleaseIdentity),
    Unavailable(String),
    Skipped,
}

pub(crate) fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// Serialize launches and toggles within the caller’s total deadline.
fn lock_state(path: &Path, deadline: Instant) -> io::Result<fs::File> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("state path has no parent"))?;
    fs::create_dir_all(parent)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(parent.join(".update-state.lock"))?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        // CLOEXEC closes the descriptor at exec, but other threads’ forked
        // children may retain it until then. Retry within the total budget.
        loop {
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                break;
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::WouldBlock {
                return Err(error);
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "another update check is running",
                ));
            }
            std::thread::sleep(
                Duration::from_millis(1).min(deadline.saturating_duration_since(Instant::now())),
            );
        }
    }
    Ok(file)
}

/// Runtime preference for the update menu. Changing it never fetches.
pub fn set_checks_enabled(enabled: bool) -> io::Result<()> {
    let path =
        state_path().ok_or_else(|| io::Error::other("update state directory unavailable"))?;
    set_checks_enabled_at(&path, enabled)
}

/// Persist a preference at an explicit state path with the 20-second budget.
pub fn set_checks_enabled_at(path: &Path, enabled: bool) -> io::Result<()> {
    set_checks_enabled_until(path, enabled, Instant::now() + Duration::from_secs(20))
}

fn set_checks_enabled_until(path: &Path, enabled: bool, deadline: Instant) -> io::Result<()> {
    let _lock = lock_state(path, deadline)?;
    let mut state = UpdateState::load(path);
    state.checks_enabled = enabled;
    state.save(path)
}

#[cfg(test)]
fn check_with(
    path: &Path,
    now: u64,
    explicit: bool,
    fetch: impl FnOnce(Duration) -> Result<Vec<u8>, String>,
) -> CheckResult {
    let deadline = Instant::now() + Duration::from_secs(if explicit { 20 } else { 1 });
    check_until(path, now, explicit, deadline, fetch)
}

fn check_until(
    path: &Path,
    now: u64,
    explicit: bool,
    deadline: Instant,
    fetch: impl FnOnce(Duration) -> Result<Vec<u8>, String>,
) -> CheckResult {
    // Opt-out is a lock-free read: no directory, lock or state writes.
    // The enabled path still reloads under the lock before claiming an attempt.
    if !explicit && !UpdateState::load(path).checks_enabled {
        return CheckResult::Skipped;
    }
    let _lock = match lock_state(path, deadline) {
        Ok(lock) => lock,
        Err(error) if !explicit && error.kind() == io::ErrorKind::WouldBlock => {
            return CheckResult::Skipped;
        }
        Err(error) => return CheckResult::Unavailable(error.to_string()),
    };
    check_locked(path, now, explicit, deadline, fetch)
}

// Caller owns the state lock, including for the entire source installation.
fn check_locked(
    path: &Path,
    now: u64,
    explicit: bool,
    deadline: Instant,
    fetch: impl FnOnce(Duration) -> Result<Vec<u8>, String>,
) -> CheckResult {
    let mut state = UpdateState::load(path);
    let eligible = state.eligible(now);
    // Do not consume the daily slot when contention left no transport budget.
    if !explicit && (!eligible || deadline.saturating_duration_since(Instant::now()) < CURL_RESERVE)
    {
        return CheckResult::Skipped;
    }
    // Persist the claim before networking so failed/interrupted attempts count.
    state.last_attempt = Some(now);
    state.next_eligible = Some(now.saturating_add(DAY));
    if let Err(error) = state.save(path) {
        return CheckResult::Unavailable(format!("cannot save update state: {error}"));
    }
    let bound = deadline.saturating_duration_since(Instant::now());
    let result = fetch(bound).and_then(|bytes| {
        if bytes.len() > BODY_LIMIT {
            Err("release manifest exceeds 65536 bytes".into())
        } else {
            validate_manifest(&bytes)
        }
    });
    match result {
        Ok(identity) => {
            state.latest = Some(identity.clone());
            state.last_success = Some(now);
            match state.save(path) {
                Ok(()) => CheckResult::Known(identity),
                Err(error) => {
                    CheckResult::Unavailable(format!("cannot save update state: {error}"))
                }
            }
        }
        Err(error) => CheckResult::Unavailable(error),
    }
}

/// Silent pre-session check. All transport and cleanup finishes before returning.
pub(crate) fn automatic_check(workspace: &Path) {
    let deadline = Instant::now() + Duration::from_secs(1);
    let Some(path) = state_path() else {
        return;
    };
    // XDG overrides must not put update state into assignment provenance.
    let mut ancestor = path.as_path();
    while !ancestor.exists() {
        let Some(parent) = ancestor.parent() else {
            return;
        };
        ancestor = parent;
    }
    let Ok(existing) = fs::canonicalize(ancestor) else {
        return;
    };
    if path.starts_with(workspace) || existing.starts_with(workspace) {
        return;
    }
    let _ = check_until(&path, unix_seconds(), false, deadline, |bound| {
        fetch_curl(Path::new("curl"), bound)
    });
}

#[cfg(unix)]
fn fetch_curl(program: &Path, bound: Duration) -> Result<Vec<u8>, String> {
    use std::{
        io::Read,
        os::{fd::AsRawFd, unix::process::CommandExt},
        process::{Child, Command, Stdio},
        thread,
        time::Instant,
    };
    // Reserve cleanup time inside the total deadline, starting before spawn.
    let deadline = Instant::now() + bound;
    let stop = deadline - CURL_RESERVE.min(bound);
    if Instant::now() >= stop {
        return Err("update check timed out before curl started".into());
    }
    let max_time = bound.as_secs_f64().ceil().to_string();
    struct OwnedChild(Child);
    impl Drop for OwnedChild {
        fn drop(&mut self) {
            // Own the whole group: a launcher must not leave helpers or pipes.
            unsafe {
                libc::kill(-(self.0.id() as i32), libc::SIGKILL);
            }
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    fn nonblocking(pipe: &impl AsRawFd) -> io::Result<()> {
        let fd = pipe.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    fn drain(pipe: &mut impl Read, output: &mut Vec<u8>, limit: usize) -> io::Result<()> {
        let mut buffer = [0; 8192];
        // Limit work per poll even when the writer floods a stream.
        for _ in 0..16 {
            match pipe.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => output
                    .extend_from_slice(&buffer[..count.min(limit.saturating_sub(output.len()))]),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
    let child = Command::new(program)
        // First argument suppresses user curlrc cookies, URLs and side effects.
        .args([
            "--disable",
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--max-time",
            &max_time,
            "--max-filesize",
            "65536",
            ENDPOINT,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                "curl is not installed".to_owned()
            } else {
                format!("cannot start curl: {error}")
            }
        })?;
    let mut child = OwnedChild(child);
    let mut stdout = child.0.stdout.take().expect("piped curl stdout");
    let mut stderr = child.0.stderr.take().expect("piped curl stderr");
    nonblocking(&stdout)
        .and_then(|()| nonblocking(&stderr))
        .map_err(|error| error.to_string())?;
    let mut body = Vec::new();
    let mut errors = Vec::new();
    loop {
        if Instant::now() >= stop {
            return Err(format!("curl timed out ({max_time} s deadline)"));
        }
        drain(&mut stdout, &mut body, BODY_LIMIT + 1)
            .and_then(|()| drain(&mut stderr, &mut errors, 4096))
            .map_err(|error| error.to_string())?;
        if body.len() > BODY_LIMIT {
            return Err("release manifest exceeds 65536 bytes".into());
        }
        if let Some(status) = child.0.try_wait().map_err(|error| error.to_string())? {
            // Reap/clean helpers before the last nonblocking drain.
            drop(child);
            drain(&mut stdout, &mut body, BODY_LIMIT + 1)
                .and_then(|()| drain(&mut stderr, &mut errors, 4096))
                .map_err(|error| error.to_string())?;
            if body.len() > BODY_LIMIT {
                return Err("release manifest exceeds 65536 bytes".into());
            }
            if !status.success() {
                let detail = crate::display::label(&String::from_utf8_lossy(&errors), 512);
                return Err(format!("curl failed ({status}): {detail}"));
            }
            return Ok(body);
        }
        thread::sleep(Duration::from_millis(5).min(stop.saturating_duration_since(Instant::now())));
    }
}

#[cfg(not(unix))]
fn fetch_curl(_program: &Path, _bound: Duration) -> Result<Vec<u8>, String> {
    Err("update checks require a supported Unix environment".into())
}

pub(crate) fn run_update(args: &[String], output: &mut impl Write) -> u8 {
    if args.is_empty() {
        return install_update(output);
    }
    if args != ["--check"] {
        let _ = writeln!(output, "{}", crate::USAGE);
        return 2;
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    let result = state_path().map_or_else(
        || CheckResult::Unavailable("update state directory unavailable".into()),
        |path| {
            check_until(&path, unix_seconds(), true, deadline, |bound| {
                fetch_curl(Path::new("curl"), bound)
            })
        },
    );
    let installed = env!("CARGO_PKG_VERSION");
    match result {
        CheckResult::Known(latest) if latest.is_newer_than(installed) => {
            let _ = writeln!(
                output,
                "Rustrace {} is available (installed {installed}). Run: rustrace update",
                latest.tag
            );
            0
        }
        CheckResult::Known(_) => {
            let _ = writeln!(output, "Rustrace {installed} is up to date.");
            0
        }
        CheckResult::Unavailable(reason) => {
            let _ = writeln!(
                output,
                "Update check unavailable: {}",
                crate::display::label(&reason, 1024)
            );
            2
        }
        CheckResult::Skipped => unreachable!("explicit check ignores preference and throttle"),
    }
}

#[derive(Deserialize)]
struct ReleaseFormats {
    event_format: u64,
    package_format: u64,
    assignment_format: u64,
}

fn validate_installed(
    path: &Path,
    latest: &ReleaseIdentity,
    formats: &ReleaseFormats,
) -> io::Result<()> {
    let result = std::process::Command::new(path)
        .args(["--version", "--verbose"])
        .output()?;
    let text = String::from_utf8(result.stdout).map_err(io::Error::other)?;
    let lines: Vec<_> = text.lines().collect();
    let expected = [
        format!("rustrace {}", latest.version),
        format!("event format: {}", formats.event_format),
        format!("package format: {}", formats.package_format),
        format!("assignment format: {}", formats.assignment_format),
        format!("target: {}", crate::version::version_metadata().target()),
    ];
    if !result.status.success()
        || lines.first().copied() != Some(expected[0].as_str())
        || expected.iter().any(|line| {
            lines
                .iter()
                .filter(|actual| **actual == line.as_str())
                .count()
                != 1
        })
    {
        return Err(io::Error::other(
            "version, target or format metadata differs from the release",
        ));
    }
    Ok(())
}

fn active_session() -> bool {
    #[cfg(unix)]
    if let Ok(directory) = env::current_dir() {
        use std::os::fd::AsRawFd;
        return active_session_at(&directory, |file| {
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        });
    }
    false
}

#[cfg(unix)]
fn active_session_at(directory: &Path, lock: impl FnOnce(&fs::File) -> io::Result<()>) -> bool {
    use std::os::{fd::AsRawFd, unix::fs::OpenOptionsExt};
    for ancestor in directory.ancestors() {
        let workspace = ancestor.join(".rustrace");
        if !workspace.is_dir() {
            continue;
        }
        // Stop at the nearest workspace. Inspect only a regular existing lock,
        // without following symlinks or blocking on a FIFO.
        let Ok(file) = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(workspace.join("writer.lock"))
        else {
            return false;
        };
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe { libc::fstat(file.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
            return false;
        }
        // fstat succeeded, so the structure is initialized.
        if unsafe { stat.assume_init() }.st_mode & libc::S_IFMT != libc::S_IFREG {
            return false;
        }
        return lock(&file).is_err_and(|error| error.raw_os_error() == Some(libc::EWOULDBLOCK));
    }
    false
}

fn install_update(output: &mut impl Write) -> u8 {
    if active_session() {
        let _ = writeln!(output, "Quit Rustrace before updating.");
        return 2;
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    let Some(path) = state_path() else {
        let _ = writeln!(
            output,
            "Update check unavailable: update state directory unavailable"
        );
        return 2;
    };
    let receipt_path = path.with_file_name("install.json");
    let _ = writeln!(output, "Checking for updates...");
    let _ = output.flush();
    // Read-only ownership decision: unsupported installations do not create
    // either a state file or a lock, even when the explicit request succeeds.
    let owned = InstallReceipt::load(&receipt_path).and_then(|receipt| {
        let root = receipt.install_root(&env::current_exe().ok()?)?;
        Some((receipt, root))
    });
    let Some((mut receipt, root)) = owned else {
        return match fetch_curl(
            Path::new("curl"),
            deadline.saturating_duration_since(Instant::now()),
        )
        .and_then(|bytes| validate_manifest(&bytes))
        {
            Ok(latest) => {
                manual_remedy(output, &latest.tag);
                2
            }
            Err(reason) => {
                let status = update_unavailable(output, &reason);
                manual_remedy(output, "vX.Y.Z");
                let _ = writeln!(
                    output,
                    "See docs/installation.md for installation instructions."
                );
                status
            }
        };
    };
    let _lock = match lock_state(&path, deadline) {
        Ok(lock) => lock,
        Err(error) => return update_unavailable(output, &error.to_string()),
    };
    // A preceding updater may have replaced the executable while we waited.
    // Re-read the receipt under the same lock and preserve its latest version.
    if let Some(current) = InstallReceipt::load(&receipt_path) {
        if current.path != receipt.path
            || current.method != receipt.method
            || current.repository != receipt.repository
        {
            return update_unavailable(output, "installation receipt changed while waiting");
        }
        receipt = current;
    } else {
        return update_unavailable(output, "installation receipt changed while waiting");
    }
    let mut formats = None;
    let latest = match check_locked(&path, unix_seconds(), true, deadline, |bound| {
        let bytes = fetch_curl(Path::new("curl"), bound)?;
        formats = serde_json::from_slice::<ReleaseFormats>(&bytes).ok();
        Ok(bytes)
    }) {
        CheckResult::Known(latest) => latest,
        CheckResult::Unavailable(reason) => return update_unavailable(output, &reason),
        CheckResult::Skipped => unreachable!("explicit check ignores preference and throttle"),
    };
    // The receipt can be newer than this already loaded process after a
    // concurrent successful update; never rebuild or downgrade that copy.
    if !latest.is_newer_than(env!("CARGO_PKG_VERSION")) || !latest.is_newer_than(&receipt.version) {
        // A manual install can leave the receipt older than this executable.
        if let (Some(running), Some(recorded)) = (
            stable_version(env!("CARGO_PKG_VERSION")),
            stable_version(&receipt.version),
        ) && running > recorded
        {
            receipt.version = env!("CARGO_PKG_VERSION").to_owned();
            receipt.tag = format!("v{}", receipt.version);
            if let Err(error) = save_json(&receipt, &receipt_path) {
                let _ = writeln!(
                    output,
                    "Cannot save installation receipt: {}",
                    crate::display::label(&error.to_string(), 1024)
                );
                return 1;
            }
        }
        let _ = writeln!(output, "Already up to date ({}).", receipt.version);
        return 0;
    }
    let Some(formats) = formats else {
        return update_unavailable(output, "release format metadata unavailable");
    };
    let _ = writeln!(
        output,
        "Building {} from source (this takes a few minutes)...",
        latest.version
    );
    let _ = output.flush();
    let result = std::process::Command::new("cargo")
        .args([
            "+1.98.1",
            "install",
            "--git",
            &receipt.repository,
            "--tag",
            &latest.tag,
            "rustrace",
            "--locked",
            "--force",
        ])
        .env("CARGO_INSTALL_ROOT", root)
        .env("RUSTUP_AUTO_INSTALL", "0")
        .status();
    match result {
        Ok(status) if status.success() => {}
        result => {
            if let Err(error) = result {
                let _ = writeln!(
                    output,
                    "Cannot run Cargo: {}",
                    crate::display::label(&error.to_string(), 1024)
                );
            }
            let _ = writeln!(output, "Update failed; the previous Rustrace is unchanged.");
            return 1;
        }
    }
    if let Err(error) = validate_installed(&receipt.path, &latest, &formats) {
        let _ = writeln!(
            output,
            "Installed binary validation failed: {}",
            crate::display::label(&error.to_string(), 1024)
        );
        let _ = writeln!(
            output,
            "The newly built binary is already installed at {}, but its identity could not be confirmed.",
            crate::display::label(&receipt.path.to_string_lossy(), 4096)
        );
        manual_remedy(output, &latest.tag);
        return 1;
    }
    receipt.tag.clone_from(&latest.tag);
    receipt.version.clone_from(&latest.version);
    if let Err(error) = save_json(&receipt, &receipt_path) {
        let _ = writeln!(
            output,
            "Cannot save installation receipt: {}",
            crate::display::label(&error.to_string(), 1024)
        );
        return 1;
    }
    let mut state = UpdateState::load(&path);
    state.latest = Some(latest.clone());
    if let Err(error) = state.save(&path) {
        return update_unavailable(output, &format!("cannot save update state: {error}"));
    }
    let _ = writeln!(
        output,
        "Installed {}. Restart Rustrace to use it.",
        latest.version
    );
    0
}

fn cargo_remedy(tag: &str) -> String {
    format!("cargo +1.98.1 install --git {REPOSITORY} --tag {tag} rustrace --locked --force")
}

fn manual_remedy(output: &mut impl Write, tag: &str) {
    let _ = writeln!(output, "{}", cargo_remedy(tag));
}

fn update_unavailable(output: &mut impl Write, reason: &str) -> u8 {
    let _ = writeln!(
        output,
        "Update check unavailable: {}",
        crate::display::label(reason, 1024)
    );
    2
}

/// Cache-only launch snapshot; opening the menu refreshes it.
pub fn cached_state() -> UpdateState {
    state_path()
        .map(|path| UpdateState::load(&path))
        .unwrap_or_default()
}

pub(crate) fn installer_managed() -> bool {
    match (state_path(), env::current_exe()) {
        (Some(path), Ok(executable)) => installer_managed_at(&path, &executable),
        _ => false,
    }
}

fn installer_managed_at(state_path: &Path, executable: &Path) -> bool {
    InstallReceipt::load(&state_path.with_file_name("install.json"))
        .and_then(|receipt| receipt.install_root(executable))
        .is_some()
}

impl UpdateState {
    pub fn panel_lines(&self, installed: &str, managed: bool, now: u64) -> Vec<String> {
        let latest = self.latest.as_ref();
        let message = match latest {
            None => "Latest release is unknown. Quit, then run: rustrace update --check".into(),
            Some(latest) if !latest.is_newer_than(installed) => "Rustrace is up to date.".into(),
            Some(latest) if !managed => cargo_remedy(&latest.tag),
            Some(latest) => format!(
                "Rustrace {} is available. Quit, then run: rustrace update",
                latest.tag
            ),
        };
        vec![
            format!("Installed: {installed}"),
            format!(
                "Latest known: {}",
                latest.map_or("unknown", |release| release.version.as_str())
            ),
            format!(
                "Last checked: {}",
                self.last_success
                    .map_or_else(|| "never".into(), |checked| relative_time(checked, now))
            ),
            String::new(),
            message,
        ]
    }
}

fn relative_time(checked: u64, now: u64) -> String {
    let elapsed = now.saturating_sub(checked);
    if elapsed < 60 {
        return "just now".into();
    }
    let (count, unit) = if elapsed < 3600 {
        (elapsed / 60, "minute")
    } else if elapsed < DAY {
        (elapsed / 3600, "hour")
    } else {
        (elapsed / DAY, "day")
    };
    format!("{count} {unit}{} ago", if count == 1 { "" } else { "s" })
}

fn cached_advisory(state: &UpdateState, now: u64) -> String {
    let installed = env!("CARGO_PKG_VERSION");
    let checks = if state.checks_enabled { "on" } else { "off" };
    if let Some(latest) = &state.latest {
        if latest.is_newer_than(installed) {
            return format!(
                "WARNING Rustrace version: {} is available; run rustrace update (automatic checks {checks})",
                latest.tag
            );
        }
        if let Some(checked) = state.last_success {
            return format!(
                "OK Rustrace version: {installed} is the latest known (checked {}; automatic checks {checks})",
                relative_time(checked, now)
            );
        }
    }
    format!(
        "OK Rustrace version: {installed} (no update check recorded; automatic checks {checks})"
    )
}

pub(crate) fn write_doctor_advisory(output: &mut impl Write) -> io::Result<()> {
    let state = state_path()
        .map(|path| UpdateState::load(&path))
        .unwrap_or_default();
    writeln!(output, "{}", cached_advisory(&state, unix_seconds()))?;
    // Presence is a filesystem probe, never a subprocess or network request.
    if !curl_available() {
        writeln!(
            output,
            "WARNING update check unavailable: curl is not installed; cached status unchanged"
        )?;
    }
    Ok(())
}

fn curl_available() -> bool {
    env::var_os("PATH").is_some_and(|path| {
        env::split_paths(&path).any(|directory| {
            fs::metadata(directory.join("curl")).is_ok_and(|metadata| {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
                }
                #[cfg(not(unix))]
                {
                    metadata.is_file()
                }
            })
        })
    })
}

#[cfg(test)]
mod tests {
    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    use super::*;
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    fn scratch() -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "rustrace-update-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        path
    }

    #[test]
    fn panel_lines_status_table_checks_release_before_installation_method() {
        let unknown = "Latest release is unknown. Quit, then run: rustrace update --check";
        let current = "Rustrace is up to date.";
        let available = "Rustrace v99.0.0 is available. Quit, then run: rustrace update";
        let cargo = "cargo +1.98.1 install --git https://github.com/baochunli/rustrace --tag v99.0.0 rustrace --locked --force";
        for (version, managed, expected) in [
            (None, true, unknown),
            (None, false, unknown),
            (Some("0.0.0"), true, current),
            (Some("0.0.0"), false, current),
            (Some("0.1.0"), true, current),
            (Some("0.1.0"), false, current),
            (Some("99.0.0"), true, available),
            (Some("99.0.0"), false, cargo),
        ] {
            let state = UpdateState {
                latest: version.map(|version| ReleaseIdentity {
                    version: version.into(),
                    tag: format!("v{version}"),
                    commit: "a".repeat(40),
                }),
                last_success: version.map(|_| 0),
                ..Default::default()
            };
            let lines = state.panel_lines("0.1.0", managed, 7200);
            assert_eq!(
                lines,
                vec![
                    "Installed: 0.1.0".to_owned(),
                    format!("Latest known: {}", version.unwrap_or("unknown")),
                    format!(
                        "Last checked: {}",
                        if version.is_some() {
                            "2 hours ago"
                        } else {
                            "never"
                        }
                    ),
                    String::new(),
                    expected.into(),
                ],
                "latest={version:?}, managed={managed}"
            );
            assert!(lines.iter().all(|line| !line.contains("vX.Y.Z")));
        }
    }

    #[test]
    fn menu_receipt_probe_uses_install_json_and_running_binary() {
        let home = super::test_home::TestHome::new(false);
        let state = home.root.join("state/rustrace/update-state.json");
        let binary = home.root.join("cargo/bin/rustrace");
        fs::create_dir_all(binary.parent().unwrap()).unwrap();
        fs::write(&binary, b"binary").unwrap();
        assert!(!installer_managed_at(&state, &binary));
        let mut receipt = serde_json::json!({"schema_version":1,"method":"cargo-git",
            "path":binary,"tag":"v0.1.0","version":"0.1.0","repository":REPOSITORY});
        for (method, expected) in [("cargo-git", true), ("cargo", false), ("unknown", false)] {
            receipt["method"] = serde_json::json!(method);
            fs::write(
                state.with_file_name("install.json"),
                serde_json::to_vec(&receipt).unwrap(),
            )
            .unwrap();
            assert_eq!(installer_managed_at(&state, &binary), expected);
        }
        receipt["method"] = serde_json::json!("cargo-git");
        fs::write(
            state.with_file_name("install.json"),
            serde_json::to_vec(&receipt).unwrap(),
        )
        .unwrap();
        assert!(!installer_managed_at(
            &state,
            &home.root.join("other/bin/rustrace")
        ));
    }

    #[test]
    fn menu_cache_read_rejects_oversized_valid_json() {
        let home = super::test_home::TestHome::new(false);
        let path = home.root.join("state/rustrace/update-state.json");
        let mut bytes = fs::read(&path).unwrap();
        bytes.resize(BODY_LIMIT + 1, b' ');
        fs::write(&path, bytes).unwrap();
        assert_eq!(UpdateState::load(&path), UpdateState::default());
    }

    #[cfg(unix)]
    #[test]
    fn menu_cache_and_receipt_reads_reject_fifos_without_waiting_for_writer() {
        use std::{ffi::CString, os::unix::ffi::OsStrExt};
        let home = super::test_home::TestHome::new(false);
        let state = home.root.join("state/rustrace/update-state.json");
        fs::remove_file(&state).unwrap();
        for path in [&state, &state.with_file_name("install.json")] {
            let name = CString::new(path.as_os_str().as_bytes()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        }
        assert_eq!(UpdateState::load(&state), UpdateState::default());
        assert!(!installer_managed_at(
            &state,
            &home.root.join("bin/rustrace")
        ));
    }

    #[test]
    fn receipt_ownership_requires_cargo_git_and_the_running_cargo_binary() {
        let root = scratch();
        let binary = root.join("cargo/bin/rustrace");
        fs::create_dir_all(binary.parent().unwrap()).unwrap();
        fs::write(&binary, b"old binary").unwrap();
        let path = root.join("install.json");
        assert!(InstallReceipt::load(&path).is_none());
        let mut value = serde_json::json!({"schema_version":1,"method":"cargo-git",
            "path":binary,"tag":"v0.1.0","version":"0.1.0","repository":REPOSITORY});
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        let receipt = InstallReceipt::load(&path).unwrap();
        assert_eq!(receipt.install_root(&binary), Some(root.join("cargo")));
        assert_eq!(receipt.install_root(&root.join("other/bin/rustrace")), None);
        // Logical roots from the installer may contain symlinks.
        std::os::unix::fs::symlink(root.join("cargo"), root.join("linked")).unwrap();
        value["path"] = serde_json::json!(root.join("linked/bin/rustrace"));
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(
            InstallReceipt::load(&path).unwrap().install_root(&binary),
            Some(root.join("linked"))
        );
        for (key, bad) in [
            ("method", serde_json::json!("cargo")),
            ("schema_version", serde_json::json!(2)),
            ("path", serde_json::json!(root.join("rustrace"))),
        ] {
            let mut bad_value = value.clone();
            bad_value[key] = bad;
            fs::write(&path, serde_json::to_vec(&bad_value).unwrap()).unwrap();
            assert!(
                InstallReceipt::load(&path)
                    .and_then(|r| r.install_root(&binary))
                    .is_none()
            );
        }
        fs::write(&path, b"bad JSON").unwrap();
        assert!(InstallReceipt::load(&path).is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn automatic_check_with_less_than_curl_reserve_keeps_daily_eligibility() {
        let _serial = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let root = scratch();
        let path = root.join("update-state.json");
        UpdateState::default().save(&path).unwrap();
        let before = fs::read(&path).unwrap();
        let result = check_until(
            &path,
            100_000,
            false,
            Instant::now() + Duration::from_millis(40),
            |_| panic!("curl reserve exhausted"),
        );
        assert_eq!(result, CheckResult::Skipped);
        assert_eq!(fs::read(&path).unwrap(), before);
        assert!(UpdateState::load(&path).eligible(100_000));
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn contended_automatic_check_does_not_claim_attempt_after_late_lock() {
        let _serial = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let root = scratch();
        let path = root.join("update-state.json");
        UpdateState::default().save(&path).unwrap();
        let before = fs::read(&path).unwrap();
        let lock = lock_state(&path, Instant::now() + Duration::from_secs(1)).unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(75));
            drop(lock);
        });
        let result = check_until(
            &path,
            100_000,
            false,
            Instant::now() + Duration::from_millis(100),
            |_| panic!("curl reserve exhausted after lock"),
        );
        release.join().unwrap();
        assert_eq!(result, CheckResult::Skipped);
        assert_eq!(fs::read(&path).unwrap(), before);
        assert!(UpdateState::load(&path).eligible(100_000));
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn active_session_probe_only_classifies_would_block_as_active() {
        let root = scratch();
        fs::create_dir(root.join(".rustrace")).unwrap();
        fs::write(root.join(".rustrace/writer.lock"), b"").unwrap();
        for error in [libc::EOPNOTSUPP, libc::EACCES, libc::EBADF] {
            assert!(
                !active_session_at(&root, |_| Err(io::Error::from_raw_os_error(error))),
                "flock error {error} is not an active session"
            );
        }
        assert!(active_session_at(&root, |_| Err(
            io::Error::from_raw_os_error(libc::EWOULDBLOCK)
        )));
        assert!(!active_session_at(&root, |_| Ok(())));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn state_absent_malformed_unknown_and_round_trip() {
        let _serial = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let root = scratch();
        let path = root.join("rustrace/update-state.json");
        assert_eq!(UpdateState::load(&path), UpdateState::default());
        let state = UpdateState {
            checks_enabled: false,
            last_attempt: Some(10),
            last_success: Some(9),
            next_eligible: Some(10 + DAY),
            ..UpdateState::default()
        };
        state.save(&path).unwrap();
        assert_eq!(UpdateState::load(&path), state);
        let canonical = fs::read(&path).unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&canonical).unwrap()["schema_version"],
            1
        );
        for body in [
            b"{bad".as_slice(),
            b"null",
            b"{}",
            b"{\"schema_version\":2}",
        ] {
            fs::write(&path, body).unwrap();
            assert_eq!(UpdateState::load(&path), UpdateState::default());
        }
        let mut value: serde_json::Value = serde_json::from_slice(&canonical).unwrap();
        value["unexpected"] = true.into();
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(UpdateState::load(&path), UpdateState::default());
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn state_paths_are_separate_from_config_and_empty_xdg_falls_back() {
        let _serial = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        use std::ffi::OsStr;
        assert_eq!(
            state_file_path(Some(OsStr::new("/state")), Some(OsStr::new("/home"))),
            Some(PathBuf::from("/state/rustrace/update-state.json"))
        );
        assert_eq!(
            state_file_path(Some(OsStr::new("")), Some(OsStr::new("/home"))),
            Some(PathBuf::from(
                "/home/.local/state/rustrace/update-state.json"
            ))
        );
        assert_eq!(state_file_path(None, None), None);
    }

    #[test]
    fn throttle_boundary_and_clock_skew_repair() {
        let _serial = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let now = 100_000;
        let mut state = UpdateState {
            last_attempt: Some(now),
            last_success: Some(now),
            next_eligible: Some(now + DAY),
            ..UpdateState::default()
        };
        assert!(!state.eligible(now + DAY - 1));
        assert!(state.eligible(now + DAY));
        assert!(state.eligible(now - 1));
        assert_eq!(
            (state.last_attempt, state.last_success, state.next_eligible),
            (None, None, None)
        );
        state.last_attempt = Some(now - 1);
        state.next_eligible = Some(now + DAY + 1);
        assert!(state.eligible(now));
        assert_eq!((state.last_attempt, state.next_eligible), (None, None));
        state.checks_enabled = false;
        assert!(!state.eligible(now));
    }

    #[test]
    fn atomic_save_failure_preserves_existing_destination_and_removes_temp() {
        let _serial = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let root = scratch();
        let path = root.join("destination");
        fs::create_dir(&path).unwrap();
        assert!(UpdateState::default().save(&path).is_err());
        assert!(path.is_dir());
        assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
        fs::remove_dir_all(root).unwrap();
    }
    fn manifest(version: &str) -> serde_json::Value {
        serde_json::json!({"schema_version":1, "version":version, "tag":format!("v{version}"),
            "commit":"0123456789abcdef0123456789abcdef01234567", "event_format":1,
            "package_format":1, "assignment_format":2,
            "source":{"repository":"https://github.com/baochunli/rustrace", "tag":format!("v{version}")},
            "targets":{}})
    }

    #[test]
    fn manifest_validation_and_semver_ordering_table() {
        let _serial = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        for (version, newer) in [
            ("0.0.9", false),
            ("0.1.0", false),
            ("0.1.1", true),
            ("0.10.0", true),
            ("1.0.0", true),
            ("10.0.0", true),
        ] {
            let identity =
                validate_manifest(&serde_json::to_vec(&manifest(version)).unwrap()).unwrap();
            assert_eq!(identity.is_newer_than("0.1.0"), newer, "{version}");
        }
        for (latest, installed, newer) in [
            ("1.10.0", "1.9.0", true),
            ("1.9.0", "1.10.0", false),
            ("1.0.10", "1.0.9", true),
            ("10.0.0", "9.99.99", true),
            ("2.0.0", "10.0.0", false),
        ] {
            let identity =
                validate_manifest(&serde_json::to_vec(&manifest(latest)).unwrap()).unwrap();
            assert_eq!(
                identity.is_newer_than(installed),
                newer,
                "{latest} vs {installed}"
            );
        }
        for version in [
            "",
            "v1.0.0",
            "1.0",
            "01.0.0",
            "1.00.0",
            "1.0.00",
            "1.0.0-rc.1",
            "1.0.0+build",
            "1.-1.0",
            "1.0.0.0",
            "1.0. 0",
            "18446744073709551616.0.0",
        ] {
            assert!(
                validate_manifest(&serde_json::to_vec(&manifest(version)).unwrap()).is_err(),
                "{version}"
            );
        }
        for (key, value) in [
            ("schema_version", serde_json::json!(2)),
            ("tag", serde_json::json!("v2.0.0")),
            (
                "commit",
                serde_json::json!("g123456789abcdef0123456789abcdef01234567"),
            ),
            ("commit", serde_json::json!("123")),
            ("event_format", serde_json::json!("1")),
            ("package_format", serde_json::json!(1.5)),
            ("assignment_format", serde_json::json!(null)),
            ("targets", serde_json::json!(["asset"])),
            (
                "commit",
                serde_json::json!("ABCDEF0123456789abcdef0123456789abcdef01"),
            ),
            (
                "source",
                serde_json::json!({"repository":"https://evil.example", "tag":"v1.0.0"}),
            ),
        ] {
            let mut value_to_test = manifest("1.0.0");
            value_to_test[key] = value;
            assert!(
                validate_manifest(&serde_json::to_vec(&value_to_test).unwrap()).is_err(),
                "{key}: {value_to_test}"
            );
        }
        for key in [
            "schema_version",
            "version",
            "tag",
            "commit",
            "event_format",
            "package_format",
            "assignment_format",
            "source",
            "targets",
        ] {
            let mut value = manifest("1.0.0");
            value.as_object_mut().unwrap().remove(key);
            assert!(
                validate_manifest(&serde_json::to_vec(&value).unwrap()).is_err(),
                "missing {key}"
            );
        }
        let mut future_targets = manifest("1.0.0");
        future_targets["targets"] = serde_json::json!({"linux-x86_64":{"url":"future-asset"}});
        assert!(validate_manifest(&serde_json::to_vec(&future_targets).unwrap()).is_ok());
        assert!(validate_manifest(b"{bad").is_err());
    }

    #[test]
    fn invalid_cached_release_is_absent() {
        let _serial = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let root = scratch();
        let path = root.join("state.json");
        let state = UpdateState {
            latest: Some(ReleaseIdentity {
                version: "garbage".into(),
                tag: "bad".into(),
                commit: "bad".into(),
            }),
            ..UpdateState::default()
        };
        state.save(&path).unwrap();
        assert_eq!(UpdateState::load(&path), UpdateState::default());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn checker_throttles_across_loads_counts_failures_and_preserves_cache() {
        let _serial = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let root = scratch();
        let path = root.join("state.json");
        let now = 100_000;
        let fetch = |_: std::time::Duration| Ok(serde_json::to_vec(&manifest("1.0.0")).unwrap());
        assert!(matches!(
            check_with(&path, now, false, fetch),
            CheckResult::Known(_)
        ));
        assert_eq!(UpdateState::load(&path).last_success, Some(now));
        assert_eq!(
            check_with(&path, now + 1, false, |_| panic!("throttled")),
            CheckResult::Skipped
        );
        assert!(matches!(
            check_with(&path, now + DAY, false, |_| Err("offline".into())),
            CheckResult::Unavailable(_)
        ));
        let failed = UpdateState::load(&path);
        assert_eq!(failed.latest.unwrap().version, "1.0.0");
        assert_eq!(failed.last_success, Some(now));
        assert_eq!(failed.last_attempt, Some(now + DAY));
        assert_eq!(failed.next_eligible, Some(now + 2 * DAY));
        assert_eq!(
            check_with(&path, now + DAY + 1, false, |_| panic!(
                "failed attempts throttle"
            )),
            CheckResult::Skipped
        );
        let mut state = UpdateState::load(&path);
        state.checks_enabled = false;
        state.save(&path).unwrap();
        assert_eq!(
            check_with(&path, now + 3 * DAY, false, |_| panic!("disabled")),
            CheckResult::Skipped
        );
        assert!(matches!(
            check_with(&path, now + DAY + 2, true, fetch),
            CheckResult::Known(_)
        ));
        assert!(!UpdateState::load(&path).checks_enabled);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn checker_newer_equal_older_bad_json_and_skew() {
        let _serial = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let root = scratch();
        let path = root.join("state.json");
        for (version, now) in [("1.0.0", 100_000), ("0.1.0", 100_001), ("0.0.9", 100_002)] {
            assert!(matches!(
                check_with(&path, now, true, |_| Ok(serde_json::to_vec(&manifest(
                    version
                ))
                .unwrap())),
                CheckResult::Known(_)
            ));
            assert_eq!(UpdateState::load(&path).latest.unwrap().version, version);
        }
        assert!(matches!(
            check_with(&path, 100_003, true, |_| Ok(b"bad JSON".to_vec())),
            CheckResult::Unavailable(_)
        ));
        assert_eq!(UpdateState::load(&path).latest.unwrap().version, "0.0.9");
        assert!(matches!(
            check_with(&path, 90_000, false, |_| Ok(serde_json::to_vec(&manifest(
                "1.0.0"
            ))
            .unwrap())),
            CheckResult::Known(_)
        ));
        assert_eq!(UpdateState::load(&path).next_eligible, Some(90_000 + DAY));
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn curl_literal_argv_bounded_body_absence_and_kill_reap_deadline() {
        let _serial = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        use std::{
            os::unix::fs::PermissionsExt,
            time::{Duration, Instant},
        };
        let root = scratch();
        let curl = root.join("curl");
        let request_log = root.join("argv.json");
        let pid_file = root.join("pid");
        let script = format!(
            "#!/usr/bin/python3\nimport json,sys,os,time\njson.dump(sys.argv[1:],open({:?},'w'))\nopen({:?},'w').write(str(os.getpid()))\ntime.sleep(5)\n",
            request_log, pid_file
        );
        fs::write(&curl, script).unwrap();
        fs::set_permissions(&curl, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            fetch_curl(&curl, Duration::ZERO)
                .unwrap_err()
                .contains("before curl started")
        );
        assert!(
            !request_log.exists(),
            "expired budget must not spawn a request"
        );
        let start = Instant::now();
        assert!(
            fetch_curl(&curl, Duration::from_secs(1))
                .unwrap_err()
                .contains("timed out")
        );
        assert!(
            start.elapsed() < Duration::from_millis(1200),
            "{:?}",
            start.elapsed()
        );
        let pid: i32 = fs::read_to_string(&pid_file).unwrap().parse().unwrap();
        assert_eq!(
            unsafe { libc::kill(pid, 0) },
            -1,
            "child still exists (including zombies)"
        );
        let args: Vec<String> = serde_json::from_slice(&fs::read(&request_log).unwrap()).unwrap();
        assert_eq!(
            args,
            [
                "--disable",
                "--fail",
                "--silent",
                "--show-error",
                "--location",
                "--max-time",
                "1",
                "--max-filesize",
                "65536",
                ENDPOINT
            ]
        );
        assert_eq!(
            fetch_curl(&root.join("missing-curl"), Duration::from_secs(1)).unwrap_err(),
            "curl is not installed"
        );
        fs::write(&curl, "#!/usr/bin/python3\nimport sys\nsys.stdout.write('x'*70000)\nsys.stderr.write('e'*70000)\n").unwrap();
        assert!(
            fetch_curl(&curl, Duration::from_secs(1))
                .unwrap_err()
                .contains("65536")
        );
        fs::write(
            &curl,
            "#!/usr/bin/python3\nimport sys\nsys.stderr.write('fixture offline')\nsys.exit(22)\n",
        )
        .unwrap();
        assert!(
            fetch_curl(&curl, Duration::from_secs(1))
                .unwrap_err()
                .contains("fixture offline")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn checker_persists_attempt_before_fetch_and_uses_different_deadlines() {
        let _serial = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        use std::time::Duration;
        let root = scratch();
        let path = root.join("state.json");
        for (explicit, now, bound) in [(false, 100_000, 1), (true, 100_001, 20)] {
            let result = check_with(&path, now, explicit, |deadline| {
                assert!(deadline <= Duration::from_secs(bound));
                assert!(deadline > Duration::from_secs(bound - 1));
                let state = UpdateState::load(&path);
                assert_eq!(state.last_attempt, Some(now));
                assert_eq!(state.next_eligible, Some(now + DAY));
                Err("curl is not installed".into())
            });
            assert!(matches!(result, CheckResult::Unavailable(_)));
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn explicit_failed_check_repairs_future_success_without_losing_release() {
        let _serial = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let root = scratch();
        let path = root.join("state.json");
        let state = UpdateState {
            last_attempt: Some(200_000),
            last_success: Some(200_000),
            next_eligible: Some(200_000 + DAY),
            latest: Some(
                validate_manifest(&serde_json::to_vec(&manifest("1.0.0")).unwrap()).unwrap(),
            ),
            ..UpdateState::default()
        };
        state.save(&path).unwrap();
        assert!(matches!(
            check_with(&path, 100_000, true, |_| Err("offline".into())),
            CheckResult::Unavailable(_)
        ));
        let repaired = UpdateState::load(&path);
        assert_eq!(repaired.last_success, None);
        assert_eq!(repaired.last_attempt, Some(100_000));
        assert_eq!(repaired.next_eligible, Some(100_000 + DAY));
        assert_eq!(repaired.latest, state.latest);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn preference_toggle_is_network_free_and_preserves_cache() {
        let _serial = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let root = scratch();
        let path = root.join("state.json");
        let mut state = UpdateState {
            last_attempt: Some(100_000),
            last_success: Some(100_000),
            next_eligible: Some(100_000 + DAY),
            latest: Some(
                validate_manifest(&serde_json::to_vec(&manifest("1.0.0")).unwrap()).unwrap(),
            ),
            ..UpdateState::default()
        };
        state.save(&path).unwrap();
        set_checks_enabled_at(&path, false).unwrap();
        state.checks_enabled = false;
        assert_eq!(UpdateState::load(&path), state);
        assert_eq!(
            check_with(&path, 300_000, false, |_| panic!(
                "toggle must disable networking"
            )),
            CheckResult::Skipped
        );
        set_checks_enabled_at(&path, true).unwrap();
        state.checks_enabled = true;
        assert_eq!(UpdateState::load(&path), state);
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn concurrent_launch_cannot_fetch_or_overwrite_preference() {
        let _serial = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let root = scratch();
        let path = root.join("state.json");
        let first = check_with(&path, 100_000, false, |_| {
            let waiting = Instant::now();
            assert!(matches!(
                check_with(&path, 100_000, false, |_| panic!("concurrent fetch")),
                CheckResult::Skipped
            ));
            assert!(waiting.elapsed() >= Duration::from_secs(1));
            assert!(
                set_checks_enabled_until(&path, false, Instant::now() + Duration::from_millis(20))
                    .is_err()
            );
            Ok(serde_json::to_vec(&manifest("1.0.0")).unwrap())
        });
        assert!(matches!(first, CheckResult::Known(_)));
        assert!(UpdateState::load(&path).checks_enabled);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn disabled_automatic_check_leaves_state_directory_untouched() {
        let _serial = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let root = scratch();
        let path = root.join("state/update-state.json");
        UpdateState {
            checks_enabled: false,
            ..UpdateState::default()
        }
        .save(&path)
        .unwrap();
        let before = fs::read(&path).unwrap();
        assert_eq!(
            check_with(&path, 100_000, false, |_| panic!("disabled")),
            CheckResult::Skipped
        );
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn lock_wait_reduces_fetch_budget_and_descriptor_is_cloexec() {
        use std::{os::fd::AsRawFd, sync::mpsc, thread};
        let _serial = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let root = scratch();
        let path = root.join("state.json");
        let (ready, held) = mpsc::channel();
        let other_path = path.clone();
        let holder = thread::spawn(move || {
            let lock = lock_state(&other_path, Instant::now() + Duration::from_secs(1)).unwrap();
            assert_ne!(
                unsafe { libc::fcntl(lock.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
                0
            );
            ready.send(()).unwrap();
            thread::sleep(Duration::from_millis(150));
            drop(lock);
        });
        held.recv().unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        let result = check_until(&path, 100_000, false, deadline, |remaining| {
            assert!(
                remaining < Duration::from_millis(900),
                "lock wait was not deducted: {remaining:?}"
            );
            assert!(
                remaining
                    <= deadline.saturating_duration_since(Instant::now())
                        + Duration::from_millis(1)
            );
            Ok(serde_json::to_vec(&manifest("1.0.0")).unwrap())
        });
        holder.join().unwrap();
        assert!(matches!(result, CheckResult::Known(_)), "{result:?}");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cached_freshness_uses_last_success_and_cannot_underflow() {
        let _serial = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        for (now, expected) in [
            (1, "just now"),
            (59, "just now"),
            (60, "1 minute ago"),
            (120, "2 minutes ago"),
            (3600, "1 hour ago"),
            (7200, "2 hours ago"),
            (DAY, "1 day ago"),
            (2 * DAY, "2 days ago"),
        ] {
            assert_eq!(relative_time(0, now), expected);
        }
        assert_eq!(relative_time(100, 0), "just now");
        let state = UpdateState {
            last_success: Some(0),
            last_attempt: Some(DAY),
            latest: Some(
                validate_manifest(
                    &serde_json::to_vec(&manifest(env!("CARGO_PKG_VERSION"))).unwrap(),
                )
                .unwrap(),
            ),
            ..UpdateState::default()
        };
        assert!(
            cached_advisory(&state, 2 * DAY).ends_with("(checked 2 days ago; automatic checks on)")
        );
    }
    #[cfg(unix)]
    #[test]
    fn inherited_fork_descriptor_cannot_spuriously_block_the_next_check() {
        let _serial = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        use std::{os::fd::AsRawFd, os::unix::net::UnixStream, thread};
        let root = scratch();
        let path = root.join("state.json");
        let (reader, mut writer) = UnixStream::pair().unwrap();
        let read_fd = reader.as_raw_fd();
        let write_fd = writer.as_raw_fd();
        let mut pid = -1;
        let first = check_with(&path, 100_000, true, |_| {
            // Model another thread forking while our CLOEXEC flock is held.
            // The child performs only raw syscalls until _exit, no Rust runtime.
            pid = unsafe { libc::fork() };
            assert!(pid >= 0);
            if pid == 0 {
                unsafe {
                    libc::close(write_fd);
                    let mut byte = 0_u8;
                    libc::read(read_fd, (&mut byte as *mut u8).cast(), 1);
                    libc::_exit(0);
                }
            }
            Ok(serde_json::to_vec(&manifest("1.0.0")).unwrap())
        });
        assert!(matches!(first, CheckResult::Known(_)), "{first:?}");
        drop(reader);
        let release = thread::spawn(move || {
            thread::sleep(Duration::from_millis(150));
            writer.write_all(b"x").unwrap();
        });
        let toggled = set_checks_enabled_at(&path, true);
        let second = check_with(&path, 100_001, true, |_| {
            Ok(serde_json::to_vec(&manifest("1.0.0")).unwrap())
        });
        release.join().unwrap();
        unsafe {
            libc::waitpid(pid, std::ptr::null_mut(), 0);
        }
        fs::remove_dir_all(root).unwrap();
        assert!(
            toggled.is_ok(),
            "inherited descriptor blocked preference: {toggled:?}"
        );
        assert!(matches!(second, CheckResult::Known(_)), "{second:?}");
    }
}
