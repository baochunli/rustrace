//! Bounded, reproducible fuzzing support shared by the T8.3 parser targets.
//!
//! Every target runs in a re-executed child process so filesystem writes can be
//! confined, and every parser call is wrapped so a panic, an abort, or a peak
//! allocation above the target's bound fails the test.
//!
//! # What an allocation bound here is, and is not
//!
//! Peak allocation is a composite of everything live at once: input buffers,
//! decoded structures, re-encodings performed for canonical-form checks, owned
//! name copies, and spool state. Five review rounds each found one more input
//! inside the documented format limits whose peak exceeded a bound that had
//! been derived by hand from those limits. Hand derivation does not work for
//! this quantity, so no target attempts it any more.
//!
//! Each target instead states an **enumerated family** of near-limit and
//! hostile inputs, one line per input, and sets its bound to the **maximum
//! peak measured over that family** times a small stated multiplier. The
//! comment gives the measured maximum and which input produced it.
//!
//! **The true worst case over all inputs inside the documented limits is not
//! derived and may be higher than any bound here.** A bound is an observed
//! envelope that guards against regression, not a proof of a ceiling. The
//! invariant these targets actually establish is: no panic, no abort, no write
//! outside the sandbox, and allocation bounded by a function of the documented
//! limits — not a closed-form ceiling on that function.

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime};

use proptest::prelude::prop_oneof;
use proptest::strategy::Strategy;
use proptest::test_runner::{
    Config, FileFailurePersistence, RngAlgorithm, TestCaseError, TestCaseResult, TestRng,
    TestRunner,
};

/// Iterations per target when no budget is requested, kept small so the
/// ordinary `cargo test` run stays fast.
const DEFAULT_ITERATIONS: u32 = 48;
const DEFAULT_MAX_SHRINK_ITERS: u32 = 1_024;
/// Cases per `TestRunner`; the wall-clock budget is checked between chunks.
const CHUNK_CASES: u32 = 64;

struct CountingAllocator;

static LIVE_BYTES: AtomicUsize = AtomicUsize::new(0);
static PEAK_BYTES: AtomicUsize = AtomicUsize::new(0);
static MEASURING: AtomicBool = AtomicBool::new(false);
static SANDBOX_NONCE: AtomicU64 = AtomicU64::new(0);
static IN_ISOLATED_CHILD: AtomicBool = AtomicBool::new(false);

/// Passed to the isolated child and asserted there. The allocation counters are
/// process-wide, so every measurement depends on this.
const SINGLE_THREAD_ARGUMENT: &str = "--test-threads=1";

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            allocated(layout.size());
        }
        pointer
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if !pointer.is_null() {
            allocated(layout.size());
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        LIVE_BYTES.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(pointer, layout) };
    }

    unsafe fn realloc(&self, pointer: *mut u8, old: Layout, new_size: usize) -> *mut u8 {
        let resized = unsafe { System.realloc(pointer, old, new_size) };
        if !resized.is_null() {
            if new_size >= old.size() {
                allocated(new_size - old.size());
            } else {
                LIVE_BYTES.fetch_sub(old.size() - new_size, Ordering::Relaxed);
            }
        }
        resized
    }
}

fn allocated(bytes: usize) {
    let live = LIVE_BYTES.fetch_add(bytes, Ordering::Relaxed) + bytes;
    if MEASURING.load(Ordering::Relaxed) {
        PEAK_BYTES.fetch_max(live, Ordering::Relaxed);
    }
}

