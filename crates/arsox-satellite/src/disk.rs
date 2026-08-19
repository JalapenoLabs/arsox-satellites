// Copyright © 2026 Jalapeno Labs

//! What the satellite is holding on disk, measured rather than guessed.
//!
//! Per-thread quotas stop one runaway workspace. They do not stop forty
//! well-behaved ones from filling a volume between them, so an orchestrator
//! deciding whether it needs another satellite needs the satellite-wide numbers:
//! how much the workspace root holds, how much the volume has left, and how much
//! the embedded database has grown to.
//!
//! # Measuring is expensive, so it is cached
//!
//! There is no cheap way to size a directory tree. A workspace holding a few
//! repo checkouts is tens of thousands of `stat` calls, and `/v1/status` is an
//! endpoint operators poll. [`Meter`] therefore walks the tree at most once per
//! [`DEFAULT_REFRESH_INTERVAL`] and serves the recorded measurement in between.
//!
//! **The tradeoff is staleness, and it is deliberate.** A status response can
//! report disk figures up to one refresh interval old. Nothing in the satellite
//! decides anything from these numbers: per-thread quotas are enforced against
//! their own thread's subtree elsewhere, so the cost of a stale reading is an
//! operator seeing a slightly old figure, never an enforcement gate opening or
//! closing on one. The alternative, walking the tree per request, makes a poll
//! against a busy satellite arbitrarily slow.
//!
//! The walk itself runs on the blocking pool. It is filesystem I/O measured in
//! seconds on a large workspace, and running it on an async worker would stall
//! every other request the satellite is serving.
//!
//! # Sizes are apparent, not allocated
//!
//! A file's size is what its metadata reports, so sparse files count for more
//! than they occupy and block rounding is not added. This is the number a `du
//! --apparent-size` gives, it is what a quota is written against, and it is the
//! only one obtainable without per-platform block accounting.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How long a measurement is served before the tree is walked again.
///
/// Ten seconds against a poll interval measured in seconds means one walk per
/// handful of requests rather than one per request, while keeping the figure
/// recent enough that an operator watching a volume fill sees it filling. See
/// the module docs for what staleness does and does not affect.
pub const DEFAULT_REFRESH_INTERVAL: Duration = Duration::from_secs(10);

/// Disk the satellite is holding, as one measurement.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Usage {
    /// Everything under the workspace root, including whatever is not a thread.
    pub workspace_bytes: u64,

    /// The embedded database and its write-ahead sidecars.
    pub database_bytes: u64,

    /// What the volume has left.
    ///
    /// Absent when the platform could not answer, which is a different fact from
    /// a full volume. Reporting zero would claim the satellite has no room left
    /// and is about to fail every write, so an unanswerable question stays
    /// unanswered.
    pub available_bytes: Option<u64>,

    /// Bytes held by each subtree directly under the workspace root, keyed by
    /// directory name.
    ///
    /// Thread ids are those directory names, which is what lets a thread summary
    /// report its own size out of a single walk rather than one walk per thread.
    pub by_thread: BTreeMap<String, u64>,
}

impl Usage {
    /// What one thread's workspace subtree holds.
    ///
    /// Zero for a thread that has no directory yet. A thread that declares no
    /// repos never provisions one, so this is an ordinary case rather than a
    /// missing measurement.
    #[must_use]
    pub fn thread_bytes(&self, thread_id: &str) -> u64 {
        self.by_thread.get(thread_id).copied().unwrap_or_default()
    }
}

/// A measurement, and when it was taken.
#[derive(Debug)]
struct Snapshot {
    measured_at: Instant,
    usage: Usage,
}

/// Measures the satellite's disk, no more often than it has to.
///
/// Cheap to call from a request handler: the walk happens on the blocking pool,
/// at most once per refresh interval, and concurrent callers share one walk
/// rather than starting several.
#[derive(Debug)]
pub struct Meter {
    workspace_root: PathBuf,
    database_path: PathBuf,
    refresh_interval: Duration,

