use std::{
    fs::OpenOptions,
    io::Write,
    path::PathBuf,
    sync::{
        LazyLock, Mutex, Once,
        atomic::{self, AtomicBool},
    },
    time::Duration,
};

use relative_path::RelativePathBuf;

/// File extension for newline separated JSON log files for test cases.
const TEST_CASE_LOGFILE_EXTENSION: &str = "jsonl";

/// Holds the absolute path to the logs directory for a crate.
/// e.g. <EMBSINTH_OUT_DIR>/<CARGO_PKG_NAME>/logs/
///
/// It uses the `CARGO_PKG_NAME` as crate name and `EMBSINTH_OUT_DIR` to get the path.
/// This directory will be automatically created if it doesn't exist.
pub static LOG_OUTPUT_BASE_PATH: std::sync::LazyLock<PathBuf> = std::sync::LazyLock::new(|| {
    let base_path =
        PathBuf::from(std::env::var("EMBSINTH_OUT_DIR").expect("EMBSINTH_OUT_DIR must be set"))
            .join(std::env::var("CARGO_PKG_NAME").expect("CARGO_PKG_NAME set by cargo"))
            .join("logs");
    // Note: silent fail on error, because logfile creations would fail if this fails
    let _ = std::fs::create_dir_all(&base_path);
    base_path
});

static ENV_LOGGER: LazyLock<env_logger::Logger> = LazyLock::new(|| env_logger::builder().build());

/// Thread local static used to store the currently active test case per thread.
static CURR_TEST_CASE_NAME: LazyLock<Mutex<Option<&'static str>>> =
    LazyLock::new(|| Mutex::new(None));
/// Thread local static used to store the currently active test case per thread.
static CURR_EXPECTED_PANIC_MSG: LazyLock<Mutex<Option<ExpectedPanicMsg>>> =
    LazyLock::new(|| Mutex::new(None));

