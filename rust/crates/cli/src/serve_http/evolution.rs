//! Self-evolution pipeline (AutoGenesis SEPL borrow).
//!
//! Every self-modification goes through Reflect → [shadow] → Evaluate →
//! Commit, where **Evaluate gates Commit**:
//!
//!   * Shadow phase: a proposal is written to a `.shadow` sibling.
//!   * Evaluate gate: a smoke result must pass; rejected proposals stay
//!     outside the collector namespace even when cleanup fails.
//!   * Commit: the exact claimed proposal is copied to a complete promotion
//!     file, the prior live file is durably snapshotted, then promotion is
//!     atomically exchanged with live after a version check.
//!   * Rollback: the newest complete snapshot atomically replaces live.
//!
//! Linux performs every production operation relative to verified directory
//! descriptors. Other targets fail closed until they have an equivalent
//! handle-relative backend; they never fall back to check-then-use path writes.

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use uuid::Uuid;

static LAST_SNAPSHOT_ORDER: AtomicU64 = AtomicU64::new(0);
static EVOLUTION_TRANSACTION: Mutex<()> = Mutex::new(());

const TRANSACTION_PREFIX: &str = ".nonoclaw-transaction-";
const TRANSACTION_SUFFIX: &str = ".tmp";

#[derive(Debug, PartialEq)]
pub enum RecoveryLiveState {
    Proposal,
    Other,
    Missing,
    Unknown,
}

#[derive(Debug, PartialEq)]
pub enum CommitOutcome {
    Committed,
    NoShadow,
    Rejected(String),
    RecoveryRequired {
        transaction: String,
        live_state: RecoveryLiveState,
    },
}

#[cfg(not(target_os = "linux"))]
fn unsupported_backend() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "self-evolution requires a handle-relative filesystem backend",
    )
}

fn invalid_resource(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn permission_denied(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message)
}

fn live_changed_error(stage: &'static str) -> io::Error {
    io::Error::new(
        io::ErrorKind::WouldBlock,
        format!("live evolution resource changed {stage}"),
    )
}

fn shadow_name(file_name: &OsStr) -> OsString {
    let mut name = file_name.to_os_string();
    name.push(".shadow");
    name
}

fn temporary_name(purpose: &str) -> OsString {
    OsString::from(format!(
        ".nonoclaw-{purpose}-{}-{}.tmp",
        std::process::id(),
        Uuid::new_v4().simple()
    ))
}

fn next_snapshot_order(floor: u64) -> io::Result<u64> {
    if floor == u64::MAX {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "evolution snapshot order is exhausted",
        ));
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos().min(u64::MAX as u128) as u64)
        .unwrap_or(0);
    let minimum = floor
        .checked_add(1)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "snapshot order overflow"))?;
    let mut previous = LAST_SNAPSHOT_ORDER.load(Ordering::Relaxed);
    loop {
        let after_previous = previous
            .checked_add(1)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "snapshot order overflow"))?;
        let next = now.max(minimum).max(after_previous);
        match LAST_SNAPSHOT_ORDER.compare_exchange_weak(
            previous,
            next,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return Ok(next),
            Err(current) => previous = current,
        }
    }
}

fn encoded_resource_components(resource: &str) -> Vec<String> {
    let mut encoded = String::with_capacity(resource.len() * 2);
    for byte in resource.as_bytes() {
        use std::fmt::Write as _;
        let _ = write!(encoded, "{byte:02x}");
    }
    if encoded.is_empty() {
        return vec!["_".to_string()];
    }
    encoded
        .as_bytes()
        .chunks(120)
        .map(|component| {
            std::str::from_utf8(component)
                .expect("hex is UTF-8")
                .to_string()
        })
        .collect()
}

/// Legacy history flattened path separators to `_`, so nested skill paths or
/// literal underscores cannot be mapped back to one resource. Retain fallback
/// only where the old filename is injective within the evolvable namespace.
fn legacy_resource_ids(resource: &str) -> Option<[String; 2]> {
    let unambiguous = resource == "APPEND_SYSTEM.md"
        || resource
            .strip_prefix("skills/")
            .is_some_and(|relative| !relative.contains(['/', '\\', '_']));
    unambiguous.then(|| {
        [
            resource.replace(['/', '\\'], "_"),
            format!("{resource}.shadow").replace(['/', '\\'], "_"),
        ]
    })
}

/// The default gate accepts only a completed run with reward ≥ 0.5.
pub fn default_gate(latest_status: &str, latest_reward: f64) -> Result<(), String> {
    if latest_status == "done" && latest_reward >= 0.5 {
        Ok(())
    } else {
        Err(format!(
            "gate: status={latest_status} reward={latest_reward:.2}"
        ))
    }
}