    /// The last measurement. Held under a lock that is taken across the refresh,
    /// so ten pollers arriving on a cold cache queue behind one walk instead of
    /// starting ten of them.
    cached: tokio::sync::Mutex<Option<Snapshot>>,
}

impl Meter {
    /// A meter over the workspace root and the database file.
    #[must_use]
    pub fn new(workspace_root: PathBuf, database_path: PathBuf) -> Self {
        Self::with_refresh_interval(workspace_root, database_path, DEFAULT_REFRESH_INTERVAL)
    }

    /// A meter that refreshes on a caller-chosen interval.
    ///
    /// Exists for tests, which need a measurement that either never goes stale
    /// or is stale immediately, and cannot wait ten seconds to find out which.
    #[must_use]
    pub fn with_refresh_interval(
        workspace_root: PathBuf,
        database_path: PathBuf,
        refresh_interval: Duration,
    ) -> Self {
        Self {
            workspace_root,
            database_path,
            refresh_interval,
            cached: tokio::sync::Mutex::new(None),
        }
    }

    /// The current measurement, walking the tree only if the last one is stale.
    ///
    /// Never fails. A workspace root that cannot be read measures zero bytes with
    /// no free space reported, which is what an absent volume honestly looks
    /// like, and is a state readiness reports on rather than one status should
    /// refuse over.
    pub async fn usage(&self) -> Usage {
        let mut cached = self.cached.lock().await;

        if let Some(snapshot) = cached.as_ref()
            && snapshot.measured_at.elapsed() < self.refresh_interval
        {
            return snapshot.usage.clone();
        }

        let workspace_root = self.workspace_root.clone();
        let database_path = self.database_path.clone();

        let Ok(usage) =
            tokio::task::spawn_blocking(move || measure(&workspace_root, &database_path)).await
        else {
            // The walk panicked or the runtime is shutting down. Neither is worth
            // recording as a measurement, so the next caller tries again rather
            // than being served an empty one for the rest of the interval.
            return Usage::default();
        };

        *cached = Some(Snapshot {
            measured_at: Instant::now(),
            usage: usage.clone(),
        });

        usage
    }
}

/// Walks the workspace root and sizes the database. Blocking.
fn measure(workspace_root: &Path, database_path: &Path) -> Usage {
    let mut usage = Usage {
        available_bytes: available_bytes(workspace_root),
        database_bytes: database_bytes(database_path),
        ..Usage::default()
    };

    let Ok(entries) = std::fs::read_dir(workspace_root) else {
        // The root does not exist yet, or is not readable. Both are honestly
        // "nothing measured here" rather than a failure to report.
        return usage;
    };

    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };

        if file_type.is_dir() {
            let bytes = directory_bytes(&entry.path());
            usage.workspace_bytes = usage.workspace_bytes.saturating_add(bytes);
            usage
                .by_thread
                .insert(entry.file_name().to_string_lossy().into_owned(), bytes);
        } else {
            // Counted toward the total but attributed to no thread. Anything the
            // satellite drops beside the thread directories still occupies the
            // volume, and a total that omitted it would be wrong.
            usage.workspace_bytes = usage
                .workspace_bytes
                .saturating_add(entry.metadata().map(|data| data.len()).unwrap_or_default());
        }
    }

    usage
}

/// Sums every file beneath `root`, following no symlinks. Blocking.
///
/// Iterative rather than recursive: a workspace holds repo checkouts, checkouts
/// hold `node_modules`, and a deep tree must not be able to overflow the stack of
/// the thread measuring it.
///
/// Entries that cannot be read are skipped rather than aborting the walk. A
/// thread being collected while this runs makes its directory disappear
/// mid-walk, which is normal and not worth losing the whole measurement over.
///
/// Symlinks are counted as the links they are rather than followed, so a link
/// into another thread's workspace cannot be counted twice and a cycle cannot
/// run forever.
fn directory_bytes(root: &Path) -> u64 {
    let mut total: u64 = 0;
    let mut pending = vec![root.to_path_buf()];

    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };

        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };

            if file_type.is_dir() {
                pending.push(entry.path());
            } else {
                total = total
                    .saturating_add(entry.metadata().map(|data| data.len()).unwrap_or_default());
            }
        }
    }

    total
}

