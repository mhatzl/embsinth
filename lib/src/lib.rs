//! The embedded systems and integration test harness `embsinth` provides functionality that makes embedded testing easier.
//!
//! The crate is split into a library and CLI part, where the library crate provides functionality per test case
//! and the CLI tool may be used for post processing of test case results.
//!
//! Use `cargo install --locked embsinth` to install `embsinth` as CLI tool.
//!
//! When used as library, wrap each test function intended as system or integration test with the [`embsinth::test`](crate::test) macro.
//! Such tests must then be executed on the host and each test function may connect to embedded targets depending on the test.
//! The macro will capture all logs set via [log] on the host side and [defmt] logs set on the connected targets
//! using [`env_logger`]. Use `RUST_LOG` to restrict the logs that should be printed to stdout.
//! All captured logs are stored per test function in newline-separated JSON `*.jsonl` files
//! in the `logs` subfolder inside the directory set via environmental variable `EMBSINTH_OUT_DIR`.
//! Those log files are later used for post processing via CLI.
//!
//! Inside test functions, convenience wrapper around [probe-rs](https://crates.io/crates/probe-rs) may be used
//! to attach and flash binaries to embedded targets, and to read the `defmt` logs set by the attached targets.
//! The [`ProbeId`](crate::probe::ProbeId) is used as base to identify which debug probe should be [attached](crate::probe::ProbeId::attach_under_reset) to.
//! Once attached, an [`AttachedProbe`](crate::probe::AttachedProbe) may be used to [flash](crate::probe::AttachedProbe::flash_and_connect) and connect
//! to an embedded target. This returns a [`Connection`](crate::connection::Connection) that may be used to read and filter for [defmt](https://defmt.ferrous-systems.com) messages.
//!
//! **Note:** Since system and integration tests typically require connected hardware,
//! tests typically must be run in sequence. For `cargo test`, this is already handled by `embsinth`
//! via multi-threaded test guards.
//! For [`cargo nextest`](https://nexte.st), the argument `-j=1` must be set to limit the number of concurrent test executions,
//! because each test function is run in it's own process.

pub mod connection;
pub mod defmt;
pub mod logger;
pub mod mantra;
pub mod probe;

use std::path::PathBuf;

pub use embsinth_procm::*;
use mantra_schema::time::OffsetDateTime;

/// CLI commands for embsinth.
#[derive(Debug, Clone, clap::Parser)]
pub enum Cmd {
    /// Post process the captured test logs and convert them to the [mantra](https://github.com/mhatzl/mantra) TestRun schema.
    PostProcess(PostProcess),
}

impl Cmd {
    /// Execute the embsinth command.
    ///
    /// For post processing, logs captured during test executions are converted to the [mantra](https://github.com/mhatzl/mantra) TestRun schema.
    /// The resulting JSON file may then be passed to mantra for requirements-based test coverage.
    pub fn run(self) -> Result<(), anyhow::Error> {
        match self {
            Cmd::PostProcess(args) => {
                let test_run_schema = crate::mantra::process(
                    &args.logs_dir,
                    args.test_run_name,
                    args.test_run_date
                        .unwrap_or(mantra_schema::time::OffsetDateTime::now_utc()),
                )?;

                let content = serde_json::to_string_pretty(&test_run_schema)?;
                std::fs::write(args.out, content)?;
            }
        }

        Ok(())
    }
}

/// Post process arguments
#[derive(Debug, Clone, clap::Args)]
pub struct PostProcess {
    /// The path to search for captured test logs.
    /// embsinth looks for `*.jsonl` files.
    pub logs_dir: PathBuf,
    /// The output path the converted [mantra](https://github.com/mhatzl/mantra) TestRun will be written to.
    #[clap(long)]
    pub out: PathBuf,
    /// The name of the resulting test run.
    #[clap(long)]
    pub test_run_name: String,
    /// The test run date and time. Will be set to the current timestamp if not set.
    /// Date and time must be given following [`mantra_schema::time::format_description::well_known::iso8601`].
    #[clap(long, value_parser = mantra_schema::test_runs::test_date_from_str)]
    pub test_run_date: Option<OffsetDateTime>,
}
