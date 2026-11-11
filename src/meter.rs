//! The read meter (specs/auto-interval.md N2): counts what the PRODUCTION read
//! paths really consume, so a test can assert "at most 512 KiB of a 5 MiB
//! transcript" about the code that ships, not about a copy of it.
//!
//! Why a registry and not a parameter: the three paths (the session transcript
//! tail, the fallback scan of recent transcripts, the output of `uw`) sit
//! several calls below `Vigil`, and their public signatures are the contract
//! of other tests. A test registers a [`ReadTap`] on a directory (every file
//! below it is metered) and the production code reports each read it makes
//! through [`note_open`] and [`note_read`]. With no tap registered those calls
//! cost one atomic load.
//!
//! Invariants: the meter only observes; it never changes what is read. Taps
//! are scoped by path prefix, and each test uses its own tempdir, so tests
//! running in parallel do not see each other's reads. Mating parts: the
//! callers in `cache.rs` (`read_tail`) and `credit.rs` (`uw` output).

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};

/// What the production code did to one file (or to one command's output).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FileTally {
    /// Times it was opened.
    pub opens: u64,
    /// Bytes handed out by reads.
    pub bytes_read: u64,
    /// The lowest offset a read began at (`None` for a pipe or no read).
    pub lowest_offset: Option<u64>,
}

type Tallies = BTreeMap<PathBuf, FileTally>;
type Shared = Arc<Mutex<Tallies>>;

static ACTIVE_TAPS: AtomicUsize = AtomicUsize::new(0);
static TAPS: Mutex<Vec<(PathBuf, Weak<Mutex<Tallies>>)>> = Mutex::new(Vec::new());

/// A registration: everything read below `prefix` while this is alive is
/// counted here. Dropping it unregisters.
pub struct ReadTap {
    shared: Shared,
}

impl ReadTap {
    pub fn on(prefix: &Path) -> ReadTap {
        let shared: Shared = Arc::default();
        TAPS.lock().expect("tap registry poisoned").push((prefix.to_path_buf(), Arc::downgrade(&shared)));
        ACTIVE_TAPS.fetch_add(1, Ordering::SeqCst);
        ReadTap { shared }
    }

    /// Every path that was opened or read, with its tally.
    pub fn files(&self) -> BTreeMap<PathBuf, FileTally> {
        self.shared.lock().expect("tally poisoned").clone()
    }

    /// The tally of one path (zero when it was never touched).
    pub fn of(&self, path: &Path) -> FileTally {
        self.files().get(path).copied().unwrap_or_default()
    }

    /// Bytes read across every path below the prefix.
    pub fn total_bytes(&self) -> u64 {
        self.files().values().map(|tally| tally.bytes_read).sum()
    }

    /// How many distinct paths were opened.
    pub fn files_opened(&self) -> usize {
        self.files().values().filter(|tally| tally.opens > 0).count()
    }
}

impl Drop for ReadTap {
    fn drop(&mut self) {
        let mut taps = TAPS.lock().expect("tap registry poisoned");
        let mine = Arc::downgrade(&self.shared);
        taps.retain(|(_, weak)| !Weak::ptr_eq(weak, &mine));
        ACTIVE_TAPS.fetch_sub(1, Ordering::SeqCst);
    }
}

fn each_tap(path: &Path, mut record: impl FnMut(&mut FileTally)) {
    if ACTIVE_TAPS.load(Ordering::SeqCst) == 0 {
        return;
    }
    let taps = TAPS.lock().expect("tap registry poisoned");
    for (prefix, weak) in taps.iter() {
        if !path.starts_with(prefix) {
            continue;
        }
        if let Some(shared) = weak.upgrade() {
            record(shared.lock().expect("tally poisoned").entry(path.to_path_buf()).or_default());
        }
    }
}

/// Production code opened `path`.
pub(crate) fn note_open(path: &Path) {
    each_tap(path, |tally| tally.opens += 1);
}

/// Production code read `bytes` from `path`, starting at `offset` (`None`
/// for a pipe, which has no offsets).
pub(crate) fn note_read(path: &Path, offset: Option<u64>, bytes: u64) {
    if bytes == 0 {
        return;
    }
    each_tap(path, |tally| {
        tally.bytes_read += bytes;
        if let Some(offset) = offset {
            tally.lowest_offset = Some(tally.lowest_offset.map_or(offset, |lowest| lowest.min(offset)));
        }
    });
}

/// A regular file opened for reading whose every read is reported to the
/// meter. Production reads transcripts only through this.
pub(crate) struct MeteredFile {
    file: File,
    path: PathBuf,
    position: u64,
}

impl MeteredFile {
    pub(crate) fn open(path: &Path) -> std::io::Result<MeteredFile> {
        let file = File::open(path)?;
        // A device or a pipe that raced into the path is not a transcript.
        if !file.metadata()?.is_file() {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "not a regular file"));
        }
        note_open(path);
        Ok(MeteredFile { file, path: path.to_path_buf(), position: 0 })
    }
}

impl Read for MeteredFile {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let count = self.file.read(buf)?;
        note_read(&self.path, Some(self.position), count as u64);
        self.position += count as u64;
        Ok(count)
    }
}

impl Seek for MeteredFile {
    fn seek(&mut self, to: SeekFrom) -> std::io::Result<u64> {
        self.position = self.file.seek(to)?;
        Ok(self.position)
    }
}