/// Runs `body` in a re-executed child confined to a fresh sandbox directory.
///
/// The child gets `TMPDIR`, `HOME`, and its working directory inside the
/// sandbox. On macOS `sandbox-exec` additionally denies every write outside it,
/// so an escape terminates the child and fails the target. On every platform
/// the child, which runs this one test alone, proves afterwards that every
/// regular file under the package directory is unchanged in length and
/// modification time. That is what [`snapshot_tree`] compares; it is not a
/// content comparison, though any real write moves the modification time.
///
/// The child is also the only place [`bounded`] may measure. Its allocation
/// counters are process-wide, so a concurrent thread freeing memory would drive
/// live bytes below the baseline and silence the bound; the child therefore
/// runs with `--test-threads=1` and asserts that it received it.
pub fn isolated(test_name: &str, body: fn()) {
    if std::env::var("RUSTRACE_FUZZ_CHILD").as_deref() == Ok(test_name) {
        assert!(
            std::env::args().any(|argument| argument == SINGLE_THREAD_ARGUMENT),
            "the isolated child must run with {SINGLE_THREAD_ARGUMENT}; \
             the allocation bound is not measurable otherwise"
        );
        IN_ISOLATED_CHILD.store(true, Ordering::SeqCst);
        let package = Path::new(env!("CARGO_MANIFEST_DIR"));
        let before = snapshot_tree(package);
        body();
        assert_eq!(
            before,
            snapshot_tree(package),
            "fuzz target {test_name} changed a file's length or modification time inside {}",
            package.display()
        );
        return;
    }

    let nonce = SANDBOX_NONCE.fetch_add(1, Ordering::Relaxed);
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("rustrace-fuzz-{}-{nonce}", std::process::id()));
    std::fs::create_dir_all(&root).expect("create fuzz sandbox");

    let executable = std::env::current_exe().expect("resolve fuzz test executable");
    let mut command;
    #[cfg(target_os = "macos")]
    {
        let escaped = root.to_string_lossy().replace('"', "\\\"");
        let profile = format!(
            "(version 1) (allow default) (deny file-write*) (allow file-write* (subpath \"{escaped}\"))"
        );
        command = Command::new("/usr/bin/sandbox-exec");
        command.args(["-p", &profile]).arg(&executable);
    }
    #[cfg(not(target_os = "macos"))]
    {
        command = Command::new(&executable);
    }

    let status = command
        .args(["--exact", test_name, "--nocapture", SINGLE_THREAD_ARGUMENT])
        .current_dir(&root)
        .env("RUSTRACE_FUZZ_CHILD", test_name)
        .env("TMPDIR", &root)
        .env("TMP", &root)
        .env("TEMP", &root)
        .env("HOME", &root)
        .status()
        .expect("run isolated fuzz target");

    std::fs::remove_dir_all(&root).expect("remove fuzz sandbox");
    assert!(
        status.success(),
        "fuzz target {test_name} aborted or failed"
    );
}

/// Records every regular file under `root` as (length, modification time).
fn snapshot_tree(root: &Path) -> BTreeMap<PathBuf, (u64, SystemTime)> {
    let mut snapshot = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if metadata.is_dir() {
                pending.push(entry.path());
            } else {
                let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                snapshot.insert(entry.path(), (metadata.len(), modified));
            }
        }
    }
    snapshot
}

fn observe_peak<T>(body: impl FnOnce() -> T) -> Result<(T, usize), Box<dyn std::any::Any + Send>> {
    let baseline = LIVE_BYTES.load(Ordering::Relaxed);
    PEAK_BYTES.store(baseline, Ordering::Relaxed);
    MEASURING.store(true, Ordering::SeqCst);
    let outcome = catch_unwind(AssertUnwindSafe(body));
    MEASURING.store(false, Ordering::SeqCst);
    let peak = PEAK_BYTES.load(Ordering::Relaxed).saturating_sub(baseline);
    outcome.map(|value| (value, peak))
}

/// Calls a parser and fails the case on a panic or on a peak allocation above
/// `maximum` bytes.
///
/// `maximum` is an observed envelope over the calling target's enumerated
/// input family, not a derived ceiling; see the module comment.
///
/// Only valid inside an [`isolated`] child: the allocation counters are
/// process-wide, and a concurrent deallocation would drop live bytes below the
/// baseline so the bound stopped applying with nothing to signal it.
pub fn bounded<T>(maximum: usize, body: impl FnOnce() -> T) -> Result<T, TestCaseError> {
    measured(maximum, body).map(|(value, _)| value)
}

