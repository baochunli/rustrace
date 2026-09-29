//! Small local state for the embedded piped Cargo console.

use rustrace_model::{Hash, MAX_TEST_CASE_ARGS_FILE_BYTES, WorkspacePath, parse_test_case_args};
use rustrace_workspace::hash::PinnedWorkspaceRoot;
use rustrace_workspace::{
    OpenedRegularFile,
    assignment_package::{MAX_TEST_CASE_FILE_BYTES, MAX_TEST_CASE_TOTAL_BYTES, MAX_TEST_CASES},
    create_external_regular_file_in, external_regular_file_exists_in,
    fixture_tree::{
        FIXTURE_ROOT, FixtureTree, PinnedFixtureRoot, open_deployed_fixture_root,
        read_deployed_fixture_tree,
    },
    list_external_regular_files_in_with_filter, open_external_regular_file_read_in,
    open_external_regular_file_write_in,
};
use std::{
    collections::BTreeMap,
    error::Error,
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};
use unicode_segmentation::UnicodeSegmentation;

pub(crate) const MAX_CONSOLE_LINE_BYTES: usize = 4096;
pub(crate) const MAX_TEST_CASE_LINE_PREVIEW_INPUT_BYTES: usize = 256;
pub(crate) const MAX_TEST_CASE_LINE_PREVIEW_OUTPUT_BYTES: usize = 128;

const _: () =
    assert!(rustrace_model::MAX_TEST_CASE_EXPECTED_LINE_BYTES == MAX_TEST_CASE_FILE_BYTES);

type Result<T> = std::result::Result<T, Box<dyn Error>>;

static TEST_CASE_REFRESH_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OutputDisposition {
    CreateNew,
    Overwrite,
}

/// The one case folder beside every format 1 or 2 workspace.
pub(crate) const FIXED_TEST_CASE_FOLDER: &str = "test-cases";
/// The suffix of a format 3 workspace's own case folder.
pub(crate) const TEST_CASE_FOLDER_SUFFIX: &str = ".test-cases";
const WORKSPACE_SUFFIX: &str = ".work";
const MAX_FOLDER_NAME_BYTES: usize = 255;

/// Where a session's packaged cases live and which files make a case.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TestCaseLayout {
    /// Formats 1 and 2: the shared sibling `test-cases/`, where a case is a
    /// complete `NAME.in` and `NAME.expected` pair.
    Paired,
    /// Format 3: the workspace's own sibling `NAME.test-cases/`, where a case
    /// is `NAME.expected` with optional `NAME.in` and `NAME.args`, beside an
    /// optional `files/` fixture tree.
    Extended,
}

impl TestCaseLayout {
    pub(crate) fn for_format_version(format_version: u32) -> Self {
        if format_version == 3 {
            Self::Extended
        } else {
            Self::Paired
        }
    }

    /// The case folder's name beside a workspace directory named
    /// `workspace_name`. Format 3 derives it from the workspace: `lab2.work`
    /// gets `lab2.test-cases`, and any other name gets `.test-cases`
    /// appended. A name that could be another workspace's case folder, the
    /// shared format 2 folder, or a Cargo template is refused.
    pub(crate) fn folder_name(self, workspace_name: &std::ffi::OsStr) -> Result<String> {
        match self {
            Self::Paired => {
                if workspace_name == FIXED_TEST_CASE_FOLDER {
                    return Err(
                        "the selected workspace cannot be the fixed sibling test-cases directory"
                            .into(),
                    );
                }
                Ok(FIXED_TEST_CASE_FOLDER.to_owned())
            }
            Self::Extended => {
                let name = workspace_name
                    .to_str()
                    .ok_or("a format 3 workspace name must be valid UTF-8")?;
                let lowercase = name.to_ascii_lowercase();
                if lowercase == FIXED_TEST_CASE_FOLDER
                    || lowercase.ends_with(TEST_CASE_FOLDER_SUFFIX)
                {
                    return Err(format!(
                        "a format 3 workspace cannot be named `{FIXED_TEST_CASE_FOLDER}` or end in `{TEST_CASE_FOLDER_SUFFIX}`, which name test-case folders"
                    )
                    .into());
                }
                if name.contains(['{', '}']) {
                    return Err(
                        "a format 3 workspace name cannot contain `{` or `}`, which Cargo reads as build-directory template variables"
                            .into(),
                    );
                }
                let stem = name.strip_suffix(WORKSPACE_SUFFIX).unwrap_or(name);
                if stem.is_empty() {
                    return Err("a format 3 workspace needs a name before `.work`".into());
                }
                let folder = format!("{stem}{TEST_CASE_FOLDER_SUFFIX}");
                if folder.len() > MAX_FOLDER_NAME_BYTES {
                    return Err(format!(
                        "the test-case folder name `{folder}` would exceed {MAX_FOLDER_NAME_BYTES} bytes"
                    )
                    .into());
                }
                Ok(folder)
            }
        }
    }
}

/// The sibling case folder of `workspace_root` for this layout.
pub(crate) fn test_case_folder(workspace_root: &Path, layout: TestCaseLayout) -> Result<PathBuf> {
    let name = workspace_root
        .file_name()
        .ok_or("the workspace must name a directory")?;
    let parent = workspace_root
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .ok_or("workspace has no parent for sibling test-cases")?;
    Ok(parent.join(layout.folder_name(name)?))
}

/// Rustrace's ownership record at the top of a format 3 case folder. Its name
/// is not a case file, so packages cannot contain it and listings skip it, and
/// it lies outside `files/`, so the fixture-tree hash never covers it.
pub(crate) const CASE_FOLDER_MARKER: &str = ".rustrace-cases.json";
const MAX_CASE_FOLDER_MARKER_BYTES: u64 = 4096;

/// Which workspace and packaged suite a format 3 case folder belongs to.
/// `lab2` and `lab2.work` both map to `lab2.test-cases`, and one workspace
/// name can meet two package versions, so deployment and opening check it.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CaseFolderMarker {
    pub version: u32,
    /// The owning workspace directory's name.
    pub workspace: String,
    pub test_case_suite_hash: Hash,
}

impl CaseFolderMarker {
    pub(crate) fn new(workspace: &str, test_case_suite_hash: Hash) -> Self {
        Self {
            version: 1,
            workspace: workspace.to_owned(),
            test_case_suite_hash,
        }
    }

    /// Compact JSON followed by one LF.
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut bytes = serde_json::to_vec(self).expect("marker fields serialize");
        bytes.push(b'\n');
        bytes
    }

    /// Reads the marker of a case folder, or `None` when it has none.
    pub(crate) fn read(root: &PinnedWorkspaceRoot) -> Result<Option<Self>> {
        let path = WorkspacePath::new(CASE_FOLDER_MARKER).expect("valid marker name");
        if !external_regular_file_exists_in(root, &path)? {
            return Ok(None);
        }
        let mut opened = open_external_regular_file_read_in(root, &path)?;
        let mut bytes = Vec::new();
        opened
            .file_mut()
            .take(MAX_CASE_FOLDER_MARKER_BYTES + 1)
            .read_to_end(&mut bytes)?;
        root.verify_binding()?;
        let marker = serde_json::from_slice::<Self>(&bytes)
            .ok()
            .filter(|marker| {
                bytes.len() as u64 <= MAX_CASE_FOLDER_MARKER_BYTES
                    && marker.version == 1
                    && marker.encode() == bytes
            })
            .ok_or_else(|| {
                format!(
                    "the test-case folder `{}` has an unreadable {CASE_FOLDER_MARKER}",
                    root.path().display()
                )
            })?;
        Ok(Some(marker))
    }

    /// Requires `root` to be a case folder Rustrace created for the workspace
    /// named `workspace` and, when given, for the packaged suite `suite`.
    pub(crate) fn require(
        root: &PinnedWorkspaceRoot,
        workspace: &str,
        suite: Option<Hash>,
    ) -> Result<()> {
        let folder = root.path().display();
        let marker = Self::read(root)?.ok_or_else(|| {
            format!(
                "the test-case folder `{folder}` was not created by Rustrace for this workspace; move it aside and run the same command again"
            )
        })?;
        if marker.workspace != workspace {
            return Err(format!(
                "the test-case folder `{folder}` belongs to the workspace `{}`, not `{workspace}`; choose another workspace name",
                crate::display::label(&marker.workspace, 256)
            )
            .into());
        }
        if suite.is_some_and(|suite| suite != marker.test_case_suite_hash) {
            return Err(format!(
                "the test-case folder `{folder}` holds the cases of a different assignment package; move it aside and run the same command again"
            )
            .into());
        }
        Ok(())
    }
}