/// Sizes the database and the sidecars WAL mode keeps beside it. Blocking.
///
/// The write-ahead log is not a rounding error: it holds every committed page
/// until a checkpoint folds it back, and on a satellite appending events
/// continuously it can be the larger of the two files. Reporting the database
/// file alone would under-report exactly when the number matters.
fn database_bytes(database_path: &Path) -> u64 {
    // The names SQLite derives from the database file in WAL mode.
    const SIDECARS: [&str; 3] = ["", "-wal", "-shm"];

    SIDECARS.iter().fold(0_u64, |total, suffix| {
        let mut name = database_path.as_os_str().to_owned();
        name.push(suffix);

        let bytes = std::fs::metadata(PathBuf::from(name))
            .map(|data| data.len())
            .unwrap_or_default();

        total.saturating_add(bytes)
    })
}

/// Widens a platform integer whose width is not the same on every target.
///
/// `statvfs` reports `c_ulong` fields, 64 bits on the targets the satellite ships
/// on and 32 on others. An `as` cast is a lint on the first and a silent
/// truncation risk in general, so the conversion is written once and goes through
/// `TryInto`, which cannot fail for any unsigned type this is called with.
#[cfg(unix)]
fn widen(value: impl TryInto<u64>) -> u64 {
    value.try_into().unwrap_or_default()
}

/// Free space on the volume holding `path`, as the filesystem reports it.
///
/// `std` has no free-space API, so this is a direct call to the platform. `libc`
/// is used rather than a hand-written `extern` block because the `statvfs`
/// layout differs by architecture, and getting that wrong is undefined behavior
/// rather than a wrong number.
///
/// `f_bavail` rather than `f_bfree`: the difference is the reserve only root may
/// write into, and the satellite's agents do not run as root.
#[cfg(unix)]
fn available_bytes(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt as _;

    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();

    // SAFETY: `path` is a live NUL-terminated C string for the duration of the
    // call, and `stats` is a live, correctly aligned allocation of exactly the
    // type the call writes. A non-zero return means it wrote nothing, which is
    // the branch below.
    if unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return None;
    }

    // SAFETY: `statvfs` returned zero, so it initialized the whole struct.
    let stats = unsafe { stats.assume_init() };

    Some(widen(stats.f_frsize).saturating_mul(widen(stats.f_bavail)))
}

/// Free space on the volume holding `path`, as the filesystem reports it.
///
/// The value is what is available to the calling process rather than to the
/// volume as a whole, which is the honest number under a disk quota.
#[cfg(windows)]
fn available_bytes(path: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt as _;

    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);

    let mut available: u64 = 0;

    // SAFETY: `wide` is a NUL-terminated UTF-16 path that outlives the call, and
    // `available` is a live `u64` the call writes at most once. The two totals
    // this satellite does not report are passed as null, which the API documents
    // as permitted.
    let measured = unsafe {
        windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &raw mut available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };

    (measured != 0).then_some(available)
}