#[cfg(target_os = "linux")]
mod secure {
    use std::ffi::CString;
    use std::fs::{File, Permissions};
    use std::io::{self, Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::path::{Component, Path, PathBuf};

    use super::*;

    #[derive(Debug)]
    struct SecureDir {
        file: File,
    }

    impl SecureDir {
        fn c_name(name: &OsStr) -> io::Result<CString> {
            let bytes = name.as_bytes();
            if bytes.is_empty() || bytes.contains(&b'/') {
                return Err(invalid_resource("directory-relative name is invalid"));
            }
            CString::new(bytes)
                .map_err(|_| invalid_resource("directory-relative name contains NUL"))
        }

        fn open_verified_ambient(path: &Path) -> io::Result<Self> {
            let before = std::fs::symlink_metadata(path)?;
            if !before.file_type().is_dir() {
                return Err(permission_denied("trusted root is not a direct directory"));
            }
            let canonical = std::fs::canonicalize(path)?;
            let path = CString::new(canonical.as_os_str().as_bytes())
                .map_err(|_| invalid_resource("trusted root contains NUL"))?;
            // SAFETY: `path` is NUL-terminated and retained for the call. A
            // successful descriptor is newly owned and transferred to `File`.
            let descriptor = unsafe {
                libc::open(
                    path.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                )
            };
            if descriptor < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: successful `open` returned an owned descriptor.
            let file = unsafe { File::from_raw_fd(descriptor) };
            let after = file.metadata()?;
            if before.dev() != after.dev() || before.ino() != after.ino() {
                return Err(permission_denied("trusted root changed while opening"));
            }
            Ok(Self { file })
        }

        fn try_clone(&self) -> io::Result<Self> {
            Ok(Self {
                file: self.file.try_clone()?,
            })
        }

        fn open_dir(&self, name: &OsStr) -> io::Result<Self> {
            let name = Self::c_name(name)?;
            // SAFETY: parent fd and component are valid; O_NOFOLLOW rejects a
            // symlink/reparse-style final component.
            let descriptor = unsafe {
                libc::openat(
                    self.file.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                )
            };
            if descriptor < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: successful `openat` returned an owned descriptor.
            Ok(Self {
                file: unsafe { File::from_raw_fd(descriptor) },
            })
        }

        fn open_optional_dir(&self, name: &OsStr) -> io::Result<Option<Self>> {
            match self.open_dir(name) {
                Ok(directory) => Ok(Some(directory)),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(error),
            }
        }

        fn ensure_dir(&self, name: &OsStr) -> io::Result<Self> {
            let name_c = Self::c_name(name)?;
            // SAFETY: parent fd and component are valid and retained. mkdirat
            // has no pointer ownership side effects.
            let result =
                unsafe { libc::mkdirat(self.file.as_raw_fd(), name_c.as_ptr(), libc::S_IRWXU) };
            let created = result == 0;
            if !created {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::AlreadyExists {
                    return Err(error);
                }
            }
            let directory = self.open_dir(name)?;
            if created {
                // Make both the new inode and its parent entry durable before
                // a later cutover can depend on descendants remaining reachable.
                directory.sync()?;
                self.sync()?;
            }
            Ok(directory)
        }

        fn open_regular(&self, name: &OsStr) -> io::Result<File> {
            let name = Self::c_name(name)?;
            // O_NONBLOCK prevents a raced FIFO/device from blocking before
            // fstat rejects it as non-regular.
            // SAFETY: parent fd and component are valid for this call.
            let descriptor = unsafe {
                libc::openat(
                    self.file.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
                )
            };
            if descriptor < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: successful `openat` returned an owned descriptor.
            let file = unsafe { File::from_raw_fd(descriptor) };
            if !file.metadata()?.is_file() {
                return Err(permission_denied("evolution entry is not a regular file"));
            }
            Ok(file)
        }

        fn create_new(&self, name: &OsStr) -> io::Result<File> {
            let name = Self::c_name(name)?;
            // SAFETY: parent fd and component are valid. O_EXCL guarantees a
            // new object and O_NOFOLLOW rejects a raced symlink.
            let descriptor = unsafe {
                libc::openat(
                    self.file.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_WRONLY
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_CLOEXEC
                        | libc::O_NOFOLLOW,
                    libc::S_IRUSR | libc::S_IWUSR,
                )
            };
            if descriptor < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: successful `openat` returned an owned descriptor.
            Ok(unsafe { File::from_raw_fd(descriptor) })
        }

        fn rename_noreplace(&self, source: &OsStr, destination: &OsStr) -> io::Result<()> {
            let source = Self::c_name(source)?;
            let destination = Self::c_name(destination)?;
            // SAFETY: both names and the directory descriptor are valid.
            let result = unsafe {
                libc::renameat2(
                    self.file.as_raw_fd(),
                    source.as_ptr(),
                    self.file.as_raw_fd(),
                    destination.as_ptr(),
                    libc::RENAME_NOREPLACE,
                )
            };
            if result == 0 {
                return Ok(());
            }
            let error = io::Error::last_os_error();
            if !error
                .raw_os_error()
                .is_some_and(|code| matches!(code, libc::ENOSYS | libc::EINVAL))
            {
                return Err(error);
            }

            // Old kernels/filesystems: link+unlink remains no-clobber. The
            // process transaction lock and Dream's cross-process lock prevent
            // another cooperating collector from observing the short overlap.
            // SAFETY: descriptors and names are valid and retained.
            let linked = unsafe {
                libc::linkat(
                    self.file.as_raw_fd(),
                    source.as_ptr(),
                    self.file.as_raw_fd(),
                    destination.as_ptr(),
                    0,
                )
            };
            if linked != 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: directory descriptor and source name are valid.
            let removed = unsafe { libc::unlinkat(self.file.as_raw_fd(), source.as_ptr(), 0) };
            if removed != 0 {
                let error = io::Error::last_os_error();
                // SAFETY: cleanup uses the same validated directory/name.
                unsafe {
                    libc::unlinkat(self.file.as_raw_fd(), destination.as_ptr(), 0);
                }
                return Err(error);
            }
            Ok(())
        }

        fn replace(&self, source: &OsStr, destination: &OsStr) -> io::Result<()> {
            let source = Self::c_name(source)?;
            let destination = Self::c_name(destination)?;
            // SAFETY: both names and the directory descriptor are valid;
            // renameat atomically replaces a regular destination on Linux.
            let result = unsafe {
                libc::renameat(
                    self.file.as_raw_fd(),
                    source.as_ptr(),
                    self.file.as_raw_fd(),
                    destination.as_ptr(),
                )
            };
            if result == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        }

        fn exchange(&self, left: &OsStr, right: &OsStr) -> io::Result<()> {
            let left = Self::c_name(left)?;
            let right = Self::c_name(right)?;
            // SAFETY: both names and the directory descriptor are valid;
            // RENAME_EXCHANGE swaps the two entries atomically.
            let result = unsafe {
                libc::renameat2(
                    self.file.as_raw_fd(),
                    left.as_ptr(),
                    self.file.as_raw_fd(),
                    right.as_ptr(),
                    libc::RENAME_EXCHANGE,
                )
            };
            if result == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        }

        fn hard_link(&self, source: &OsStr, destination: &OsStr) -> io::Result<()> {
            let source = Self::c_name(source)?;
            let destination = Self::c_name(destination)?;
            // SAFETY: both names and the directory descriptor are valid.
            let result = unsafe {
                libc::linkat(
                    self.file.as_raw_fd(),
                    source.as_ptr(),
                    self.file.as_raw_fd(),
                    destination.as_ptr(),
                    0,
                )
            };
            if result == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        }

        fn unlink(&self, name: &OsStr) -> io::Result<()> {
            let name = Self::c_name(name)?;
            // SAFETY: directory descriptor and component are valid.
            let result = unsafe { libc::unlinkat(self.file.as_raw_fd(), name.as_ptr(), 0) };
            if result == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        }

        fn entry_names(&self) -> io::Result<Vec<OsString>> {
            let path = PathBuf::from(format!("/proc/self/fd/{}", self.file.as_raw_fd()));
            std::fs::read_dir(path)?
                .map(|entry| entry.map(|entry| entry.file_name()))
                .collect()
        }

        fn sync(&self) -> io::Result<()> {
            self.file.sync_all()
        }
    }

    #[derive(Debug)]
    struct ValidatedResource {
        identity: String,
        root: SecureDir,
        parent: SecureDir,
        file_name: OsString,
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct LiveVersion {
        device: u64,
        inode: u64,
        len: u64,
        modified_seconds: i64,
        modified_nanos: i64,
        changed_seconds: i64,
        changed_nanos: i64,
    }

    impl LiveVersion {
        fn same_file(&self, other: &Self) -> bool {
            self.device == other.device && self.inode == other.inode
        }

        /// Rename/exchange updates ctime on Linux, so post-exchange identity
        /// compares the stable inode and content metadata while pre-publish
        /// checks still use full equality (including ctime).
        fn same_file_content(&self, other: &Self) -> bool {
            self.same_file(other)
                && self.len == other.len
                && self.modified_seconds == other.modified_seconds
                && self.modified_nanos == other.modified_nanos
        }
    }

    #[derive(Debug)]
    struct SnapshotCandidate {
        order: u64,
        file: File,
    }

    fn version(file: &File) -> io::Result<LiveVersion> {
        let metadata = file.metadata()?;
        Ok(LiveVersion {
            device: metadata.dev(),
            inode: metadata.ino(),
            len: metadata.len(),
            modified_seconds: metadata.mtime(),
            modified_nanos: metadata.mtime_nsec(),
            changed_seconds: metadata.ctime(),
            changed_nanos: metadata.ctime_nsec(),
        })
    }

    fn current_live_version(resource: &ValidatedResource) -> io::Result<Option<LiveVersion>> {
        match resource.parent.open_regular(&resource.file_name) {
            Ok(file) => version(&file).map(Some),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn is_transaction_record(name: &OsStr) -> bool {
        name.to_str().is_some_and(|name| {
            name.starts_with(TRANSACTION_PREFIX) && name.ends_with(TRANSACTION_SUFFIX)
        })
    }

    fn transaction_records(directory: &SecureDir) -> io::Result<Vec<OsString>> {
        Ok(directory
            .entry_names()?
            .into_iter()
            .filter(|name| is_transaction_record(name))
            .collect())
    }

    fn ensure_no_recovery_required(directory: &SecureDir) -> io::Result<()> {
        if transaction_records(directory)?.is_empty() {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "an unresolved evolution transaction requires restore or manual recovery",
            ))
        }
    }

    fn encode_name(name: &OsStr) -> String {
        let mut encoded = String::with_capacity(name.as_bytes().len() * 2);
        for byte in name.as_bytes() {
            use std::fmt::Write as _;
            let _ = write!(encoded, "{byte:02x}");
        }
        encoded
    }

    fn version_record(version: Option<&LiveVersion>) -> String {
        version.map_or_else(
            || "none".to_string(),
            |version| {
                format!(
                    "{}:{}:{}:{}:{}:{}:{}",
                    version.device,
                    version.inode,
                    version.len,
                    version.modified_seconds,
                    version.modified_nanos,
                    version.changed_seconds,
                    version.changed_nanos
                )
            },
        )
    }

    fn create_transaction_record(
        resource: &ValidatedResource,
        claimed: &OsStr,
        promotion: &OsStr,
        expected: Option<&LiveVersion>,
        proposal: &LiveVersion,
    ) -> io::Result<OsString> {
        let content = format!(
            "version=1\nresource={}\nlive={}\nclaimed={}\npromotion={}\nexpected={}\nproposal={}\n",
            encoded_resource_components(&resource.identity).join(""),
            encode_name(&resource.file_name),
            encode_name(claimed),
            encode_name(promotion),
            version_record(expected),
            version_record(Some(proposal)),
        );
        loop {
            let name = temporary_name("transaction");
            match write_new(&resource.parent, &name, content.as_bytes()) {
                Ok(()) => {
                    if let Err(error) = resource.parent.sync() {
                        let _ = resource.parent.unlink(&name);
                        return Err(error);
                    }
                    return Ok(name);
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
    }

    fn read_transaction_record(directory: &SecureDir, transaction: &OsStr) -> io::Result<Vec<u8>> {
        const MAX_TRANSACTION_RECORD_BYTES: u64 = 8 * 1024;
        let file = directory.open_regular(transaction)?;
        let mut content = Vec::new();
        file.take(MAX_TRANSACTION_RECORD_BYTES + 1)
            .read_to_end(&mut content)?;
        if content.len() as u64 > MAX_TRANSACTION_RECORD_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "evolution transaction record exceeds size limit",
            ));
        }
        Ok(content)
    }

    fn transaction_matches_resource(
        resource: &ValidatedResource,
        transaction: &OsStr,
    ) -> io::Result<bool> {
        let content = read_transaction_record(&resource.parent, transaction)?;
        let content = std::str::from_utf8(&content).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "evolution transaction record is not UTF-8",
            )
        })?;
        let expected_resource = format!(
            "resource={}",
            encoded_resource_components(&resource.identity).join("")
        );
        let expected_live = format!("live={}", encode_name(&resource.file_name));
        Ok(content.lines().any(|line| line == expected_resource)
            && content.lines().any(|line| line == expected_live))
    }

    fn clear_transaction_record(directory: &SecureDir, transaction: &OsStr) -> io::Result<()> {
        let content = read_transaction_record(directory, transaction)?;
        directory.unlink(transaction)?;
        if let Err(sync_error) = directory.sync() {
            // Restore the visible marker when durable retirement cannot be
            // proved, so subsequent operations continue to fail closed.
            match write_new(directory, transaction, &content) {
                Ok(()) => {
                    let _ = directory.sync();
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => tracing::warn!(
                    kind = ?error.kind(),
                    "failed to recreate undurably cleared evolution transaction"
                ),
            }
            return Err(sync_error);
        }
        Ok(())
    }

    fn clear_recovery_records(resource: &ValidatedResource) -> io::Result<()> {
        let records = transaction_records(&resource.parent)?;
        for record in records {
            if transaction_matches_resource(resource, &record)? {
                clear_transaction_record(&resource.parent, &record)?;
            }
        }
        Ok(())
    }

    struct ClaimedCandidate<'a> {
        directory: &'a SecureDir,
        claimed: OsString,
        shadow: OsString,
        active: bool,
    }

    impl<'a> ClaimedCandidate<'a> {
        fn new(directory: &'a SecureDir, claimed: OsString, shadow: OsString) -> Self {
            Self {
                directory,
                claimed,
                shadow,
                active: true,
            }
        }

        fn name(&self) -> &OsStr {
            &self.claimed
        }

        fn retain_for_recovery(&mut self) {
            self.active = false;
        }

        fn requeue(&mut self) -> io::Result<()> {
            if !self.active {
                return Ok(());
            }
            self.directory
                .rename_noreplace(&self.claimed, &self.shadow)?;
            self.active = false;
            self.directory.sync()
        }

        fn discard(&mut self, context: &'static str) {
            self.active = false;
            if let Err(error) = self.directory.unlink(&self.claimed) {
                tracing::warn!(
                    candidate = %self.claimed.to_string_lossy(),
                    kind = ?error.kind(),
                    %context,
                    "evolution candidate cleanup failed"
                );
            }
        }
    }

    impl Drop for ClaimedCandidate<'_> {
        fn drop(&mut self) {
            if !self.active {
                return;
            }
            if let Err(error) = self.requeue() {
                match error.kind() {
                    io::ErrorKind::AlreadyExists => tracing::warn!(
                        candidate = %self.claimed.to_string_lossy(),
                        kind = ?error.kind(),
                        "newer shadow exists; failed candidate retained in quarantine"
                    ),
                    io::ErrorKind::NotFound => {}
                    _ => tracing::warn!(
                        candidate = %self.claimed.to_string_lossy(),
                        kind = ?error.kind(),
                        "failed candidate retained in quarantine"
                    ),
                }
            }
        }
    }

    fn recovery_live_state(
        resource: &ValidatedResource,
        proposal: &LiveVersion,
    ) -> RecoveryLiveState {
        match current_live_version(resource) {
            Ok(Some(current)) if current.same_file(proposal) => RecoveryLiveState::Proposal,
            Ok(Some(_)) => RecoveryLiveState::Other,
            Ok(None) => RecoveryLiveState::Missing,
            Err(_) => RecoveryLiveState::Unknown,
        }
    }

    fn validate_resource(project_nonoclaw: &Path, live: &Path) -> io::Result<ValidatedResource> {
        let relative = live
            .strip_prefix(project_nonoclaw)
            .map_err(|_| permission_denied("evolution resource is outside project .nonoclaw"))?;
        let mut components = Vec::new();
        for component in relative.components() {
            let Component::Normal(component) = component else {
                return Err(invalid_resource(
                    "evolution resource path is not normalized",
                ));
            };
            let component = component
                .to_str()
                .ok_or_else(|| invalid_resource("evolution resource path is not UTF-8"))?;
            components.push(component.to_string());
        }
        let allowed = components.as_slice() == ["APPEND_SYSTEM.md"]
            || (components.len() >= 2 && components.first().is_some_and(|part| part == "skills"));
        if !allowed {
            return Err(permission_denied("resource is not evolvable"));
        }

        let root = SecureDir::open_verified_ambient(project_nonoclaw)?;
        let mut parent = root.try_clone()?;
        for component in &components[..components.len() - 1] {
            parent = parent.open_dir(OsStr::new(component))?;
        }
        let file_name = OsString::from(
            components
                .last()
                .ok_or_else(|| invalid_resource("evolution resource has no filename"))?,
        );
        match parent.open_regular(&file_name) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        Ok(ValidatedResource {
            identity: components.join("/"),
            root,
            parent,
            file_name,
        })
    }

    fn write_new(directory: &SecureDir, name: &OsStr, content: &[u8]) -> io::Result<()> {
        let mut file = directory.create_new(name)?;
        let result = (|| {
            file.write_all(content)?;
            file.sync_all()
        })();
        if result.is_err() {
            drop(file);
            let _ = directory.unlink(name);
        }
        result
    }

    fn copy_to_new(
        source: &mut File,
        directory: &SecureDir,
        name: &OsStr,
        permissions: Permissions,
    ) -> io::Result<()> {
        let mut output = directory.create_new(name)?;
        let result = (|| {
            io::copy(source, &mut output)?;
            output.set_permissions(Permissions::from_mode(permissions.mode()))?;
            output.sync_all()
        })();
        if result.is_err() {
            drop(output);
            let _ = directory.unlink(name);
        }
        result
    }

    fn resource_history_directory(
        root: &SecureDir,
        resource: &str,
        create: bool,
    ) -> io::Result<Option<SecureDir>> {
        let mut directory = root.try_clone()?;
        let mut components = vec![
            OsString::from("evolution"),
            OsString::from("history"),
            OsString::from("v2"),
        ];
        components.extend(
            encoded_resource_components(resource)
                .into_iter()
                .map(OsString::from),
        );
        for component in components {
            directory = if create {
                directory.ensure_dir(&component)?
            } else {
                let Some(next) = directory.open_optional_dir(&component)? else {
                    return Ok(None);
                };
                next
            };
        }
        Ok(Some(directory))
    }

    fn local_legacy_directory(root: &SecureDir) -> io::Result<Option<SecureDir>> {
        let Some(evolution) = root.open_optional_dir(OsStr::new("evolution"))? else {
            return Ok(None);
        };
        evolution.open_optional_dir(OsStr::new("history"))
    }

    fn global_legacy_directory(project_nonoclaw: &Path) -> io::Result<Option<SecureDir>> {
        let Some(cwd) = project_nonoclaw.parent() else {
            return Ok(None);
        };
        let Some(project_dir) = nonoclaw_engine::session::project_dir(cwd) else {
            return Ok(None);
        };
        let project = match SecureDir::open_verified_ambient(&project_dir) {
            Ok(project) => project,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let Some(evolution) = project.open_optional_dir(OsStr::new("evolution"))? else {
            return Ok(None);
        };
        evolution.open_optional_dir(OsStr::new("history"))
    }

    fn newest_v2_in_directory(directory: &SecureDir) -> io::Result<Option<SnapshotCandidate>> {
        let mut best: Option<SnapshotCandidate> = None;
        for name in directory.entry_names()? {
            let Some(order) = name
                .to_str()
                .and_then(|name| name.strip_suffix(".bak"))
                .and_then(|order| order.parse().ok())
            else {
                continue;
            };
            let file = match directory.open_regular(&name) {
                Ok(file) => file,
                Err(_) => continue,
            };
            if best.as_ref().is_none_or(|current| order > current.order) {
                best = Some(SnapshotCandidate { order, file });
            }
        }
        Ok(best)
    }

    fn newest_v2(root: &SecureDir, resource: &str) -> io::Result<Option<SnapshotCandidate>> {
        let mut best: Option<SnapshotCandidate> = None;
        // The `.shadow` identity was briefly emitted by the old production
        // caller; dual-read it during migration, but only write the live ID.
        for identity in [resource.to_string(), format!("{resource}.shadow")] {
            let Some(directory) = resource_history_directory(root, &identity, false)? else {
                continue;
            };
            if let Some(candidate) = newest_v2_in_directory(&directory)? {
                if best
                    .as_ref()
                    .is_none_or(|current| candidate.order > current.order)
                {
                    best = Some(candidate);
                }
            }
        }
        Ok(best)
    }

    fn newest_legacy(
        directory: &SecureDir,
        resource: &str,
    ) -> io::Result<Option<SnapshotCandidate>> {
        let Some(expected) =
            legacy_resource_ids(resource).map(|ids| ids.map(|id| format!("{id}.bak")))
        else {
            return Ok(None);
        };
        let mut best: Option<SnapshotCandidate> = None;
        for name in directory.entry_names()? {
            let Some(name_text) = name.to_str() else {
                continue;
            };
            let Some((timestamp, suffix)) = name_text.split_once('-') else {
                continue;
            };
            if !expected.iter().any(|candidate| candidate == suffix) {
                continue;
            }
            let Ok(timestamp) = timestamp.parse::<u64>() else {
                continue;
            };
            let Some(order) = timestamp.checked_mul(1_000_000_000) else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "legacy evolution timestamp overflows snapshot order",
                ));
            };
            let file = match directory.open_regular(&name) {
                Ok(file) => file,
                Err(_) => continue,
            };
            if best.as_ref().is_none_or(|current| order > current.order) {
                best = Some(SnapshotCandidate { order, file });
            }
        }
        Ok(best)
    }

    fn newest_snapshot(
        project_nonoclaw: &Path,
        resource: &ValidatedResource,
    ) -> io::Result<Option<SnapshotCandidate>> {
        let mut candidates = Vec::new();
        if let Some(candidate) = newest_v2(&resource.root, &resource.identity)? {
            candidates.push(candidate);
        }
        if let Some(directory) = local_legacy_directory(&resource.root)? {
            if let Some(candidate) = newest_legacy(&directory, &resource.identity)? {
                candidates.push(candidate);
            }
        }
        if let Some(directory) = global_legacy_directory(project_nonoclaw)? {
            if let Some(candidate) = newest_legacy(&directory, &resource.identity)? {
                candidates.push(candidate);
            }
        }
        Ok(candidates
            .into_iter()
            .max_by_key(|candidate| candidate.order))
    }

    fn publish_snapshot(
        project_nonoclaw: &Path,
        resource: &ValidatedResource,
        source: &mut File,
        permissions: Permissions,
        expected: &LiveVersion,
    ) -> io::Result<()> {
        let history = resource_history_directory(&resource.root, &resource.identity, true)?
            .expect("create=true always returns a history directory");
        let floor = newest_snapshot(project_nonoclaw, resource)?
            .map(|candidate| candidate.order)
            .unwrap_or(0);
        let temporary = loop {
            let name = temporary_name("snapshot");
            match copy_to_new(source, &history, &name, permissions.clone()) {
                Ok(()) => break name,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        };

        let copied_version = version(source);
        if !copied_version
            .as_ref()
            .is_ok_and(|actual| actual == expected)
        {
            let _ = history.unlink(&temporary);
            let _ = history.sync();
            return match copied_version {
                Err(error) => Err(error),
                Ok(_) => Err(live_changed_error("while snapshotting")),
            };
        }

        let mut order = match next_snapshot_order(floor) {
            Ok(order) => order,
            Err(error) => {
                let _ = history.unlink(&temporary);
                let _ = history.sync();
                return Err(error);
            }
        };
        loop {
            let destination = OsString::from(format!("{order}.bak"));
            match history.hard_link(&temporary, &destination) {
                Ok(()) => {
                    if let Err(error) = history.sync() {
                        let _ = history.unlink(&destination);
                        let _ = history.unlink(&temporary);
                        let _ = history.sync();
                        return Err(error);
                    }
                    if let Err(error) = history.unlink(&temporary) {
                        tracing::warn!(kind = ?error.kind(), "complete snapshot temp cleanup failed");
                    } else if let Err(error) = history.sync() {
                        tracing::warn!(kind = ?error.kind(), "snapshot temp cleanup sync failed");
                    }
                    return Ok(());
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    order = match next_snapshot_order(order) {
                        Ok(order) => order,
                        Err(error) => {
                            let _ = history.unlink(&temporary);
                            let _ = history.sync();
                            return Err(error);
                        }
                    };
                }
                Err(error) => {
                    let _ = history.unlink(&temporary);
                    let _ = history.sync();
                    return Err(error);
                }
            }
        }
    }

    /// Snapshot the exact opened live object and return its stable version.
    fn snapshot(
        project_nonoclaw: &Path,
        resource: &ValidatedResource,
    ) -> io::Result<Option<LiveVersion>> {
        let mut live = match resource.parent.open_regular(&resource.file_name) {
            Ok(live) => live,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let before = version(&live)?;
        let permissions = live.metadata()?.permissions();
        publish_snapshot(project_nonoclaw, resource, &mut live, permissions, &before)?;
        Ok(Some(before))
    }

    pub(super) fn resource_identity(project_nonoclaw: &Path, live: &Path) -> io::Result<String> {
        validate_resource(project_nonoclaw, live).map(|resource| resource.identity)
    }

    pub(super) fn stage_shadow(
        project_nonoclaw: &Path,
        live: &Path,
        content: &str,
    ) -> io::Result<PathBuf> {
        let _transaction = EVOLUTION_TRANSACTION
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let resource = validate_resource(project_nonoclaw, live)?;
        ensure_no_recovery_required(&resource.parent)?;
        let shadow = shadow_name(&resource.file_name);
        let temporary = loop {
            let name = temporary_name("stage");
            match write_new(&resource.parent, &name, content.as_bytes()) {
                Ok(()) => break name,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        };
        if let Err(error) = resource.parent.replace(&temporary, &shadow) {
            let _ = resource.parent.unlink(&temporary);
            return Err(error);
        }
        Ok(live.with_file_name(shadow))
    }

    pub(super) fn commit_gated(
        live: &Path,
        project_nonoclaw: &Path,
        gate: impl FnOnce() -> Result<(), String>,
    ) -> io::Result<CommitOutcome> {
        let _transaction = EVOLUTION_TRANSACTION
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let resource = validate_resource(project_nonoclaw, live)?;
        ensure_no_recovery_required(&resource.parent)?;
        let shadow = shadow_name(&resource.file_name);
        let claimed_name = loop {
            let candidate = temporary_name("candidate");
            match resource.parent.rename_noreplace(&shadow, &candidate) {
                Ok(()) => break candidate,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    return Ok(CommitOutcome::NoShadow)
                }
                Err(error) => return Err(error),
            }
        };
        let mut claim = ClaimedCandidate::new(&resource.parent, claimed_name, shadow);
        let mut candidate = resource.parent.open_regular(claim.name())?;
        let candidate_permissions = candidate.metadata()?.permissions();

        if let Err(reason) = gate() {
            claim.discard("rejected candidate");
            tracing::info!(resource = %resource.identity, %reason, "evolution: gate rejected candidate");
            return Ok(CommitOutcome::Rejected(reason));
        }

        let candidate_before = version(&candidate)?;
        let live_before = snapshot(project_nonoclaw, &resource)?;
        if current_live_version(&resource)? != live_before {
            return Err(live_changed_error("after snapshot"));
        }
        let promotion = loop {
            let name = temporary_name("promotion");
            match copy_to_new(
                &mut candidate,
                &resource.parent,
                &name,
                candidate_permissions.clone(),
            ) {
                Ok(()) => break name,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        };

        let candidate_after = version(&candidate);
        if !candidate_after
            .as_ref()
            .is_ok_and(|actual| actual == &candidate_before)
        {
            let _ = resource.parent.unlink(&promotion);
            return match candidate_after {
                Err(error) => Err(error),
                Ok(_) => Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "claimed evolution candidate changed while preparing promotion",
                )),
            };
        }
        let promotion_version = match resource
            .parent
            .open_regular(&promotion)
            .and_then(|file| version(&file))
        {
            Ok(version) => version,
            Err(error) => {
                let _ = resource.parent.unlink(&promotion);
                return Err(error);
            }
        };
        match current_live_version(&resource) {
            Ok(current) if current == live_before => {}
            Ok(_) => {
                let _ = resource.parent.unlink(&promotion);
                return Err(live_changed_error("before promotion"));
            }
            Err(error) => {
                let _ = resource.parent.unlink(&promotion);
                return Err(error);
            }
        }

        let transaction = match create_transaction_record(
            &resource,
            claim.name(),
            &promotion,
            live_before.as_ref(),
            &promotion_version,
        ) {
            Ok(transaction) => transaction,
            Err(error) => {
                let _ = resource.parent.unlink(&promotion);
                return Err(error);
            }
        };
        let cutover = match live_before.as_ref() {
            None => resource
                .parent
                .rename_noreplace(&promotion, &resource.file_name),
            Some(_) => resource.parent.exchange(&promotion, &resource.file_name),
        };

        if let Err(cutover_error) = cutover {
            let promotion_after = resource
                .parent
                .open_regular(&promotion)
                .and_then(|file| version(&file));
            let live_after = current_live_version(&resource);
            let namespace_unchanged = promotion_after
                .as_ref()
                .is_ok_and(|actual| actual == &promotion_version)
                && live_after
                    .as_ref()
                    .is_ok_and(|actual| actual == &live_before);
            if namespace_unchanged {
                if let Err(error) = claim.requeue() {
                    tracing::warn!(kind = ?error.kind(), "failed to durably requeue uncommitted evolution candidate");
                    let live_state = recovery_live_state(&resource, &promotion_version);
                    claim.retain_for_recovery();
                    return Ok(CommitOutcome::RecoveryRequired {
                        transaction: transaction.to_string_lossy().into_owned(),
                        live_state,
                    });
                }
                if let Err(error) = clear_transaction_record(&resource.parent, &transaction) {
                    tracing::warn!(kind = ?error.kind(), "failed to clear unissued evolution transaction");
                    let live_state = recovery_live_state(&resource, &promotion_version);
                    return Ok(CommitOutcome::RecoveryRequired {
                        transaction: transaction.to_string_lossy().into_owned(),
                        live_state,
                    });
                }
                if let Err(error) = resource.parent.unlink(&promotion) {
                    tracing::warn!(kind = ?error.kind(), "unused evolution promotion cleanup failed");
                }
                return Err(cutover_error);
            }

            if let Err(error) = resource.parent.sync() {
                tracing::warn!(kind = ?error.kind(), "ambiguous evolution cutover sync failed");
            }
            let live_state = recovery_live_state(&resource, &promotion_version);
            claim.retain_for_recovery();
            return Ok(CommitOutcome::RecoveryRequired {
                transaction: transaction.to_string_lossy().into_owned(),
                live_state,
            });
        }

        let proposal_is_live = current_live_version(&resource)
            .as_ref()
            .is_ok_and(|current| {
                current
                    .as_ref()
                    .is_some_and(|actual| actual.same_file_content(&promotion_version))
            });
        let displaced_is_expected = match live_before.as_ref() {
            None => true,
            Some(expected) => resource
                .parent
                .open_regular(&promotion)
                .and_then(|file| version(&file))
                .as_ref()
                .is_ok_and(|actual| actual.same_file_content(expected)),
        };
        if !proposal_is_live || !displaced_is_expected {
            if let Err(error) = resource.parent.sync() {
                tracing::warn!(kind = ?error.kind(), "conflicted evolution cutover sync failed");
            }
            let live_state = recovery_live_state(&resource, &promotion_version);
            claim.retain_for_recovery();
            return Ok(CommitOutcome::RecoveryRequired {
                transaction: transaction.to_string_lossy().into_owned(),
                live_state,
            });
        }
        if let Err(error) = resource.parent.sync() {
            tracing::warn!(kind = ?error.kind(), "committed evolution cutover sync failed");
            let live_state = recovery_live_state(&resource, &promotion_version);
            claim.retain_for_recovery();
            return Ok(CommitOutcome::RecoveryRequired {
                transaction: transaction.to_string_lossy().into_owned(),
                live_state,
            });
        }
        if let Err(error) = clear_transaction_record(&resource.parent, &transaction) {
            tracing::warn!(kind = ?error.kind(), "committed evolution transaction cleanup failed");
            let live_state = recovery_live_state(&resource, &promotion_version);
            claim.retain_for_recovery();
            return Ok(CommitOutcome::RecoveryRequired {
                transaction: transaction.to_string_lossy().into_owned(),
                live_state,
            });
        }

        if live_before.is_some() {
            if let Err(error) = resource.parent.unlink(&promotion) {
                tracing::warn!(kind = ?error.kind(), "old live cleanup after commit failed");
            }
        }
        claim.discard("committed candidate");
        if let Err(error) = resource.parent.sync() {
            tracing::warn!(kind = ?error.kind(), "committed evolution cleanup sync failed");
        }
        tracing::info!(resource = %resource.identity, "evolution: committed candidate -> live");
        Ok(CommitOutcome::Committed)
    }

    pub(super) fn restore(project_nonoclaw: &Path, live: &Path) -> io::Result<bool> {
        let _transaction = EVOLUTION_TRANSACTION
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let resource = validate_resource(project_nonoclaw, live)?;
        let Some(mut snapshot) = newest_snapshot(project_nonoclaw, &resource)? else {
            return Ok(false);
        };
        let snapshot_before = version(&snapshot.file)?;
        let permissions = snapshot.file.metadata()?.permissions();
        let promotion = loop {
            let name = temporary_name("restore");
            match copy_to_new(
                &mut snapshot.file,
                &resource.parent,
                &name,
                permissions.clone(),
            ) {
                Ok(()) => break name,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        };
        let snapshot_after = version(&snapshot.file);
        if !snapshot_after
            .as_ref()
            .is_ok_and(|actual| actual == &snapshot_before)
        {
            let _ = resource.parent.unlink(&promotion);
            return match snapshot_after {
                Err(error) => Err(error),
                Ok(_) => Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "evolution snapshot changed while preparing restore",
                )),
            };
        }
        if let Err(error) = resource.parent.replace(&promotion, &resource.file_name) {
            let _ = resource.parent.unlink(&promotion);
            return Err(error);
        }
        resource.parent.sync()?;
        clear_recovery_records(&resource)?;
        Ok(true)
    }
}