/// Same as [`bounded`], and also reports the peak, so a boundary case can
/// require the parser to have reached, or to have stayed well below, the
/// allocation it is written to exercise.
pub fn measured<T>(maximum: usize, body: impl FnOnce() -> T) -> Result<(T, usize), TestCaseError> {
    assert!(
        IN_ISOLATED_CHILD.load(Ordering::Relaxed),
        "bounded must run inside an isolated single-threaded child"
    );
    let (value, peak) = observe_peak(body).map_err(|payload| {
        TestCaseError::fail(format!("target panicked: {}", panic_text(&*payload)))
    })?;
    if peak > maximum {
        return Err(TestCaseError::fail(format!(
            "peak allocation {peak} exceeded the {maximum}-byte bound"
        )));
    }
    Ok((value, peak))
}

/// Same as [`bounded`] outside a proptest case, for fixed boundary inputs.
// Each target includes this module with #[path], so every target compiles its
// own copy and uses only the helpers it needs.
#[allow(dead_code, reason = "not every target calls every helper")]
pub fn assert_bounded<T>(label: &str, maximum: usize, body: impl FnOnce() -> T) -> T {
    assert_measured(label, maximum, body).0
}

/// Same as [`measured`] outside a proptest case, for fixed boundary inputs.
pub fn assert_measured<T>(label: &str, maximum: usize, body: impl FnOnce() -> T) -> (T, usize) {
    match measured(maximum, body) {
        Ok(result) => result,
        Err(error) => panic!("{label}: {error}"),
    }
}

fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "non-string panic payload".to_owned()
    }
}

/// Runs one fuzz target for its iteration budget from a fixed seed.
///
/// `RUSTRACE_FUZZ_ITERATIONS` raises the case count and `RUSTRACE_FUZZ_SECONDS`
/// caps the wall-clock spent per target; the extended CI budget uses both. The
/// budget is spent in chunks so the deadline can end a target between chunks:
/// a `TestRunner` never resets its success counter, so it must be rebuilt to
/// run more cases, and the run RNG is carried forward to keep the whole
/// sequence reproducible from the one seed.
pub fn run_cases<S, F>(name: &str, seed: [u8; 32], strategy: S, target: F)
where
    S: Strategy,
    F: Fn(S::Value) -> TestCaseResult,
{
    run_bounded_cases(name, seed, iteration_budget(), strategy, target);
}

/// Runs a fuzz target for an explicit case count and reports how many cases
/// actually ran, which the wall-clock budget can cut short.
pub fn run_bounded_cases<S, F>(
    name: &str,
    seed: [u8; 32],
    maximum: u32,
    strategy: S,
    target: F,
) -> u32
where
    S: Strategy,
    F: Fn(S::Value) -> TestCaseResult,
{
    let deadline = env_u32("RUSTRACE_FUZZ_SECONDS")
        .map(|seconds| Instant::now() + Duration::from_secs(u64::from(seconds)));
    let mut rng = TestRng::from_seed(RngAlgorithm::ChaCha, &seed);
    let mut completed = 0_u32;
    while completed < maximum && deadline.is_none_or(|limit| Instant::now() < limit) {
        let chunk = CHUNK_CASES.min(maximum - completed);
        let config = Config {
            cases: chunk,
            max_shrink_iters: DEFAULT_MAX_SHRINK_ITERS,
            failure_persistence: Some(Box::new(FileFailurePersistence::Off)),
            ..Config::default()
        };
        let mut runner = TestRunner::new_with_rng(config, rng.clone());
        runner
            .run(&strategy, &target)
            .unwrap_or_else(|error| panic!("{name} failed after {completed} iterations: {error}"));
        completed += chunk;
        rng = runner.rng().clone();
    }
    println!(
        "FUZZ_RESULT target={name} iterations={completed} seed={}",
        hex_seed(seed)
    );
    completed
}

