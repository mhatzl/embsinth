# embsinth

Embedded system and integration test harness for Rust projects.

This crate provides convenience wrapper around [probe-rs](https://crates.io/crates/probe-rs)
to flash and read [defmt](https://defmt.ferrous-systems.com) messages.

For testing, set the attribute macro `#[embsinth::test]` on functions to create a test that captures all logs set during test execution.
The destination the captured logs are stored to must be set by the environmental variable `EMBSINTH_OUT_DIR`.

**A typical test may look like:**

```rust
#[embsinth::test]
fn system_test() {
    // Attaches to the probe of the application device that will be tested
    let app_probe = embsinth::probe::ProbeId::with_serial_nr(0x1366, 0x1051, "001050272949")
        .attach_under_reset("nRF52840_xxAA")
        .expect("Failed to attach to rad target")
    // Attaches to the probe of the simulation device used to control I/O
    let sim_probe = embsinth::probe::ProbeId::new(0x1366, 0x1052)
        .attach_under_reset("nRF52840_xxAA")
        .expect("Failed to attach to rad target")

    // Flashes the binary once per execution of a binary and connects to the target
    let app_connection = app_probe.flash_once_and_connect("<APP binary filepath>")
        .expect("Failed to flash application binary");
    // Flashes the binary that is used as simulation sequence for this test and connects to the target
    let sim_connection = sim_probe.flash_and_connect("<simulation sequence binary filepath>")
        .expect("Failed to flash simulation binary");

    let timeout = std::time::Duration::from_secs(10);
    assert!(app_connection
            .search_msg_for(timeout, |msg| {
                msg.message.starts_with("<Some log indicating the app has started>")
            })
            .is_some(),
        "Application did not start as expected"
    );

    // some more checks...

    assert!(sim_connection
            .search_msg_for(timeout, |msg| {
                msg.message
                    .starts_with("Simulation flow finished")
            })
            .is_some(),
        "Simulation flow did not succeed"
    );
}
```

**Note:** The probe and connection setup code may be moved to a common test setup module
that is accessible for all test functions to reduce code duplication.

# License

Apache-2.0

# Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you shall be licensed as above, without any additional terms or conditions.