#[cfg(target_os = "linux")]
pub(super) fn resource_identity(project_nonoclaw: &Path, live: &Path) -> io::Result<String> {
    secure::resource_identity(project_nonoclaw, live)
}

#[cfg(not(target_os = "linux"))]
pub(super) fn resource_identity(_project_nonoclaw: &Path, _live: &Path) -> io::Result<String> {
    Err(unsupported_backend())
}

#[cfg(target_os = "linux")]
pub(super) fn stage_shadow(
    project_nonoclaw: &Path,
    live: &Path,
    content: &str,
) -> io::Result<PathBuf> {
    secure::stage_shadow(project_nonoclaw, live, content)
}

#[cfg(not(target_os = "linux"))]
pub fn stage_shadow(_project_nonoclaw: &Path, _live: &Path, _content: &str) -> io::Result<PathBuf> {
    Err(unsupported_backend())
}

#[cfg(target_os = "linux")]
pub(super) fn commit_gated(
    live: &Path,
    project_nonoclaw: &Path,
    gate: impl FnOnce() -> Result<(), String>,
) -> io::Result<CommitOutcome> {
    secure::commit_gated(live, project_nonoclaw, gate)
}

#[cfg(not(target_os = "linux"))]
pub fn commit_gated(
    _live: &Path,
    _project_nonoclaw: &Path,
    _gate: impl FnOnce() -> Result<(), String>,
) -> io::Result<CommitOutcome> {
    Err(unsupported_backend())
}

