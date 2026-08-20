use std::{collections::HashMap, path::Path, str::FromStr};

use anyhow::{Context, anyhow, bail};
use ignore::Walk;
use mantra_macros::coverage::LineCoverage;
use mantra_schema::{
    test_runs::{
        CoveredFile, CoveredLine, TestCase, TestCaseLocation, TestCaseState, TestRun, TestRunSchema,
    },
    time::OffsetDateTime,
};
use relative_path::RelativePathBuf;

use crate::logger::LogEntry;

/// Process all `*.jsonl` files found in the given directory and convert them into a mantra test run.
/// This assumes that all `*.jsonl` files contain log entries set by `embsinth`.
pub fn process(
    dir: &Path,
    test_run_name: impl Into<String>,
    test_run_date: OffsetDateTime,
) -> Result<TestRunSchema, anyhow::Error> {
    let mut test_run = TestRun {
        name: test_run_name.into(),
        utc_date: test_run_date,
        description: None,
        revisions: None,
        origin: None,
        nr_of_test_cases: 0,
        properties: None,
        duration_sec: None,
        logs: None,
        test_cases: vec![],
        covered_files: vec![],
        test_runs: vec![],
    };

    let mut walker = Walk::new(dir);
    while let Some(Ok(entry)) = walker.next() {
        let path = entry.path();
        if path.is_file()
            && path
                .extension()
                .map(|e| e.to_string_lossy() == crate::logger::TEST_CASE_LOGFILE_EXTENSION)
                .unwrap_or(false)
        {
            let test_case =
                convert_file_content(path).with_context(|| format!("File: {}", path.display()))?;

            test_run.test_cases.push(test_case);
            test_run.nr_of_test_cases += 1;
        }
    }

    Ok(TestRunSchema {
        schema_version: Some(mantra_schema::SCHEMA_VERSION.to_owned()),
        test_runs: vec![test_run],
        test_run_properties: None,
        test_case_properties: None,
        origin: None,
    })
}

fn convert_file_content(filepath: &Path) -> Result<TestCase, anyhow::Error> {
    let content = std::fs::read_to_string(filepath)?;
    let logs = crate::mantra::content_to_logs(&content)
        .context("Failed converting file content to log entries")?;
    let test_case = crate::mantra::logs_to_mantra_test_case(filepath, &logs)?;
    Ok(test_case)
}

fn content_to_logs(content: &str) -> Result<Vec<LogEntry>, anyhow::Error> {
    let mut logs: Vec<LogEntry> = Vec::new();

    for line in content.lines().filter(|l| !l.trim().is_empty()) {
        let entry = serde_json::from_str(line).with_context(|| format!("Bad line: {line}"))?;
        logs.push(entry);
    }

    Ok(logs)
}

fn logs_to_mantra_test_case(
    filepath: &Path,
    logs: &[LogEntry],
) -> Result<mantra_schema::test_runs::TestCase, anyhow::Error> {
    if logs.is_empty() {
        bail!("At least a test case start entry must be present in the logs");
    }

    let mut logs_iter = logs.iter().peekable();

    let LogEntry::TestCaseStart(test_case_start) = logs_iter
        .next()
        .expect("Ensured above that at least one entry is in the list")
    else {
        bail!("First log entry must be a test case start")
    };

    let mut test_case = TestCase {
        name: test_case_start.name.clone(),
        description: None,
        state: TestCaseState::Failed,
        state_properties: None,
        location: Some(TestCaseLocation {
            filepath: mantra_schema::path::RelativePathBuf::from_path(&test_case_start.filepath)
                .map_err(|_err| {
                    anyhow!(
                        "Test case filepath is not relative: {}",
                        test_case_start.filepath.display()
                    )
                })?,
            file_hash: None,
            line: test_case_start.line.into(),
        }),
        utc_date: None,
        duration_sec: None,
        properties: None,
        logs: None,
        verified_reqs: vec![],
        covered_files: vec![],
    };

    let mut covered_files: HashMap<RelativePathBuf, HashMap<u32, CoveredLine>> = HashMap::new();

    while let Some(LogEntry::LogFrame(frame)) = logs_iter.peek() {
        let _ = logs_iter.next();

        if LineCoverage::potential_log(&frame.message) {
            let coverage = LineCoverage::from_str(&frame.message).map_err(|err| {
                anyhow!("Failed to extract mantra coverage from log.").context(err)
            })?;

            covered_files
                .entry(coverage.file().into())
                .and_modify(|lines| {
                    lines
                        .entry(coverage.line())
                        .and_modify(|line| {
                            *line
                                .hits
                                .as_mut()
                                .expect("All entries start with a hit of 1") += 1
                        })
                        .or_insert(CoveredLine {
                            nr: coverage.line().into(),
                            hits: Some(1),
                        });
                })
                .or_insert(HashMap::from_iter([(
                    coverage.line(),
                    CoveredLine {
                        nr: coverage.line().into(),
                        hits: Some(1),
                    },
                )]));
        }
    }

    if let Some(LogEntry::TestCaseEnd(test_case_end)) = logs_iter.next() {
        test_case.state = test_case_end.state;
    } else {
        test_case.state = TestCaseState::Failed;
    }

    if let Some(LogEntry::LogFrame(_frame)) = logs_iter.next() {
        eprintln!(
            "Ignoring log entry after test case ended in file: {}",
            filepath.display()
        );
    }

    for file in covered_files {
        test_case.covered_files.push(CoveredFile {
            filepath: file.0,
            file_hash: None,
            lines: file.1.into_values().collect(),
        });
    }

    Ok(test_case)
}