/// The iteration budget every target runs, so a caller can check it.
pub fn iteration_budget() -> u32 {
    env_u32("RUSTRACE_FUZZ_ITERATIONS").unwrap_or(DEFAULT_ITERATIONS)
}

fn env_u32(name: &str) -> Option<u32> {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|value| *value > 0)
}

fn hex_seed(seed: [u8; 32]) -> String {
    seed.into_iter().map(|byte| format!("{byte:02x}")).collect()
}

/// One byte-level corruption applied to a valid production fixture.
#[derive(Clone, Debug)]
pub enum Mutation {
    FlipBit {
        position: u16,
        bit: u8,
    },
    SetByte {
        position: u16,
        value: u8,
    },
    Truncate {
        keep: u16,
    },
    Duplicate {
        position: u16,
        length: u16,
    },
    Insert {
        position: u16,
        value: u8,
        length: u16,
    },
    Remove {
        position: u16,
        length: u16,
    },
}

/// Byte-level mutations: bit flips, truncation, duplication, insertion, and
/// removal, biased towards short edit sequences that still shrink well.
pub fn mutations(maximum_operations: usize) -> impl Strategy<Value = Vec<Mutation>> {
    let operation = prop_oneof![
        (0_u16..=u16::MAX, 0_u8..8).prop_map(|(position, bit)| Mutation::FlipBit { position, bit }),
        (0_u16..=u16::MAX, 0_u8..=u8::MAX)
            .prop_map(|(position, value)| Mutation::SetByte { position, value }),
        (0_u16..=u16::MAX).prop_map(|keep| Mutation::Truncate { keep }),
        (0_u16..=u16::MAX, 1_u16..512)
            .prop_map(|(position, length)| Mutation::Duplicate { position, length }),
        (0_u16..=u16::MAX, 0_u8..=u8::MAX, 1_u16..512).prop_map(|(position, value, length)| {
            Mutation::Insert {
                position,
                value,
                length,
            }
        }),
        (0_u16..=u16::MAX, 1_u16..512)
            .prop_map(|(position, length)| Mutation::Remove { position, length }),
    ];
    proptest::collection::vec(operation, 1..=maximum_operations.max(1))
}

/// Applies `operations` in order, keeping the result at or below `ceiling`
/// bytes so a corrupted length never makes the harness itself unbounded.
pub fn apply_mutations(seed: &[u8], operations: &[Mutation], ceiling: usize) -> Vec<u8> {
    let mut bytes = seed.to_vec();
    for operation in operations {
        match *operation {
            Mutation::FlipBit { position, bit } => {
                if let Some(index) = index_in(&bytes, position) {
                    bytes[index] ^= 1 << bit;
                }
            }
            Mutation::SetByte { position, value } => {
                if let Some(index) = index_in(&bytes, position) {
                    bytes[index] = value;
                }
            }
            Mutation::Truncate { keep } => {
                let keep = usize::from(keep).min(bytes.len());
                bytes.truncate(keep);
            }
            Mutation::Duplicate { position, length } => {
                if let Some(index) = index_in(&bytes, position) {
                    let end = (index + usize::from(length)).min(bytes.len());
                    let slice = bytes[index..end].to_vec();
                    let room = ceiling.saturating_sub(bytes.len()).min(slice.len());
                    bytes.splice(end..end, slice[..room].iter().copied());
                }
            }
            Mutation::Insert {
                position,
                value,
                length,
            } => {
                let index = index_in(&bytes, position).unwrap_or(0);
                let room = ceiling.saturating_sub(bytes.len()).min(usize::from(length));
                bytes.splice(index..index, std::iter::repeat_n(value, room));
            }
            Mutation::Remove { position, length } => {
                if let Some(index) = index_in(&bytes, position) {
                    let end = (index + usize::from(length)).min(bytes.len());
                    bytes.drain(index..end);
                }
            }
        }
    }
    bytes.truncate(ceiling);
    bytes
}

fn index_in(bytes: &[u8], position: u16) -> Option<usize> {
    (!bytes.is_empty()).then(|| usize::from(position) % bytes.len())
}