/// Defines the matching behavior for tests marked with `#[should_panic]`.
#[derive(Debug, Clone, Copy)]
pub enum ExpectedPanicMsg {
    /// Matches on the string literal given in `#[should_panic(expected = "<the expected panic msg>")]`.
    Exact(&'static str),
    /// Matches on any panic message.
    Any,
}

/// Defines the panic handling of a test case.
pub enum PanicHandling {
    /// Informs `traced-test` that the current test case should panic with the given [`ExpectedPanicMsg`].
    ShouldPanic(ExpectedPanicMsg),
    /// Informs `traced-test` that the current test case must fail if a panic is raised.
    /// This is the default behavior if a test case is **not** marked with `#[should_panic]`.
    FailOnPanic,
}

static TEST_CASE_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Initializes a logger and accompanying logfile per running test case to capture mantra traces.
pub fn test_case_start(
    test_case_name: &'static str,
    filepath: &'static str,
    line: u32,
    panic_handling: PanicHandling,
) {
    while !TEST_CASE_ACTIVE.swap(true, atomic::Ordering::Relaxed) {
        std::thread::sleep(Duration::from_secs(5));
        // panic!(
        //     "Embedded integration tests must be run in sequence! Limit tests to only one thread"
        // );
    }

    static ONCE: Once = Once::new();

    let test_case_logpath = get_test_case_logpath(test_case_name);
    std::fs::write(
        &test_case_logpath,
        format!(
            "{}\n",
            LogEntry::from(TestCaseStart::new(
                test_case_name.to_string(),
                PathBuf::from(filepath),
                line,
            ))
            .to_jsonl()
        ),
    )
    .unwrap_or_else(|_| {
        panic!(
            "Failed to write test-case logs to file: {}",
            test_case_logpath.display()
        )
    });

    *CURR_TEST_CASE_NAME.lock().unwrap() = Some(test_case_name);
    match panic_handling {
        PanicHandling::ShouldPanic(expected_panic) => {
            *CURR_EXPECTED_PANIC_MSG.lock().unwrap() = Some(expected_panic);
        }
        PanicHandling::FailOnPanic => *CURR_EXPECTED_PANIC_MSG.lock().unwrap() = None,
    }

    ONCE.call_once(|| {
        log::set_logger(&TestCaseLogger).unwrap();
        log::set_max_level(log::LevelFilter::Trace);

        let curr_hook = std::panic::take_hook();
        std::panic::set_hook(std::boxed::Box::new(move |info| {
            let curr_expected_panic_msg = CURR_EXPECTED_PANIC_MSG.lock().unwrap();
            let should_panic =
                curr_expected_panic_msg.is_some_and(|expected_panic| match expected_panic {
                    ExpectedPanicMsg::Any => true,
                    ExpectedPanicMsg::Exact(expected_msg) => {
                        // as of edition 2021, the panic payload is either `String` or `&str`
                        Some(&expected_msg.to_string()) == info.payload().downcast_ref::<String>()
                            || Some(&expected_msg) == info.payload().downcast_ref::<&str>()
                    }
                });

            if should_panic {
                test_case_end();
            }

            if should_panic || curr_expected_panic_msg.is_none() {
                // Note: Calling the original panic handler is needed to pass tests marked with `should_panic`,
                // and it preserves the panic behavior for regular tests.
                curr_hook(info);
            } else {
                eprintln!("{info}");

                if let Some(ExpectedPanicMsg::Exact(expected)) = *curr_expected_panic_msg {
                    eprintln!("Expected Panic: {expected}");
                }
            }
        }));
    });
}

/// Marks the test case end in the related logfile.
pub fn test_case_end() {
    let test_case_name = CURR_TEST_CASE_NAME
        .lock()
        .unwrap()
        .expect("Test case name must be set at the start of a test case");

    let test_case_logpath = get_test_case_logpath(test_case_name);
    let mut opened_file = OpenOptions::new()
        .append(true)
        .open(&test_case_logpath)
        .unwrap_or_else(|_| panic!("Couldn't open logfile: {}", test_case_logpath.display()));
    writeln!(
        opened_file,
        "{}",
        LogEntry::from(TestCaseEnd {
            state: TestState::Passed
        })
        .to_jsonl()
    )
    .expect("Couldn't append log entries to logfile.");

    *CURR_TEST_CASE_NAME.lock().unwrap() = None;

    TEST_CASE_ACTIVE.store(false, atomic::Ordering::Relaxed);
}

/// Returns the absolute path to the logfile of a test case.
fn get_test_case_logpath(test_case_name: &str) -> PathBuf {
    // replacing needed, because `:` is invalid as part of a filename at least on macOS and Windows
    let mut test_case_logpath = LOG_OUTPUT_BASE_PATH.join(test_case_name.replace(":", "-"));
    test_case_logpath.set_extension(TEST_CASE_LOGFILE_EXTENSION);
    test_case_logpath
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub enum LogEntry {
    TestCaseStart(TestCaseStart),
    TestCaseEnd(TestCaseEnd),
    LogFrame(LogFrame),
}

impl LogEntry {
    fn to_jsonl(&self) -> String {
        serde_json::to_string(self)
            .expect("Serializing derived log entry must work")
            .replace("\n", " ")
    }
}

impl From<LogFrame> for LogEntry {
    fn from(value: LogFrame) -> Self {
        Self::LogFrame(value)
    }
}

impl From<TestCaseStart> for LogEntry {
    fn from(value: TestCaseStart) -> Self {
        Self::TestCaseStart(value)
    }
}

impl From<TestCaseEnd> for LogEntry {
    fn from(value: TestCaseEnd) -> Self {
        Self::TestCaseEnd(value)
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct LogFrame {
    pub level: log::Level,
    pub message: String,
    pub file: Option<RelativePathBuf>,
    pub line: Option<u32>,
}

/// Represents a function that is marked with `#[traced_test]`.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct TestCaseStart {
    /// Fully qualified name of the function.
    pub name: String,
    /// File the test case is defined in.
    pub filepath: PathBuf,
    /// Line in the file the test case is defined at.
    pub line: u32,
}

impl TestCaseStart {
    fn new(name: String, filepath: PathBuf, line: u32) -> Self {
        Self {
            name,
            filepath,
            line,
        }
    }
}

/// Marks the end of a test case in a test case logfile.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct TestCaseEnd {
    /// The final state of the test case.
    pub state: TestState,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub enum TestState {
    Passed,
    Failed,
}

struct TestCaseLogger;

impl log::Log for TestCaseLogger {
    fn enabled(&self, _metadata: &log::Metadata) -> bool {
        // Note: `set_max_level()` in the `init()` already filters logs so we don't need to double check.
        // We also don't check `enabled()` in the `log()` fn, because we assume it always returns `true`.
        true
    }

    fn log(&self, record: &log::Record) {
        let Some(test_case_name) = *CURR_TEST_CASE_NAME.lock().unwrap() else {
            ENV_LOGGER.log(record);
            return;
        };
        let test_case_logpath = get_test_case_logpath(test_case_name);
        let log_content = format!("{}", record.args());

        let mantra_msg = log_content.contains("mantra coverage");

        let log_frame = LogFrame {
            level: record.level(),
            message: log_content,
            file: record.file().map(RelativePathBuf::from),
            line: record.line(),
        };

        let mut opened_file = OpenOptions::new()
            .append(true)
            .open(&test_case_logpath)
            .unwrap_or_else(|_| panic!("Couldn't open logfile: {}", test_case_logpath.display()));
        writeln!(
            opened_file,
            "{}",
            serde_json::to_string(&log_frame)
                .expect("Log frame is serializable")
                .replace("\n", " ")
        )
        .expect("Couldn't append requirements trace to logfile.");

        if !mantra_msg {
            ENV_LOGGER.log(record);
        }
    }

    fn flush(&self) {}
}