/// Refuses a would-be case folder that is itself a Rustrace workspace.
pub(crate) fn refuse_workspace_as_case_folder(folder: &Path) -> Result<()> {
    match fs::symlink_metadata(folder.join(".rustrace")) {
        Ok(_) => Err(format!(
            "`{}` is a Rustrace workspace, not a test-case folder",
            folder.display()
        )
        .into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// The name a case-folder marker records for a workspace: its name on disk
/// when it exists, so a differently cased spelling on macOS still matches.
pub(crate) fn case_folder_owner(workspace_root: &Path) -> Result<String> {
    let resolved = match fs::canonicalize(workspace_root) {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => workspace_root.to_owned(),
        Err(error) => return Err(error.into()),
    };
    Ok(resolved
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("a format 3 workspace name must be valid UTF-8")?
        .to_owned())
}

/// How a deployed fixture tree compares with the one the package declared.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FixtureTreeCheck {
    /// The package has no fixture tree, so the program runs in the workspace.
    /// `deployed` reports whether a `files/` folder exists anyway.
    NotPackaged { deployed: bool },
    /// The deployed tree hashes exactly as packaged.
    Matches { fixtures_blake3: Hash },
    /// The deployed tree is missing or differs; warn, and record `deployed`.
    Differs {
        packaged: Hash,
        deployed: Option<Hash>,
    },
}

impl FixtureTreeCheck {
    fn classify(packaged: Option<Hash>, deployed: Option<Hash>) -> Self {
        match (packaged, deployed) {
            (None, deployed) => Self::NotPackaged {
                deployed: deployed.is_some(),
            },
            (Some(packaged), Some(deployed)) if packaged == deployed => Self::Matches {
                fixtures_blake3: deployed,
            },
            (Some(packaged), deployed) => Self::Differs { packaged, deployed },
        }
    }
}

/// The deployed fixture folder a format 3 Run starts in, held open from the
/// moment it was hashed until the program is launched in it. Holding it open
/// fixes which directory the program starts in, not what the directory
/// contains by then.
#[derive(Debug)]
pub(crate) struct PinnedFixtures {
    pub root: PinnedFixtureRoot,
    /// [`FixtureTreeCheck::Matches`] or [`FixtureTreeCheck::Differs`] with the
    /// deployed hash.
    pub check: FixtureTreeCheck,
    /// `NAME.test-cases/files`, for messages.
    pub display: String,
}

impl PinnedFixtures {
    /// The deployed tree's hash when it was pinned.
    pub fn deployed(&self) -> Hash {
        match self.check {
            FixtureTreeCheck::Matches { fixtures_blake3 } => fixtures_blake3,
            FixtureTreeCheck::Differs {
                deployed: Some(deployed),
                ..
            } => deployed,
            _ => unreachable!("a pinned fixture tree was hashed"),
        }
    }

    /// The student-facing warning for a deployed tree that differs from the
    /// package, or `None` when it matches. A packaged case run then records
    /// the deployed hash and no longer verifies against the package.
    pub fn warning(&self, test_case: bool) -> Option<String> {
        matches!(self.check, FixtureTreeCheck::Differs { .. })
            .then(|| fixtures_changed_warning(&self.display, test_case))
    }
}

const FIXTURES_CHANGED_PREFIX: &str = "warning: the files in ";
const FIXTURES_CHANGED_DIFFER: &str = " differ from the assignment package; ";

/// Warns that a Run uses fixture files that differ from the package. The text
/// keeps "warning" and none of the words that make a toast an error.
pub(crate) fn fixtures_changed_warning(folder: &str, test_case: bool) -> String {
    let consequence = if test_case {
        "the case runs with them as they are and will not verify against the package"
    } else {
        "your program runs with them as they are"
    };
    crate::display::label(
        &format!(
            "{FIXTURES_CHANGED_PREFIX}{folder}{FIXTURES_CHANGED_DIFFER}{consequence}. To restore them, remove the files you changed or added, then quit and resume the workspace"
        ),
        512,
    )
}

/// Whether a status is a [`fixtures_changed_warning`], which the editor keeps
/// showing after the short run it warned about finishes.
pub(crate) fn is_fixtures_changed_warning(status: &str) -> bool {
    status.starts_with(FIXTURES_CHANGED_PREFIX) && status.contains(FIXTURES_CHANGED_DIFFER)
}

/// The files of one format 3 case, read fresh from the case folder just
/// before a Run rather than taken from an earlier listing.
#[derive(Debug)]
pub(crate) struct ExtendedCaseFiles {
    /// Parsed `NAME.args`; empty when the case has no such file.
    pub args: Vec<String>,
    /// The `NAME.in` bytes and their BLAKE3, or `None` for closed stdin.
    pub input: Option<(Vec<u8>, Hash)>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct TestCase {
    name: String,
    pair_blake3: Option<Hash>,
    has_input: bool,
    /// Parsed `NAME.args`; empty without the file, `None` when a present file
    /// cannot be read or parsed. Format 2 cases never have arguments.
    args: Option<Vec<String>>,
}

impl TestCase {
    pub(crate) fn new(name: impl Into<String>) -> Result<Self> {
        let name = name.into();
        if !rustrace_model::is_valid_test_case_name(&name) {
            return Err("invalid packaged test-case name".into());
        }
        Ok(Self {
            name,
            pair_blake3: None,
            has_input: true,
            args: Some(Vec::new()),
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Whether the case has a `NAME.in`; without one it runs with stdin closed.
    pub(crate) fn has_input(&self) -> bool {
        self.has_input
    }

    /// The case's program arguments, or `None` when its `NAME.args` is invalid.
    pub(crate) fn args(&self) -> Option<&[String]> {
        self.args.as_deref()
    }

    pub(crate) fn input_path(&self) -> WorkspacePath {
        WorkspacePath::new(format!("{}.in", self.name)).expect("validated test-case path")
    }

    pub(crate) fn args_path(&self) -> WorkspacePath {
        WorkspacePath::new(format!("{}.args", self.name)).expect("validated test-case path")
    }

    pub(crate) fn expected_path(&self) -> WorkspacePath {
        WorkspacePath::new(format!("{}.expected", self.name)).expect("validated test-case path")
    }
}

fn refresh_identity(refresh_id: u64, name: &str) -> Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"rustrace.live-test-case-refresh.v1");
    hasher.update(&refresh_id.to_le_bytes());
    hasher.update(&(name.len() as u64).to_le_bytes());
    hasher.update(name.as_bytes());
    Hash::from_bytes(*hasher.finalize().as_bytes())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TestCaseComparison {
    pub case: TestCase,
    pub outcome: TestCaseOutcome,
    pub expected_blake3: Option<Hash>,
    pub actual_blake3: Option<Hash>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TestCaseOutcome {
    Pass,
    Fail(TestCaseMismatch),
    Error(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TestCaseMismatch {
    pub line: u64,
    pub expected_len: usize,
    pub actual_len: usize,
    pub expected_preview: String,
    pub actual_preview: String,
}

/// Authority for the one fixed sibling `test-cases/` directory.
#[derive(Debug)]
pub(crate) struct TestCaseDirectory {
    root: PinnedWorkspaceRoot,
}

impl TestCaseDirectory {
    /// Opens the session's case folder: the fixed `test-cases/` for formats
    /// 1 and 2, or the workspace's own `NAME.test-cases/` for format 3, whose
    /// marker must name this workspace and, when given, the session's suite.
    /// A folder that is itself a Rustrace workspace is refused in any format.
    pub(crate) fn open(
        workspace_root: &Path,
        layout: TestCaseLayout,
        suite: Option<Hash>,
    ) -> Result<Self> {
        let workspace = PinnedWorkspaceRoot::open(workspace_root)?;
        let expected = test_case_folder(workspace.path(), layout)?;
        let metadata = fs::symlink_metadata(&expected)?;
        if metadata.file_type().is_symlink() {
            return Err("the fixed sibling test-cases root must not be a symlink".into());
        }
        let root = PinnedWorkspaceRoot::open(&expected)?;
        if root.path() != expected {
            return Err("the fixed sibling test-cases root is not canonical".into());
        }
        root.verify_binding()?;
        workspace.verify_binding()?;
        if root.is_same_directory(&workspace) {
            return Err("the fixed sibling test-cases root aliases the selected workspace".into());
        }
        refuse_workspace_as_case_folder(root.path())?;
        if layout == TestCaseLayout::Extended {
            let owner = workspace
                .path()
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or("a format 3 workspace name must be valid UTF-8")?;
            CaseFolderMarker::require(&root, owner, suite)?;
        }
        root.verify_binding()?;
        Ok(Self { root })
    }

    pub(crate) fn list(&self) -> Result<Vec<WorkspacePath>> {
        self.list_for(TestCaseLayout::Paired)
    }

    fn list_for(&self, layout: TestCaseLayout) -> Result<Vec<WorkspacePath>> {
        Ok(match layout {
            TestCaseLayout::Paired => list_external_regular_files_in_with_filter(
                &self.root,
                MAX_TEST_CASES * 2,
                is_test_case_candidate,
            )?,
            TestCaseLayout::Extended => list_external_regular_files_in_with_filter(
                &self.root,
                MAX_TEST_CASES * 3,
                is_extended_test_case_candidate,
            )?,
        })
    }

    /// Lists live cases for one assignment format. Format 2 lists complete
    /// `.in`/`.expected` pairs exactly as [`Self::list_cases`] does.
    pub(crate) fn list_cases_for(&self, layout: TestCaseLayout) -> Result<Vec<TestCase>> {
        match layout {
            TestCaseLayout::Paired => self.list_cases(),
            TestCaseLayout::Extended => self.list_extended_cases(),
        }
    }

    fn list_extended_cases(&self) -> Result<Vec<TestCase>> {
        #[derive(Default)]
        struct Files {
            input: bool,
            args: bool,
            expected: bool,
        }

        let mut groups = BTreeMap::<String, Files>::new();
        for path in self.list_for(TestCaseLayout::Extended)? {
            let Some((name, suffix)) = path.as_str().rsplit_once('.') else {
                continue;
            };
            if path.as_str().contains('/') || TestCase::new(name).is_err() {
                continue;
            }
            let files = groups.entry(name.to_owned()).or_default();
            match suffix {
                "in" => files.input = true,
                "args" => files.args = true,
                "expected" => files.expected = true,
                _ => {}
            }
        }
        let refresh_id = TEST_CASE_REFRESH_ID.fetch_add(1, Ordering::Relaxed);
        let mut remaining_identity_bytes = MAX_TEST_CASE_TOTAL_BYTES;
        let mut cases = Vec::new();
        for (name, files) in groups {
            if !files.expected {
                continue;
            }
            let mut case = TestCase {
                pair_blake3: Some(refresh_identity(refresh_id, &name)),
                name,
                has_input: files.input,
                args: Some(Vec::new()),
            };
            if files.args {
                case.args = self.read_args(&case).ok();
            }
            if remaining_identity_bytes > 0
                && let Ok(identity) =
                    self.extended_identity(&case, files.args, &mut remaining_identity_bytes)
            {
                case.pair_blake3 = Some(identity);
            }
            cases.push(case);
        }
        Ok(cases)
    }

    /// Reads and parses the live `NAME.args` with the package rules.
    pub(crate) fn read_args(&self, case: &TestCase) -> Result<Vec<String>> {
        let mut opened = open_external_regular_file_read_in(&self.root, &case.args_path())?;
        let mut bytes = Vec::with_capacity(MAX_TEST_CASE_ARGS_FILE_BYTES);
        opened
            .file_mut()
            .take(MAX_TEST_CASE_ARGS_FILE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        self.root.verify_binding()?;
        Ok(parse_test_case_args(&bytes).map_err(|error| format!("test-case arguments {error}"))?)
    }

    /// The fixture folder as students see it, `NAME.test-cases/files`, for
    /// messages and the picker.
    pub(crate) fn fixtures_display(&self) -> String {
        let folder = self
            .root
            .path()
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        crate::display::label(&format!("{folder}/{FIXTURE_ROOT}"), 300)
    }

    /// The size of a case's `NAME.in`, opened like a Run opens it.
    pub(crate) fn input_len(&self, case: &TestCase) -> Result<u64> {
        let opened = self.open_input(&case.input_path())?;
        let len = opened.file().metadata()?.len();
        self.root.verify_binding()?;
        Ok(len)
    }

    /// Opens the deployed `files/` folder by descriptor, or `None` when it is
    /// absent.
    pub(crate) fn open_fixture_root(&self) -> Result<Option<PinnedFixtureRoot>> {
        let root = open_deployed_fixture_root(&self.root)?;
        self.root.verify_binding()?;
        Ok(root)
    }

    /// Prepares the fixture folder a Run of a session whose package has the
    /// fixture tree `packaged` starts in. Cargo configuration in the case
    /// folder or `files/` is refused, a missing `files/` is refused because
    /// the Run has nowhere to start, and otherwise the folder is pinned and
    /// hashed. A tree that differs from the package still runs; the caller
    /// warns and records the deployed hash.
    pub(crate) fn pin_fixtures(&self, packaged: Hash) -> Result<PinnedFixtures> {
        self.reject_cargo_configuration()?;
        let display = self.fixtures_display();
        let root = self.open_fixture_root()?.ok_or_else(|| {
            format!(
                "the fixture folder {display} is missing; quit and resume the workspace to deploy it again"
            )
        })?;
        let deployed = root.hash()?;
        root.verify_binding()?;
        Ok(PinnedFixtures {
            root,
            check: FixtureTreeCheck::classify(Some(packaged), Some(deployed)),
            display,
        })
    }

    /// Reads a format 3 case's optional `NAME.args` and `NAME.in` as they are
    /// on disk now. `NAME.in` is read into memory (at most the 1 MiB case-file
    /// limit) so the program receives exactly the bytes that were hashed.
    pub(crate) fn read_extended_case(&self, case: &TestCase) -> Result<ExtendedCaseFiles> {
        let args = if external_regular_file_exists_in(&self.root, &case.args_path())? {
            self.read_args(case)?
        } else {
            Vec::new()
        };
        let input = if external_regular_file_exists_in(&self.root, &case.input_path())? {
            Some(self.read_input_with_blake3(case)?)
        } else {
            None
        };
        Ok(ExtendedCaseFiles { args, input })
    }

    /// Reads the deployed `files/` tree without following links, or `None`
    /// when it is absent. Symlinks, special files, and oversized trees fail.
    pub(crate) fn fixture_tree(&self) -> Result<Option<FixtureTree>> {
        Ok(read_deployed_fixture_tree(&self.root)?)
    }

    /// Reads the deployed `files/` tree once and compares it with the
    /// packaged fixture-tree hash that the session recorded at startup, so
    /// the picker's file list and its changed-files warning describe the same
    /// tree. An unreadable tree (for example one holding a symlink) is an
    /// error rather than a difference. Runs use [`Self::pin_fixtures`], which
    /// also keeps the folder open.
    pub(crate) fn check_fixture_tree(
        &self,
        packaged: Option<Hash>,
    ) -> Result<(FixtureTreeCheck, Option<FixtureTree>)> {
        let tree = self.fixture_tree()?;
        let check = FixtureTreeCheck::classify(packaged, tree.as_ref().map(FixtureTree::hash));
        Ok((check, tree))
    }

    /// Refuses a `.cargo` entry in the case folder or its `files/`. Cargo
    /// reads `.cargo/config.toml` from the directory a command runs in and
    /// from every parent, so a Run from `files/` must not start while one is
    /// there. Parents above the case folder are shared with the workspace.
    pub(crate) fn reject_cargo_configuration(&self) -> Result<()> {
        for relative in [".cargo".to_owned(), format!("{FIXTURE_ROOT}/.cargo")] {
            match fs::symlink_metadata(self.root.path().join(&relative)) {
                Ok(_) => {
                    return Err(format!(
                        "remove `{relative}` from the test-case folder: Cargo would read it as configuration"
                    )
                    .into());
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) => {}
                Err(error) => return Err(error.into()),
            }
        }
        self.root.verify_binding()?;
        Ok(())
    }

    fn extended_identity(
        &self,
        case: &TestCase,
        has_args: bool,
        remaining_bytes: &mut u64,
    ) -> Result<Hash> {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"rustrace.live-test-case.v2");
        for (present, path, oversized) in [
            (
                has_args,
                case.args_path(),
                "test arguments exceed the 1048576-byte limit",
            ),
            (
                case.has_input,
                case.input_path(),
                "test input exceeds the 1048576-byte limit",
            ),
            (
                true,
                case.expected_path(),
                "expected output exceeds the 1048576-byte limit",
            ),
        ] {
            if present {
                let bytes = self.read_case_file_for_identity(&path, oversized, remaining_bytes)?;
                hasher.update(&[1]);
                hasher.update(&(bytes.len() as u64).to_le_bytes());
                hasher.update(&bytes);
            } else {
                hasher.update(&[0]);
            }
        }
        Ok(Hash::from_bytes(*hasher.finalize().as_bytes()))
    }

    pub(crate) fn list_cases(&self) -> Result<Vec<TestCase>> {
        #[derive(Default)]
        struct Pair {
            input: bool,
            expected: bool,
        }

        let mut pairs = BTreeMap::<String, Pair>::new();
        for path in self.list()? {
            let value = path.as_str();
            if value.contains('/') {
                continue;
            }
            let (name, input) = if let Some(name) = value.strip_suffix(".in") {
                (name, true)
            } else if let Some(name) = value.strip_suffix(".expected") {
                (name, false)
            } else {
                continue;
            };
            let Ok(case) = TestCase::new(name) else {
                continue;
            };
            let pair = pairs.entry(case.name).or_default();
            if input {
                pair.input = true;
            } else {
                pair.expected = true;
            }
        }
        let refresh_id = TEST_CASE_REFRESH_ID.fetch_add(1, Ordering::Relaxed);
        let mut cases = pairs
            .into_iter()
            .filter_map(|(name, pair)| {
                (pair.input && pair.expected).then(|| TestCase {
                    pair_blake3: Some(refresh_identity(refresh_id, &name)),
                    name,
                    has_input: true,
                    args: Some(Vec::new()),
                })
            })
            .collect::<Vec<_>>();
        let mut remaining_identity_bytes = MAX_TEST_CASE_TOTAL_BYTES;
        for case in &mut cases {
            if remaining_identity_bytes == 0 {
                break;
            }
            let Ok(identity) = self.pair_identity(case, &mut remaining_identity_bytes) else {
                continue;
            };
            case.pair_blake3 = Some(identity);
        }
        Ok(cases)
    }

    pub(crate) fn open_input(&self, path: &WorkspacePath) -> Result<OpenedRegularFile> {
        Ok(open_external_regular_file_read_in(&self.root, path)?)
    }

    /// Reads a format 3 case's `NAME.in` (at most the 1 MiB case-file limit)
    /// and hashes it. The program receives exactly these bytes, and the hash
    /// is the `stdin.blake3` its comparison's invocation records.
    pub(crate) fn read_input_with_blake3(&self, case: &TestCase) -> Result<(Vec<u8>, Hash)> {
        let bytes = self.read_bounded_case_file(
            &case.input_path(),
            "test input exceeds the 1048576-byte limit",
        )?;
        let blake3 = hash_bytes(&bytes);
        Ok((bytes, blake3))
    }

    pub(crate) fn read_expected(&self, case: &TestCase) -> Result<Vec<u8>> {
        self.read_bounded_case_file(
            &case.expected_path(),
            "expected output exceeds the 1048576-byte limit",
        )
    }

    fn pair_identity(&self, case: &TestCase, remaining_bytes: &mut u64) -> Result<Hash> {
        let input = self.read_case_file_for_identity(
            &case.input_path(),
            "test input exceeds the 1048576-byte limit",
            remaining_bytes,
        )?;
        let expected = self.read_case_file_for_identity(
            &case.expected_path(),
            "expected output exceeds the 1048576-byte limit",
            remaining_bytes,
        )?;
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"rustrace.live-test-case-pair.v1");
        hasher.update(&(input.len() as u64).to_le_bytes());
        hasher.update(&input);
        hasher.update(&(expected.len() as u64).to_le_bytes());
        hasher.update(&expected);
        Ok(Hash::from_bytes(*hasher.finalize().as_bytes()))
    }

    fn read_case_file_for_identity(
        &self,
        path: &WorkspacePath,
        oversized: &str,
        remaining_bytes: &mut u64,
    ) -> Result<Vec<u8>> {
        let allowance = *remaining_bytes;
        if allowance == 0 {
            return Err("packaged test-case identity limit reached".into());
        }
        let read_limit = (MAX_TEST_CASE_FILE_BYTES + 1).min(allowance.saturating_add(1));
        let mut opened = open_external_regular_file_read_in(&self.root, path)?;
        let mut bytes = Vec::with_capacity(
            usize::try_from(read_limit)
                .unwrap_or(usize::MAX)
                .min(64 * 1024),
        );
        let read_result = opened.file_mut().take(read_limit).read_to_end(&mut bytes);
        let bytes_read = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        *remaining_bytes = remaining_bytes.saturating_sub(bytes_read.min(allowance));
        read_result?;
        if bytes_read > allowance {
            return Err("packaged test-case identity limit reached".into());
        }
        if bytes_read > MAX_TEST_CASE_FILE_BYTES {
            return Err(oversized.into());
        }
        self.root.verify_binding()?;
        Ok(bytes)
    }

    fn read_bounded_case_file(&self, path: &WorkspacePath, oversized: &str) -> Result<Vec<u8>> {
        let mut opened = open_external_regular_file_read_in(&self.root, path)?;
        let maximum = usize::try_from(MAX_TEST_CASE_FILE_BYTES)
            .expect("packaged test-case file limit fits usize");
        let mut bytes = Vec::with_capacity(maximum.min(64 * 1024));
        opened
            .file_mut()
            .take(MAX_TEST_CASE_FILE_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > maximum {
            return Err(oversized.into());
        }
        self.root.verify_binding()?;
        Ok(bytes)
    }

    pub(crate) fn output_disposition(&self, path: &WorkspacePath) -> Result<OutputDisposition> {
        Ok(if external_regular_file_exists_in(&self.root, path)? {
            OutputDisposition::Overwrite
        } else {
            OutputDisposition::CreateNew
        })
    }

    pub(crate) fn open_output(
        &self,
        path: &WorkspacePath,
        disposition: OutputDisposition,
    ) -> Result<OpenedRegularFile> {
        let opened = match disposition {
            OutputDisposition::CreateNew => create_external_regular_file_in(&self.root, path),
            OutputDisposition::Overwrite => open_external_regular_file_write_in(&self.root, path),
        };
        Ok(opened?)
    }
}

fn is_test_case_candidate(path: &WorkspacePath) -> bool {
    let value = path.as_str();
    if value.contains('/') {
        return false;
    }
    value
        .strip_suffix(".in")
        .or_else(|| value.strip_suffix(".expected"))
        .is_some_and(|name| TestCase::new(name).is_ok())
}

/// Format 3 case files are top-level; nothing below `files/` is a case.
fn is_extended_test_case_candidate(path: &WorkspacePath) -> bool {
    is_test_case_candidate(path)
        || !path.as_str().contains('/')
            && path
                .as_str()
                .strip_suffix(".args")
                .is_some_and(|name| TestCase::new(name).is_ok())
}

pub(crate) fn hash_bytes(bytes: &[u8]) -> Hash {
    Hash::from_bytes(*blake3::hash(bytes).as_bytes())
}

pub(crate) fn compare_test_case_bytes(
    case: TestCase,
    expected: &[u8],
    actual: &[u8],
) -> TestCaseComparison {
    let expected_blake3 = Some(hash_bytes(expected));
    let actual_blake3 = Some(hash_bytes(actual));
    let outcome = if expected == actual {
        TestCaseOutcome::Pass
    } else {
        TestCaseOutcome::Fail(first_mismatch(expected, actual))
    };
    TestCaseComparison {
        case,
        outcome,
        expected_blake3,
        actual_blake3,
    }
}

pub(crate) fn classify_test_case_result(
    case: TestCase,
    process_outcome: &rustrace_model::CommandOutcome,
    completeness: rustrace_model::CaptureCompleteness,
    actual: &[u8],
    expected: std::result::Result<Vec<u8>, String>,
) -> TestCaseComparison {
    use rustrace_model::{CaptureCompleteness, CommandOutcome, CommandTermination};

    let expected_blake3 = expected.as_ref().ok().map(|bytes| hash_bytes(bytes));
    let actual_blake3 =
        (completeness != CaptureCompleteness::Unavailable).then(|| hash_bytes(actual));
    let error = match process_outcome {
        CommandOutcome::LaunchFailed { .. } => Some("launch failed".to_owned()),
        CommandOutcome::Exited { code } if *code != 0 => Some(format!("exit {code}")),
        CommandOutcome::Terminated { reason, .. } => Some(
            match reason {
                CommandTermination::Cancelled => "cancelled",
                CommandTermination::Quit => "quit",
                CommandTermination::Deadline => "deadline exceeded",
                CommandTermination::OutputLimit => "output limit exceeded",
                CommandTermination::CaptureFailure => "capture failed",
                CommandTermination::Signal => "terminated by signal",
                CommandTermination::CleanupFailure => "process cleanup failed",
            }
            .to_owned(),
        ),
        CommandOutcome::Exited { code: 0 } => match completeness {
            CaptureCompleteness::Complete => None,
            CaptureCompleteness::Truncated => Some("stdout capture was truncated".to_owned()),
            CaptureCompleteness::ReadFailed => Some("stdout capture read failed".to_owned()),
            CaptureCompleteness::Unavailable => Some("stdout capture is unavailable".to_owned()),
        },
        CommandOutcome::Exited { .. } => unreachable!("nonzero exit handled above"),
    }
    .or_else(|| {
        expected.as_ref().err().map(|detail| {
            if detail.contains("exceeds the 1048576-byte limit") {
                "expected output is oversized".to_owned()
            } else {
                "expected output is unreadable".to_owned()
            }
        })
    });
    if let Some(reason) = error {
        return TestCaseComparison {
            case,
            outcome: TestCaseOutcome::Error(reason),
            expected_blake3,
            actual_blake3,
        };
    }
    compare_test_case_bytes(
        case,
        expected.as_ref().expect("comparable expected bytes"),
        actual,
    )
}

fn first_mismatch(expected: &[u8], actual: &[u8]) -> TestCaseMismatch {
    let mut expected_at = 0;
    let mut actual_at = 0;
    let mut line = 1_u64;
    loop {
        let expected_line = next_line(expected, expected_at);
        let actual_line = next_line(actual, actual_at);
        match (expected_line, actual_line) {
            (
                Some((expected, expected_lf, next_expected)),
                Some((actual, actual_lf, next_actual)),
            ) if expected == actual && expected_lf == actual_lf => {
                expected_at = next_expected;
                actual_at = next_actual;
                line += 1;
            }
            (expected, actual) => {
                let expected = expected.map_or(&[][..], |(line, _, _)| line);
                let actual = actual.map_or(&[][..], |(line, _, _)| line);
                return TestCaseMismatch {
                    line,
                    expected_len: expected.len(),
                    actual_len: actual.len(),
                    expected_preview: line_preview(expected),
                    actual_preview: line_preview(actual),
                };
            }
        }
    }
}

fn next_line(bytes: &[u8], at: usize) -> Option<(&[u8], bool, usize)> {
    let remaining = bytes.get(at..)?;
    if remaining.is_empty() {
        return None;
    }
    if let Some(relative_lf) = remaining.iter().position(|byte| *byte == b'\n') {
        Some((&remaining[..relative_lf], true, at + relative_lf + 1))
    } else {
        Some((remaining, false, bytes.len()))
    }
}

fn line_preview(bytes: &[u8]) -> String {
    crate::display::plain(
        bytes,
        crate::display::Limits {
            input_bytes: MAX_TEST_CASE_LINE_PREVIEW_INPUT_BYTES,
            output_bytes: MAX_TEST_CASE_LINE_PREVIEW_OUTPUT_BYTES,
            spans: crate::display::MAX_SPANS,
            lines: 1,
        },
    )
    .text
    .to_string()
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ConsoleLine {
    text: String,
    cursor: usize,
}

impl ConsoleLine {
    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    pub(crate) fn cursor(&self) -> usize {
        self.cursor
    }

    pub(crate) fn insert(&mut self, character: char) -> bool {
        if character.is_control()
            || self.text.len().saturating_add(character.len_utf8()) > MAX_CONSOLE_LINE_BYTES
        {
            return false;
        }
        self.text.insert(self.cursor, character);
        let desired = self.cursor + character.len_utf8();
        self.cursor = self
            .text
            .grapheme_indices(true)
            .map(|(index, _)| index)
            .chain(std::iter::once(self.text.len()))
            .find(|index| *index >= desired)
            .expect("text end is a grapheme boundary");
        true
    }

    pub(crate) fn left(&mut self) {
        self.cursor = self.text[..self.cursor]
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(index, _)| index);
    }

    pub(crate) fn right(&mut self) {
        self.cursor += self.text[self.cursor..]
            .graphemes(true)
            .next()
            .map_or(0, str::len);
    }

    pub(crate) fn home(&mut self) {
        self.cursor = 0;
    }

    pub(crate) fn end(&mut self) {
        self.cursor = self.text.len();
    }

    pub(crate) fn backspace(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        let old = self.cursor;
        self.left();
        self.text.replace_range(self.cursor..old, "");
        true
    }

    pub(crate) fn delete(&mut self) -> bool {
        let Some(grapheme) = self.text[self.cursor..].graphemes(true).next() else {
            return false;
        };
        self.text
            .replace_range(self.cursor..self.cursor + grapheme.len(), "");
        true
    }

    pub(crate) fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustrace_model::{CaptureCompleteness, CommandOutcome, CommandTermination};
    use std::{fs, os::unix::fs::symlink};

    fn fixture(name: &str) -> std::path::PathBuf {
        let parent =
            std::env::temp_dir().join(format!("rustrace-test-cases-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&parent);
        fs::create_dir(&parent).unwrap();
        fs::create_dir(parent.join("assignment.work")).unwrap();
        fs::canonicalize(parent).unwrap()
    }

    #[test]
    fn test_case_name_limit_matches_provenance_model() {
        let maximum = "a".repeat(rustrace_model::MAX_TEST_CASE_NAME_BYTES);
        assert!(TestCase::new(maximum.clone()).is_ok());
        assert!(TestCase::new(format!("{maximum}a")).is_err());
    }

    #[test]
    fn unicode_line_editing_uses_grapheme_boundaries() {
        let mut line = ConsoleLine::default();
        for character in "Ae\u{301}🦀Z".chars() {
            assert!(line.insert(character));
        }
        assert_eq!(line.text(), "Ae\u{301}🦀Z");
        assert_eq!(line.cursor(), line.text().len());

        line.left();
        line.left();
        assert_eq!(&line.text()[line.cursor()..], "🦀Z");
        assert!(line.delete());
        assert_eq!(line.text(), "Ae\u{301}Z");
        assert!(line.backspace());
        assert_eq!(line.text(), "AZ");
        line.home();
        line.right();
        assert_eq!(line.cursor(), 1);
        line.end();
        assert_eq!(line.cursor(), 2);
        assert_eq!(line.take(), "AZ");
        assert_eq!((line.text(), line.cursor()), ("", 0));
    }

    #[test]
    fn insertion_that_joins_neighbors_keeps_the_cursor_on_a_grapheme_boundary() {
        let mut line = ConsoleLine::default();
        assert!(line.insert('👩'));
        assert!(line.insert('💻'));
        line.home();
        line.right();
        assert!(line.insert('\u{200d}'));
        assert_eq!(line.text(), "👩‍💻");
        assert_eq!(line.cursor(), line.text().len());
        line.left();
        assert_eq!(line.cursor(), 0);
        line.right();
        assert_eq!(line.cursor(), line.text().len());
    }

    #[test]
    fn line_bound_is_utf8_atomic() {
        let mut line = ConsoleLine::default();
        for _ in 0..MAX_CONSOLE_LINE_BYTES - 4 {
            assert!(line.insert('a'));
        }
        assert!(line.insert('🦀'));
        assert_eq!(line.text().len(), MAX_CONSOLE_LINE_BYTES);
        assert!(!line.insert('b'));
        line.left();
        assert!(line.delete());
        assert_eq!(line.text().len(), MAX_CONSOLE_LINE_BYTES - 4);
        assert!(line.insert('🦀'));
        assert_eq!(line.text().len(), MAX_CONSOLE_LINE_BYTES);
    }

    #[cfg(unix)]
    #[test]
    fn fixed_sibling_pairs_refresh_in_bytewise_order_without_mutating_output() {
        let parent = fixture("refresh");
        fs::create_dir(parent.join("test-cases")).unwrap();
        fs::write(parent.join("test-cases/zebra.in"), b"one").unwrap();
        fs::write(parent.join("test-cases/zebra.expected"), b"expected one").unwrap();
        fs::write(parent.join("test-cases/Alpha.in"), b"two").unwrap();
        fs::write(parent.join("test-cases/Alpha.expected"), b"expected two").unwrap();
        fs::write(parent.join("test-cases/unpaired.in"), b"ignored").unwrap();
        fs::write(parent.join("test-cases/output.txt"), b"preserve").unwrap();
        let cases = TestCaseDirectory::open(
            &parent.join("assignment.work"),
            TestCaseLayout::Paired,
            None,
        )
        .unwrap();
        assert_eq!(
            cases
                .list_cases()
                .unwrap()
                .iter()
                .map(TestCase::name)
                .collect::<Vec<_>>(),
            ["Alpha", "zebra"]
        );
        let zebra = cases.list_cases().unwrap().pop().unwrap();
        assert_eq!(cases.read_expected(&zebra).unwrap(), b"expected one");
        fs::write(parent.join("test-cases/zebra.expected"), b"refreshed").unwrap();
        assert_eq!(cases.read_expected(&zebra).unwrap(), b"refreshed");
        let output = WorkspacePath::new("output.txt").unwrap();
        assert_eq!(
            cases.output_disposition(&output).unwrap(),
            OutputDisposition::Overwrite
        );
        drop(
            cases
                .open_output(&output, OutputDisposition::Overwrite)
                .unwrap(),
        );
        assert_eq!(
            fs::read(parent.join("test-cases/output.txt")).unwrap(),
            b"preserve"
        );
        fs::remove_dir_all(parent).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn maximum_packaged_pair_count_is_listable() {
        let parent = fixture("maximum-pairs");
        fs::create_dir(parent.join("test-cases")).unwrap();
        for index in 0..MAX_TEST_CASES {
            fs::write(parent.join(format!("test-cases/case-{index:03}.in")), b"i").unwrap();
            fs::write(
                parent.join(format!("test-cases/case-{index:03}.expected")),
                b"o",
            )
            .unwrap();
        }
        fs::write(
            parent.join("test-cases/case-000.actual"),
            b"ordinary generated output",
        )
        .unwrap();
        let cases = TestCaseDirectory::open(
            &parent.join("assignment.work"),
            TestCaseLayout::Paired,
            None,
        )
        .unwrap()
        .list_cases()
        .unwrap();
        assert_eq!(cases.len(), MAX_TEST_CASES);
        assert_eq!(cases.first().unwrap().name(), "case-000");
        assert_eq!(cases.last().unwrap().name(), "case-255");
        fs::remove_dir_all(parent).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn refreshed_case_identity_changes_with_input_or_expected_bytes() {
        let parent = fixture("pair-identity");
        fs::create_dir(parent.join("test-cases")).unwrap();
        fs::write(parent.join("test-cases/sample.in"), b"input one").unwrap();
        fs::write(parent.join("test-cases/sample.expected"), b"output one").unwrap();
        let directory = TestCaseDirectory::open(
            &parent.join("assignment.work"),
            TestCaseLayout::Paired,
            None,
        )
        .unwrap();
        let original = directory.list_cases().unwrap().pop().unwrap();

        fs::write(parent.join("test-cases/sample.in"), b"input two").unwrap();
        let changed_input = directory.list_cases().unwrap().pop().unwrap();
        assert_ne!(changed_input, original);

        fs::write(parent.join("test-cases/sample.expected"), b"output two").unwrap();
        let changed_expected = directory.list_cases().unwrap().pop().unwrap();
        assert_ne!(changed_expected, changed_input);
        fs::remove_dir_all(parent).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn fixed_sibling_root_and_refreshed_entries_reject_symlinks() {
        let parent = fixture("symlink-root");
        fs::create_dir(parent.join("actual-cases")).unwrap();
        symlink("actual-cases", parent.join("test-cases")).unwrap();
        assert!(
            TestCaseDirectory::open(
                &parent.join("assignment.work"),
                TestCaseLayout::Paired,
                None
            )
            .is_err()
        );
        fs::remove_dir_all(&parent).unwrap();

        let parent = fixture("symlink-entry");
        fs::create_dir(parent.join("test-cases")).unwrap();
        fs::write(parent.join("outside"), b"outside").unwrap();
        symlink("../outside", parent.join("test-cases/unsafe")).unwrap();
        let cases = TestCaseDirectory::open(
            &parent.join("assignment.work"),
            TestCaseLayout::Paired,
            None,
        )
        .unwrap();
        assert!(cases.list_cases().unwrap().is_empty());
        assert!(
            cases
                .read_expected(&TestCase::new("unsafe").unwrap())
                .is_err()
        );
        fs::remove_dir_all(parent).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn selected_workspace_named_test_cases_cannot_become_external_file_authority() {
        let parent = fixture("same-root");
        fs::remove_dir(parent.join("assignment.work")).unwrap();
        fs::create_dir(parent.join("test-cases")).unwrap();
        fs::write(parent.join("test-cases/managed.rs"), b"preserve").unwrap();
        assert!(
            TestCaseDirectory::open(&parent.join("test-cases"), TestCaseLayout::Paired, None)
                .is_err()
        );
        assert_eq!(
            fs::read(parent.join("test-cases/managed.rs")).unwrap(),
            b"preserve"
        );
        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn case_pairing_accepts_only_the_packaged_name_grammar_and_complete_pairs() {
        let parent = fixture("pairing");
        fs::create_dir(parent.join("test-cases")).unwrap();
        for path in [
            "0.in",
            "0.expected",
            "a-b_C9.in",
            "a-b_C9.expected",
            "missing-input.expected",
            "missing-expected.in",
            ".in",
            ".expected",
            "dot.name.in",
            "dot.name.expected",
            "too-long-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.in",
            "too-long-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.expected",
            "notes.txt",
        ] {
            fs::write(parent.join("test-cases").join(path), path).unwrap();
        }
        fs::create_dir(parent.join("test-cases/nested")).unwrap();
        fs::write(parent.join("test-cases/nested/hidden.in"), b"ignored").unwrap();
        fs::write(parent.join("test-cases/nested/hidden.expected"), b"ignored").unwrap();

        let cases = TestCaseDirectory::open(
            &parent.join("assignment.work"),
            TestCaseLayout::Paired,
            None,
        )
        .unwrap();
        assert_eq!(
            cases
                .list_cases()
                .unwrap()
                .iter()
                .map(TestCase::name)
                .collect::<Vec<_>>(),
            ["0", "a-b_C9"]
        );
        fs::remove_dir_all(parent).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn format3_listing_accepts_optional_input_and_args_and_ignores_fixtures() {
        let parent = fixture("format3-listing");
        let cases_root = own_case_folder(&parent);
        for (path, bytes) in [
            ("usage.expected", &b"usage\n"[..]),
            ("count.args", b"-c\nfn main\n"),
            ("count.expected", b"2\n"),
            ("stdin.in", b"alpha\n"),
            ("stdin.expected", b"alpha\n"),
            ("broken.args", b"no final newline"),
            ("broken.expected", b""),
            ("orphan.args", b"x\n"),
            ("orphan.in", b"x\n"),
            ("notes.txt", b"ignored"),
        ] {
            fs::write(cases_root.join(path), bytes).unwrap();
        }
        fs::create_dir_all(cases_root.join("files/nested")).unwrap();
        fs::write(cases_root.join("files/hidden.expected"), b"fixture").unwrap();
        fs::write(cases_root.join("files/hidden.in"), b"fixture").unwrap();
        fs::write(cases_root.join("files/nested/deep.expected"), b"fixture").unwrap();

        let directory = TestCaseDirectory::open(
            &parent.join("assignment.work"),
            TestCaseLayout::Extended,
            None,
        )
        .unwrap();
        let cases = directory.list_cases_for(TestCaseLayout::Extended).unwrap();
        let summary = cases
            .iter()
            .map(|case| {
                (
                    case.name(),
                    case.has_input(),
                    case.args().map(<[String]>::to_vec),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            summary,
            [
                ("broken", false, None),
                (
                    "count",
                    false,
                    Some(vec!["-c".to_owned(), "fn main".to_owned()])
                ),
                ("stdin", true, Some(Vec::new())),
                ("usage", false, Some(Vec::new())),
            ]
        );
        assert!(
            directory
                .read_args(&cases[0])
                .unwrap_err()
                .to_string()
                .contains("must end every argument line with LF")
        );

        // Format 2 listing is unchanged: complete pairs only, no `.args`.
        assert_eq!(
            directory
                .list_cases_for(TestCaseLayout::Paired)
                .unwrap()
                .iter()
                .map(TestCase::name)
                .collect::<Vec<_>>(),
            ["stdin"]
        );
        for (format_version, layout) in [
            (1, TestCaseLayout::Paired),
            (2, TestCaseLayout::Paired),
            (3, TestCaseLayout::Extended),
        ] {
            assert_eq!(TestCaseLayout::for_format_version(format_version), layout);
        }

        // Argument, input, and expected changes all refresh the identity.
        let identity = |directory: &TestCaseDirectory| {
            directory
                .list_cases_for(TestCaseLayout::Extended)
                .unwrap()
                .into_iter()
                .find(|case| case.name() == "count")
                .unwrap()
        };
        let original = identity(&directory);
        fs::write(cases_root.join("count.args"), b"-c\nfn  main\n").unwrap();
        let changed_args = identity(&directory);
        assert_ne!(changed_args, original);
        fs::write(cases_root.join("count.in"), b"").unwrap();
        let added_input = identity(&directory);
        assert_ne!(added_input, changed_args);
        assert!(added_input.has_input());
        fs::remove_dir_all(parent).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn format3_input_is_read_into_memory_and_hashed_within_its_limit() {
        let parent = fixture("input-hash");
        let cases_root = own_case_folder(&parent);
        fs::write(cases_root.join("case.in"), b"alpha\nbeta\n").unwrap();
        fs::write(cases_root.join("case.expected"), b"").unwrap();
        fs::write(
            cases_root.join("large.in"),
            vec![b'x'; MAX_TEST_CASE_FILE_BYTES as usize + 1],
        )
        .unwrap();
        fs::write(cases_root.join("large.expected"), b"").unwrap();
        let directory = TestCaseDirectory::open(
            &parent.join("assignment.work"),
            TestCaseLayout::Extended,
            None,
        )
        .unwrap();
        let cases = directory.list_cases_for(TestCaseLayout::Extended).unwrap();

        let (read, blake3) = directory.read_input_with_blake3(&cases[0]).unwrap();
        assert_eq!(blake3, hash_bytes(b"alpha\nbeta\n"));
        assert_eq!(read, b"alpha\nbeta\n");
        assert!(
            directory
                .read_input_with_blake3(&cases[1])
                .unwrap_err()
                .to_string()
                .contains("1048576-byte limit")
        );
        fs::remove_dir_all(parent).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn fixture_tree_check_reads_the_deployed_tree_once_or_reports_its_absence() {
        let parent = fixture("fixture-hash");
        let cases_root = own_case_folder(&parent);
        fs::write(cases_root.join("case.expected"), b"").unwrap();
        let directory = TestCaseDirectory::open(
            &parent.join("assignment.work"),
            TestCaseLayout::Extended,
            None,
        )
        .unwrap();
        assert_eq!(directory.check_fixture_tree(None).unwrap().1, None);
        assert_eq!(directory.fixtures_display(), "assignment.test-cases/files");

        fs::create_dir_all(cases_root.join("files/src")).unwrap();
        fs::write(cases_root.join("files/src/lib.rs"), b"fn a() {}\n").unwrap();
        let expected = FixtureTree::from_parts(
            [],
            [(
                WorkspacePath::new("src/lib.rs").unwrap(),
                b"fn a() {}\n".to_vec(),
            )],
        )
        .unwrap()
        .hash();
        let (check, tree) = directory.check_fixture_tree(Some(expected)).unwrap();
        assert_eq!(
            check,
            FixtureTreeCheck::Matches {
                fixtures_blake3: expected
            }
        );
        let tree = tree.unwrap();
        assert_eq!(tree.hash(), expected);
        assert_eq!(
            tree.files()
                .map(|(path, _)| path.as_str())
                .collect::<Vec<_>>(),
            ["src/lib.rs"]
        );
        assert_eq!(
            directory.check_fixture_tree(None).unwrap().0,
            FixtureTreeCheck::NotPackaged { deployed: true }
        );
        fs::write(cases_root.join("files/src/lib.rs"), b"edited\n").unwrap();
        let (check, tree) = directory.check_fixture_tree(Some(expected)).unwrap();
        let edited = tree.map(|tree| tree.hash());
        assert_ne!(edited, Some(expected));
        assert_eq!(
            check,
            FixtureTreeCheck::Differs {
                packaged: expected,
                deployed: edited
            }
        );
        fs::remove_dir_all(cases_root.join("files")).unwrap();
        assert_eq!(
            directory.check_fixture_tree(Some(expected)).unwrap(),
            (
                FixtureTreeCheck::Differs {
                    packaged: expected,
                    deployed: None
                },
                None
            )
        );
        assert_eq!(
            directory.check_fixture_tree(None).unwrap().0,
            FixtureTreeCheck::NotPackaged { deployed: false }
        );
        fs::create_dir_all(cases_root.join("files/src")).unwrap();

        symlink("../../outside", cases_root.join("files/src/escape")).unwrap();
        assert!(directory.check_fixture_tree(Some(expected)).is_err());
        fs::remove_dir_all(parent).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn pinned_fixtures_hash_the_deployed_tree_and_warn_when_it_differs() {
        let parent = fixture("pin-fixtures");
        let cases_root = own_case_folder(&parent);
        fs::write(cases_root.join("case.expected"), b"").unwrap();
        let directory = TestCaseDirectory::open(
            &parent.join("assignment.work"),
            TestCaseLayout::Extended,
            None,
        )
        .unwrap();
        let packaged = FixtureTree::from_parts(
            [],
            [(WorkspacePath::new("data.txt").unwrap(), b"data\n".to_vec())],
        )
        .unwrap()
        .hash();
        let missing = directory.pin_fixtures(packaged).unwrap_err().to_string();
        assert_eq!(
            missing,
            "the fixture folder assignment.test-cases/files is missing; quit and resume the workspace to deploy it again"
        );

        fs::create_dir(cases_root.join("files")).unwrap();
        fs::write(cases_root.join("files/data.txt"), b"data\n").unwrap();
        let pinned = directory.pin_fixtures(packaged).unwrap();
        assert_eq!(
            pinned.check,
            FixtureTreeCheck::Matches {
                fixtures_blake3: packaged
            }
        );
        assert_eq!(pinned.deployed(), packaged);
        assert_eq!(pinned.warning(true), None);
        assert_eq!(pinned.display, "assignment.test-cases/files");
        assert_eq!(pinned.root.path(), cases_root.join("files"));

        fs::write(cases_root.join("files/extra.txt"), b"added\n").unwrap();
        let changed = directory.pin_fixtures(packaged).unwrap();
        assert_ne!(changed.deployed(), packaged);
        assert_eq!(
            changed.check,
            FixtureTreeCheck::Differs {
                packaged,
                deployed: Some(changed.deployed())
            }
        );
        let warning = changed.warning(true).unwrap();
        assert_eq!(
            warning,
            "warning: the files in assignment.test-cases/files differ from the assignment package; the case runs with them as they are and will not verify against the package. To restore them, remove the files you changed or added, then quit and resume the workspace"
        );
        assert!(
            changed
                .warning(false)
                .unwrap()
                .contains("; your program runs with them as they are. To restore")
        );
        for word in ["failed", "error", "rejected", "unavailable", "stopped"] {
            assert!(!warning.to_ascii_lowercase().contains(word), "{word}");
        }

        fs::create_dir(cases_root.join("files/.cargo")).unwrap();
        assert!(
            directory
                .pin_fixtures(packaged)
                .unwrap_err()
                .to_string()
                .contains("files/.cargo")
        );
        fs::remove_dir(cases_root.join("files/.cargo")).unwrap();
        symlink("../outside", cases_root.join("files/link")).unwrap();
        assert!(directory.pin_fixtures(packaged).is_err());
        fs::remove_dir_all(parent).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn extended_case_files_are_read_fresh_from_the_case_folder() {
        let parent = fixture("extended-case-files");
        let cases_root = own_case_folder(&parent);
        fs::write(cases_root.join("plain.expected"), b"").unwrap();
        fs::write(cases_root.join("full.expected"), b"").unwrap();
        fs::write(cases_root.join("full.args"), b"-n\nfn main\n").unwrap();
        fs::write(cases_root.join("full.in"), b"input\n").unwrap();
        let directory = TestCaseDirectory::open(
            &parent.join("assignment.work"),
            TestCaseLayout::Extended,
            None,
        )
        .unwrap();
        // Defaults such as TestCase::new's are never trusted for format 3.
        let plain = directory
            .read_extended_case(&TestCase::new("plain").unwrap())
            .unwrap();
        assert!(plain.args.is_empty());
        assert!(plain.input.is_none());
        let full = directory
            .read_extended_case(&TestCase::new("full").unwrap())
            .unwrap();
        assert_eq!(full.args, ["-n", "fn main"]);
        assert_eq!(
            full.input,
            Some((b"input\n".to_vec(), hash_bytes(b"input\n")))
        );
        fs::write(cases_root.join("full.args"), b"no final newline").unwrap();
        assert!(
            directory
                .read_extended_case(&TestCase::new("full").unwrap())
                .unwrap_err()
                .to_string()
                .contains("test-case arguments")
        );
        fs::remove_file(cases_root.join("full.args")).unwrap();
        symlink("plain.expected", cases_root.join("full.args")).unwrap();
        assert!(
            directory
                .read_extended_case(&TestCase::new("full").unwrap())
                .is_err()
        );
        fs::remove_dir_all(parent).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn cargo_configuration_in_the_case_folder_or_fixtures_is_refused() {
        let parent = fixture("cargo-configuration");
        let cases_root = own_case_folder(&parent);
        fs::create_dir(cases_root.join("files")).unwrap();
        let directory = TestCaseDirectory::open(
            &parent.join("assignment.work"),
            TestCaseLayout::Extended,
            None,
        )
        .unwrap();
        directory.reject_cargo_configuration().unwrap();
        for (path, directory_entry) in [
            (".cargo", true),
            ("files/.cargo", true),
            (".cargo", false),
            ("files/.cargo", false),
        ] {
            if directory_entry {
                fs::create_dir(cases_root.join(path)).unwrap();
            } else {
                fs::write(cases_root.join(path), b"").unwrap();
            }
            let error = directory.reject_cargo_configuration().unwrap_err();
            assert!(error.to_string().contains(path), "{error}");
            if directory_entry {
                fs::remove_dir(cases_root.join(path)).unwrap();
            } else {
                fs::remove_file(cases_root.join(path)).unwrap();
            }
        }
        symlink("../elsewhere", cases_root.join(".cargo")).unwrap();
        assert!(directory.reject_cargo_configuration().is_err());
        fs::remove_file(cases_root.join(".cargo")).unwrap();
        fs::remove_dir(cases_root.join("files")).unwrap();
        fs::write(cases_root.join("files"), b"").unwrap();
        directory.reject_cargo_configuration().unwrap();
        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn format3_case_folder_is_named_after_the_workspace() {
        let folder = |name: &str| {
            TestCaseLayout::Extended
                .folder_name(std::ffi::OsStr::new(name))
                .map_err(|error| error.to_string())
        };
        assert_eq!(folder("lab2.work").unwrap(), "lab2.test-cases");
        assert_eq!(folder("Lab 2.work").unwrap(), "Lab 2.test-cases");
        assert_eq!(folder("lab2").unwrap(), "lab2.test-cases");
        assert_eq!(folder("lab2.work.work").unwrap(), "lab2.work.test-cases");
        assert_eq!(folder("lab2.WORK").unwrap(), "lab2.WORK.test-cases");
        for (name, reason) in [
            ("test-cases", "end in `.test-cases`"),
            ("Test-Cases", "end in `.test-cases`"),
            ("lab2.test-cases", "end in `.test-cases`"),
            ("lab2.TEST-CASES", "end in `.test-cases`"),
            (".work", "needs a name before `.work`"),
            ("lab{2}.work", "`{` or `}`"),
            ("lab}.work", "`{` or `}`"),
        ] {
            let error = folder(name).unwrap_err();
            assert!(error.contains(reason), "{name}: {error}");
        }
        assert!(folder(&format!("{}.work", "x".repeat(245))).is_err());
        assert_eq!(
            folder(&format!("{}.work", "x".repeat(244))).unwrap().len(),
            255
        );

        // Formats 1 and 2 keep the one fixed folder.
        for name in ["lab1.work", "lab2.test-cases", "{x}"] {
            assert_eq!(
                TestCaseLayout::Paired
                    .folder_name(std::ffi::OsStr::new(name))
                    .unwrap(),
                "test-cases"
            );
        }
        assert!(
            TestCaseLayout::Paired
                .folder_name(std::ffi::OsStr::new("test-cases"))
                .is_err()
        );
        assert_eq!(
            test_case_folder(Path::new("/course/lab2.work"), TestCaseLayout::Extended).unwrap(),
            Path::new("/course/lab2.test-cases")
        );
        assert_eq!(
            test_case_folder(Path::new("/course/lab1.work"), TestCaseLayout::Paired).unwrap(),
            Path::new("/course/test-cases")
        );
    }

    #[cfg(unix)]
    #[test]
    fn format2_and_format3_sessions_open_different_case_folders() {
        let parent = fixture("folder-by-format");
        fs::create_dir(parent.join("test-cases")).unwrap();
        fs::write(parent.join("test-cases/shared.in"), b"").unwrap();
        fs::write(parent.join("test-cases/shared.expected"), b"").unwrap();
        let workspace = parent.join("assignment.work");
        assert!(
            TestCaseDirectory::open(&workspace, TestCaseLayout::Extended, None).is_err(),
            "a format 3 session never falls back to the shared folder"
        );
        own_case_folder(&parent);
        fs::write(parent.join("assignment.test-cases/own.expected"), b"").unwrap();
        let names = |layout| {
            TestCaseDirectory::open(&workspace, layout, Some(SUITE))
                .unwrap()
                .list_cases_for(layout)
                .unwrap()
                .iter()
                .map(|case| case.name().to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(names(TestCaseLayout::Paired), ["shared"]);
        assert_eq!(names(TestCaseLayout::Extended), ["own"]);
        fs::remove_dir_all(parent).unwrap();
    }

    const SUITE: Hash = Hash::from_bytes([7; Hash::LENGTH]);

    /// `assignment.test-cases`, owned by `assignment.work` for [`SUITE`].
    fn own_case_folder(parent: &Path) -> std::path::PathBuf {
        let folder = parent.join("assignment.test-cases");
        fs::create_dir(&folder).unwrap();
        fs::write(
            folder.join(CASE_FOLDER_MARKER),
            CaseFolderMarker::new("assignment.work", SUITE).encode(),
        )
        .unwrap();
        folder
    }

    #[cfg(unix)]
    #[test]
    fn format3_case_folders_must_belong_to_their_workspace_and_suite() {
        let parent = fixture("case-folder-owner");
        let folder = own_case_folder(&parent);
        fs::write(folder.join("case.expected"), b"").unwrap();
        let workspace = parent.join("assignment.work");
        let open = |suite| TestCaseDirectory::open(&workspace, TestCaseLayout::Extended, suite);
        assert_eq!(
            fs::read_to_string(folder.join(CASE_FOLDER_MARKER)).unwrap(),
            format!(
                "{{\"version\":1,\"workspace\":\"assignment.work\",\"test_case_suite_hash\":\"{}\"}}\n",
                "07".repeat(32)
            )
        );
        let directory = open(Some(SUITE)).unwrap();
        let names = directory
            .list_cases_for(TestCaseLayout::Extended)
            .unwrap()
            .iter()
            .map(|case| case.name().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(names, ["case"], "the marker is never a case");
        open(None).unwrap();
        let error = open(Some(Hash::from_bytes([8; Hash::LENGTH]))).unwrap_err();
        assert!(
            error.to_string().contains("different assignment package"),
            "{error}"
        );

        // `assignment` would map to the same folder; its marker names another.
        fs::create_dir(parent.join("assignment")).unwrap();
        let error = TestCaseDirectory::open(
            &parent.join("assignment"),
            TestCaseLayout::Extended,
            Some(SUITE),
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("belongs to the workspace `assignment.work`"),
            "{error}"
        );

        fs::remove_file(folder.join(CASE_FOLDER_MARKER)).unwrap();
        assert!(open(Some(SUITE)).is_err(), "missing marker");
        for (label, bytes) in [
            ("malformed", &b"{"[..]),
            (
                "noncanonical",
                br#"{ "version":1,"workspace":"assignment.work","test_case_suite_hash":"0707070707070707070707070707070707070707070707070707070707070707"}
"#,
            ),
        ] {
            fs::write(folder.join(CASE_FOLDER_MARKER), bytes).unwrap();
            assert!(open(Some(SUITE)).is_err(), "{label}");
        }

        // A folder that is itself a workspace is never a case folder, in any
        // format.
        fs::write(
            folder.join(CASE_FOLDER_MARKER),
            CaseFolderMarker::new("assignment.work", SUITE).encode(),
        )
        .unwrap();
        fs::create_dir(folder.join(".rustrace")).unwrap();
        let error = open(Some(SUITE)).unwrap_err();
        assert!(
            error.to_string().contains("is a Rustrace workspace"),
            "{error}"
        );
        fs::create_dir_all(parent.join("test-cases/.rustrace")).unwrap();
        assert!(TestCaseDirectory::open(&workspace, TestCaseLayout::Paired, None).is_err());
        fs::remove_dir_all(parent).unwrap();
    }

    fn mismatch(comparison: &TestCaseComparison) -> &TestCaseMismatch {
        let TestCaseOutcome::Fail(mismatch) = &comparison.outcome else {
            panic!("expected mismatch, got {:?}", comparison.outcome);
        };
        mismatch
    }

    #[test]
    fn exact_byte_comparison_table_reports_the_first_lf_delimited_mismatch() {
        let case = TestCase::new("33").unwrap();
        for (expected, actual) in [
            (&b""[..], &b""[..]),
            (&b"same"[..], &b"same"[..]),
            (&b"same\n"[..], &b"same\n"[..]),
            (&b"\xff\x00\n"[..], &b"\xff\x00\n"[..]),
        ] {
            assert_eq!(
                compare_test_case_bytes(case.clone(), expected, actual).outcome,
                TestCaseOutcome::Pass
            );
        }

        type MismatchCase<'a> = (&'a [u8], &'a [u8], u64, usize, usize);
        let table: &[MismatchCase<'_>] = &[
            (b"same\n", b"same", 1, 4, 4),
            (b"one\n", b"one\ntwo\n", 2, 0, 3),
            (b"abc\n", b"abX\n", 1, 3, 3),
            (b"line\r\n", b"line\n", 1, 5, 4),
            (b"\xff\n", b"\xfe\n", 1, 1, 1),
            (b"one\n", b"one\nextra", 2, 0, 5),
            (b"one\nmissing", b"one\n", 2, 7, 0),
            (b"first\nsecond\nthird", b"first\nSECOND\nthird", 2, 6, 6),
        ];
        for (expected, actual, line, expected_len, actual_len) in table {
            let comparison = compare_test_case_bytes(case.clone(), expected, actual);
            let mismatch = mismatch(&comparison);
            assert_eq!(
                mismatch.line, *line,
                "expected={expected:?} actual={actual:?}"
            );
            assert_eq!(mismatch.expected_len, *expected_len);
            assert_eq!(mismatch.actual_len, *actual_len);
            assert_eq!(comparison.expected_blake3, Some(hash_bytes(expected)));
            assert_eq!(comparison.actual_blake3, Some(hash_bytes(actual)));
        }
    }

    #[test]
    fn comparison_previews_are_shared_safe_display_output_and_strictly_bounded() {
        let case = TestCase::new("unsafe").unwrap();
        let mut expected = vec![b'a'; MAX_TEST_CASE_LINE_PREVIEW_INPUT_BYTES * 2];
        expected[0] = 0x1b;
        expected[1] = 0xff;
        let actual = vec![b'b'; MAX_TEST_CASE_LINE_PREVIEW_INPUT_BYTES * 2];
        let comparison = compare_test_case_bytes(case, &expected, &actual);
        let mismatch = mismatch(&comparison);

        assert_eq!(mismatch.expected_len, expected.len());
        assert_eq!(mismatch.actual_len, actual.len());
        assert!(mismatch.expected_preview.contains("\\u{1b}\\xff"));
        assert!(
            mismatch
                .expected_preview
                .contains(crate::display::TRUNCATED)
        );
        assert!(mismatch.actual_preview.contains(crate::display::TRUNCATED));
        assert!(mismatch.expected_preview.len() <= MAX_TEST_CASE_LINE_PREVIEW_OUTPUT_BYTES);
        assert!(mismatch.actual_preview.len() <= MAX_TEST_CASE_LINE_PREVIEW_OUTPUT_BYTES);
        assert!(!mismatch.expected_preview.as_bytes().contains(&0x1b));
    }

    #[test]
    fn finished_case_classifies_every_non_comparable_condition_as_error() {
        let case = TestCase::new("errors").unwrap();
        let successful = CommandOutcome::Exited { code: 0 };
        let errors = [
            (
                CommandOutcome::LaunchFailed { os_code: Some(2) },
                CaptureCompleteness::Complete,
                Ok(b"expected".to_vec()),
                "launch failed",
            ),
            (
                CommandOutcome::Exited { code: 9 },
                CaptureCompleteness::Complete,
                Ok(b"expected".to_vec()),
                "exit 9",
            ),
            (
                CommandOutcome::Terminated {
                    reason: CommandTermination::Deadline,
                    signal: None,
                },
                CaptureCompleteness::Complete,
                Ok(b"expected".to_vec()),
                "deadline",
            ),
            (
                successful.clone(),
                CaptureCompleteness::Truncated,
                Ok(b"expected".to_vec()),
                "truncated",
            ),
            (
                successful.clone(),
                CaptureCompleteness::Unavailable,
                Ok(b"expected".to_vec()),
                "unavailable",
            ),
            (
                successful.clone(),
                CaptureCompleteness::ReadFailed,
                Ok(b"expected".to_vec()),
                "read failed",
            ),
            (
                successful,
                CaptureCompleteness::Complete,
                Err("arbitrary filesystem detail must not escape".to_owned()),
                "expected output is unreadable",
            ),
            (
                CommandOutcome::Exited { code: 0 },
                CaptureCompleteness::Complete,
                Err("expected output exceeds the 1048576-byte limit".to_owned()),
                "expected output is oversized",
            ),
        ];

        for (outcome, completeness, expected, reason) in errors {
            let comparison = classify_test_case_result(
                case.clone(),
                &outcome,
                completeness,
                b"actual",
                expected,
            );
            let TestCaseOutcome::Error(actual) = &comparison.outcome else {
                panic!("{reason} was not classified as an error: {comparison:?}");
            };
            assert!(
                actual.contains(reason),
                "{actual:?} does not contain {reason:?}"
            );
            if completeness == CaptureCompleteness::Unavailable {
                assert_eq!(comparison.actual_blake3, None);
            }
        }

        for reason in [
            CommandTermination::Cancelled,
            CommandTermination::Quit,
            CommandTermination::OutputLimit,
            CommandTermination::CaptureFailure,
            CommandTermination::Signal,
            CommandTermination::CleanupFailure,
        ] {
            let comparison = classify_test_case_result(
                case.clone(),
                &CommandOutcome::Terminated {
                    reason,
                    signal: Some(9),
                },
                CaptureCompleteness::Complete,
                b"actual",
                Ok(b"expected".to_vec()),
            );
            assert!(matches!(comparison.outcome, TestCaseOutcome::Error(_)));
        }
    }

    #[test]
    fn oversized_expected_output_is_unreadable_for_comparison() {
        let parent = fixture("oversized-expected");
        fs::create_dir(parent.join("test-cases")).unwrap();
        fs::write(parent.join("test-cases/large.in"), b"").unwrap();
        fs::write(
            parent.join("test-cases/large.expected"),
            vec![b'x'; MAX_TEST_CASE_FILE_BYTES as usize + 1],
        )
        .unwrap();
        let cases = TestCaseDirectory::open(
            &parent.join("assignment.work"),
            TestCaseLayout::Paired,
            None,
        )
        .unwrap();
        let case = cases.list_cases().unwrap().pop().unwrap();
        assert!(
            cases
                .read_expected(&case)
                .unwrap_err()
                .to_string()
                .contains("1048576-byte limit")
        );
        fs::remove_dir_all(parent).unwrap();
    }
}