/// Free space is not measurable on this target.
///
/// A satellite ships as a Linux container and is developed on Windows, so this
/// exists to keep the crate compiling anywhere rather than to be reached. Absent
/// is the honest answer: see [`Usage::available_bytes`].
#[cfg(not(any(unix, windows)))]
fn available_bytes(_path: &Path) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory of this test's own.
    ///
    /// Named with a `UUIDv7` rather than a timestamp because these tests run in
    /// parallel and a Windows clock ticks slowly enough for two of them to pick
    /// the same name and measure each other's files.
    fn scratch(label: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("arsox-disk-{label}-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&directory).expect("should create a scratch directory");
        directory
    }

    fn write(path: &Path, bytes: usize) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("should create the parent");
        }
        std::fs::write(path, vec![b'x'; bytes]).expect("should write");
    }

    #[test]
    fn a_walk_totals_the_tree_and_attributes_it_per_thread() {
        let root = scratch("walk");
        write(&root.join("thread-a/AGENTS.md"), 100);
        write(&root.join("thread-a/repos/api/src/main.rs"), 400);
        write(&root.join("thread-b/AGENTS.md"), 50);

        let usage = measure(&root, &root.join("arsox.db"));

        assert_eq!(usage.workspace_bytes, 550);
        // Nested files count toward the thread that owns the subtree, which is
        // what makes one walk enough to report every thread's size.
        assert_eq!(usage.thread_bytes("thread-a"), 500);
        assert_eq!(usage.thread_bytes("thread-b"), 50);
        // A thread with no directory yet holds nothing, rather than being a hole
        // in the map a caller has to handle.
        assert_eq!(usage.thread_bytes("thread-c"), 0);

        drop(std::fs::remove_dir_all(&root));
    }

    #[test]
    fn a_stray_file_counts_toward_the_total_but_belongs_to_no_thread() {
        let root = scratch("stray");
        write(&root.join("thread-a/AGENTS.md"), 10);
        write(&root.join("not-a-thread.log"), 90);

        let usage = measure(&root, &root.join("arsox.db"));

        assert_eq!(usage.workspace_bytes, 100, "it still occupies the volume");
        assert_eq!(usage.by_thread.len(), 1);

        drop(std::fs::remove_dir_all(&root));
    }

    #[test]
    fn the_database_is_measured_with_the_sidecars_wal_mode_keeps_beside_it() {
        let root = scratch("database");
        let database = root.join("arsox.db");
        write(&database, 30);
        write(&root.join("arsox.db-wal"), 60);
        write(&root.join("arsox.db-shm"), 10);

        let usage = measure(&root, &database);

        // The log holds every committed page until a checkpoint folds it back, so
        // omitting it would under-report precisely when the number matters.
        assert_eq!(usage.database_bytes, 100);

        drop(std::fs::remove_dir_all(&root));
    }

    #[test]
    fn an_absent_workspace_root_measures_nothing_rather_than_failing() {
        let root = std::env::temp_dir().join(format!("arsox-disk-absent-{}", uuid::Uuid::now_v7()));

        let usage = measure(&root, &root.join("arsox.db"));

        assert_eq!(usage.workspace_bytes, 0);
        assert!(usage.by_thread.is_empty());
    }

    #[test]
    fn free_space_is_a_real_number_on_a_real_volume() {
        let root = scratch("free");

        let available = available_bytes(&root).expect("a mounted volume reports its free space");
        assert!(available > 0, "a writable scratch volume has room left");

        drop(std::fs::remove_dir_all(&root));
    }

    #[tokio::test]
    async fn a_measurement_is_reused_until_it_goes_stale() {
        let root = scratch("cache");
        write(&root.join("thread-a/AGENTS.md"), 100);

        let meter = Meter::with_refresh_interval(
            root.clone(),
            root.join("arsox.db"),
            Duration::from_hours(1),
        );
        assert_eq!(meter.usage().await.workspace_bytes, 100);

        write(&root.join("thread-a/large.bin"), 900);

        // The point of the cache: a poll costs nothing until the interval
        // elapses, at the price of a figure that can lag behind the disk.
        assert_eq!(meter.usage().await.workspace_bytes, 100);

        let live =
            Meter::with_refresh_interval(root.clone(), root.join("arsox.db"), Duration::ZERO);
        assert_eq!(live.usage().await.workspace_bytes, 1000);

        drop(std::fs::remove_dir_all(&root));
    }
}
