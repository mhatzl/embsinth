use std::{
    ops::Deref,
    path::{Path, PathBuf},
    sync::Once,
    time::Duration,
};

use probe_rs::{
    Permissions, Session,
    flashing::{self, ElfOptions, FlashProgress},
    probe::DebugProbeInfo,
};

use crate::connection::Connection;

const PROBE_RS_TIMEOUT: Duration = Duration::from_millis(100);

/// The ID of a debug probe.
///
/// Use `probe-rs list` to get the IDs of probes connected to your computer.
///
/// **Note:** The returned values from probe-rs are hexadecimal.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct ProbeId {
    vid: u16,
    pid: u16,
    ser_nr: Option<String>,
}

impl ProbeId {
    /// Create a new probe ID using vendor and probe ID.
    pub fn new(vid: u16, pid: u16) -> Self {
        Self {
            vid,
            pid,
            ser_nr: None,
        }
    }

    /// Create a new probe ID using vendor and probe ID and the serial number of the probe.
    ///
    /// This is needed if several targets have the same built-in debug probe.
    pub fn with_serial_nr(vid: u16, pid: u16, ser_nr: impl Into<String>) -> Self {
        Self {
            vid,
            pid,
            ser_nr: Some(ser_nr.into()),
        }
    }

    /// Attaches to the probe identified by the ID.
    pub fn attach_under_reset(self, chip: impl Into<String>) -> Result<AttachedProbe, ProbeError> {
        AttachedProbe::attach_under_reset(self, chip)
    }
}

const MAX_PROBE_RETRIES: usize = 5;

/// An attached probe that may be used to flash and connect to an embedded target.
pub struct AttachedProbe {
    pub(crate) id: ProbeId,
    pub(crate) chip: String,
    pub(crate) session: Session,
}

impl AttachedProbe {
    /// Attaches a probe to an embedded target that uses the given chip.
    pub fn attach_under_reset(
        probe_id: ProbeId,
        chip: impl Into<String>,
    ) -> Result<Self, ProbeError> {
        // DAP Link implementation. shows up as "NXP ARM mbed" in `lsusb`
        static FOUND_PROBES: std::sync::LazyLock<Vec<DebugProbeInfo>> =
            std::sync::LazyLock::new(|| {
                let lister = probe_rs::probe::list::Lister::new();
                lister.list_all()
            });

        let mut found = None;
        for probe in FOUND_PROBES.deref() {
            if probe.vendor_id == probe_id.vid
                && probe.product_id == probe_id.pid
                && let Some(ser_nr) = &probe_id.ser_nr
                && Some(ser_nr) == probe.serial_number.as_ref()
            {
                assert!(
                    found.is_none(),
                    "found more than one probe with matching vendor ID, product ID and optional serial number"
                );

                found = Some(probe);
            }
        }

        let Some(probe) = found else {
            return Err(ProbeError {
                kind: ProbeErrorKind::MissingProbe,
                source: None,
            });
        };

        log::info!("Attaching to: {}", probe);

        let chip = chip.into();
        for _ in 1..=MAX_PROBE_RETRIES {
            match probe.open() {
                Ok(mut probe) => {
                    let _ = probe.set_speed(1_000); // 1 MHz

                    let mut session = probe
                        .attach(&chip, Permissions::default().allow_erase_all())
                        .expect("could not attach probe");

                    {
                        reset_and_halt(&mut session);
                    }

                    return Ok(Self {
                        id: probe_id,
                        chip,
                        session,
                    });
                }
                Err(err) => {
                    log::warn!("Couldn't open probe - retrying. Cause: {err}");
                    std::thread::sleep(PROBE_RS_TIMEOUT);
                    continue;
                }
            }
        }

        Err(ProbeError {
            kind: ProbeErrorKind::FailedOpening,
            source: None,
        })
    }

    /// Flashes the given binary the first time this function is called,
    /// and connects to the target.
    ///
    /// **Note:** Flashing is only done once independent of the given binary file.
    pub fn flash_once_and_connect(
        mut self,
        binary_file: impl Into<PathBuf>,
    ) -> Result<Connection, ProbeError> {
        static FLASHED: Once = Once::new();

        let binary_file = binary_file.into();

        if !binary_file.exists() {
            return Err(ProbeError {
                kind: ProbeErrorKind::NonExistingBinaryFile,
                source: None,
            });
        }

        FLASHED.call_once(|| {
            log::info!("Flashing once file '{}'", binary_file.display());
            flash_binary(&mut self.session, &binary_file).expect("Flashing the app must succeed");
        });

        enable_vector_catch(&mut self.session);

        Ok(Connection::new(self, binary_file))
    }

    /// Flashes the given binary and connects to the target.
    ///
    /// **Note:** Since this will flash every time, it may wear out embedded devices.
    pub fn flash_and_connect(
        mut self,
        binary_file: impl Into<PathBuf>,
    ) -> Result<Connection, ProbeError> {
        let binary_file = binary_file.into();

        if !binary_file.exists() {
            return Err(ProbeError {
                kind: ProbeErrorKind::NonExistingBinaryFile,
                source: None,
            });
        }

        log::info!("Flashing file '{}'", binary_file.display());
        flash_binary(&mut self.session, &binary_file)?;
        enable_vector_catch(&mut self.session);

        Ok(Connection::new(self, binary_file))
    }
}

#[derive(Debug, thiserror::Error)]
#[error("Probe error")]
pub struct ProbeError {
    kind: ProbeErrorKind,
    source: Option<anyhow::Error>,
}

impl ProbeError {
    /// The kind that caused the error.
    pub fn kind(&self) -> ProbeErrorKind {
        self.kind
    }
}

/// The errors that may be raised when attaching or flashing to an embedded target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ProbeErrorKind {
    #[error("Missing probe")]
    MissingProbe,
    #[error("Failed opening probe")]
    FailedOpening,
    #[error("Probe already taken")]
    ProbeTaken,
    #[error("Filepath does not point to an existing file")]
    NonExistingBinaryFile,
    #[error("Failed erasing all non-volatile memory")]
    ErasingNonVolatileMemory,
    #[error("Failed to download the binary file to the target")]
    DownloadingFile,
}

fn flash_binary(session: &mut Session, binary_file: &Path) -> Result<(), ProbeError> {
    if let Err(err) = flashing::erase_all(session, &mut FlashProgress::empty(), false) {
        return Err(ProbeError {
            kind: ProbeErrorKind::ErasingNonVolatileMemory,
            source: Some(err.into()),
        });
    }

    if let Err(err) = flashing::download_file(
        session,
        binary_file,
        flashing::ElfLoader(ElfOptions::default()),
    ) {
        return Err(ProbeError {
            kind: ProbeErrorKind::DownloadingFile,
            source: Some(err.into()),
        });
    }

    Ok(())
}

fn reset_and_halt(session: &mut Session) {
    let mut core = session.core(0).expect("could not select core 0");
    core.reset_and_halt(PROBE_RS_TIMEOUT)
        .expect("timeout while halting core");
}

fn enable_vector_catch(session: &mut Session) {
    let mut core = session.core(0).expect("could not select core 0");
    core.enable_vector_catch(probe_rs::VectorCatchCondition::HardFault)
        .expect("could not install vector catch condition");
}
