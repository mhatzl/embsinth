# embsinth

Embedded system and integration test harness for Rust projects

## Test Setup

When testing embedded devices in an automated setup, it is common that I/O must be controlled externally via additional devices.
Such an external *simulation* device may be another embedded target that is used to simulate the environment by setting I/O in an expected sequence.

With `embsinth`, the approach is to write regular Rust tests that are executed on the host platform
and control what embedded targets are needed and which simulation sequence is flashed per test.
Log messages set by the embedded targets may then be used for synchronization and verification.

## Usage

Create tests using the `#[embedded::test]` macro and flash and connect to embedded targets as shown in the example of [embsinth's README](lib/README.md).
Logs captured during test execution and test results are then automatically stored per test under the `logs` subfolder of the directory set via `EMBSINTH_OUT_DIR`.

After all tests have been executed, the captured logs may then be post-processsed using `embsinth` as CLI tool.
This will combine all test results into the [mantra test run schema](https://docs.rs/mantra-schema/0.8.0/mantra_schema/test_runs/struct.TestRunSchema.html)
that combines tests and code coverage into one format.
If `embsinth` is used together with [mantra](https://github.com/mhatzl/mantra) to collect requirements coverage,
activate the `defmt` feature for [`mantra-macros`](https://crates.io/crates/mantra-macros) if used in the target application
and the `log` feature if used on the host e.g. via assertions.

**For post-processing the captured test logs, run:**

```
embsinth post-process --out <test run filepath>.json --test-run-name <name of the test run> $EMBSINTH_OUT_DIR/
```

## Limitations

Connections are currently limited to only allow reading of defmt messages.
For more complex test sequences, it may be needed to set values on a target directly,
which is possible via probe-rs, but currently not provided as convenience wrapper.
Access the probe-rs [`Session`](https://docs.rs/probe-rs/0.32.0/probe_rs/struct.Session.html) directly via the `session()` function of the `Connection` struct.

Flashing to a simulation device for each test may significantly reduce the lifetime of an embedded device.
Creating a dedicated simulation application that is able to switch between test sequences should be used instead
to only require flashing once for the first test.
This requires some way to write to the simulation device e.g. using the probe-rs `Session`.

# License

Apache-2.0

# Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you shall be licensed as above, without any additional terms or conditions.