#[cfg(target_os = "linux")]
pub(super) fn restore(project_nonoclaw: &Path, live: &Path) -> io::Result<bool> {
    secure::restore(project_nonoclaw, live)
}

#[cfg(not(target_os = "linux"))]
pub fn restore(_project_nonoclaw: &Path, _live: &Path) -> io::Result<bool> {
    Err(unsupported_backend())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!("nc-evo-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(directory.join(".nonoclaw/evolution/history")).unwrap();
        directory
    }

    #[test]
    fn commit_and_rollback_roundtrip() {
        let root = tmp("round");
        let nonoclaw = root.join(".nonoclaw");
        let live = nonoclaw.join("skills/test.md");
        std::fs::create_dir_all(live.parent().unwrap()).unwrap();
        std::fs::write(&live, "v1").unwrap();
        stage_shadow(&nonoclaw, &live, "v2 proposal").unwrap();

        assert_eq!(
            commit_gated(&live, &nonoclaw, || Ok(())).unwrap(),
            CommitOutcome::Committed
        );
        assert_eq!(std::fs::read_to_string(&live).unwrap(), "v2 proposal");
        assert!(!live.with_file_name("test.md.shadow").exists());

        assert!(restore(&nonoclaw, &live).unwrap());
        assert_eq!(std::fs::read_to_string(&live).unwrap(), "v1");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rejected_gate_discards_shadow_keeps_live() {
        let root = tmp("reject");
        let nonoclaw = root.join(".nonoclaw");
        let live = nonoclaw.join("APPEND_SYSTEM.md");
        std::fs::write(&live, "live").unwrap();
        stage_shadow(&nonoclaw, &live, "bad proposal").unwrap();
        assert_eq!(
            commit_gated(&live, &nonoclaw, || Err("bench regressed".into())).unwrap(),
            CommitOutcome::Rejected("bench regressed".into())
        );
        assert_eq!(std::fs::read_to_string(&live).unwrap(), "live");
        assert!(!live.with_file_name("APPEND_SYSTEM.md.shadow").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn no_shadow_is_noop() {
        let root = tmp("noop");
        let nonoclaw = root.join(".nonoclaw");
        let live = nonoclaw.join("APPEND_SYSTEM.md");
        std::fs::write(&live, "live").unwrap();
        assert_eq!(
            commit_gated(&live, &nonoclaw, || Ok(())).unwrap(),
            CommitOutcome::NoShadow
        );
        assert_eq!(std::fs::read_to_string(&live).unwrap(), "live");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn default_gate_thresholds() {
        assert!(default_gate("done", 1.0).is_ok());
        assert!(default_gate("done", 0.5).is_ok());
        assert!(default_gate("done", 0.2).is_err());
        assert!(default_gate("error", 1.0).is_err());
    }
}
