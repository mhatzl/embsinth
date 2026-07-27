use std::{
    collections::HashMap,
    ops::Deref,
    path::{Path, PathBuf},
    sync::{Mutex, Once},
    time::Duration,
};

use probe_rs::{
    Permissions, Session,
    flashing::{self, ElfOptions, FlashProgress},
    probe::DebugProbeInfo,
};

use crate::connection::Connection;

const PROBE_RS_TIMEOUT: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub struct ProbeId {
    vid: u16,
    pid: u16,
}

impl ProbeId {
    pub fn new(vid: u16, pid: u16) -> Self {
        Self { vid, pid }
    }

    pub fn attach_under_reset(self, chip: impl Into<String>) -> Result<AttachedProbe, ProbeError> {
        AttachedProbe::attach_under_reset(self, chip)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProbeState {
    Opened,
    Free,
}

pub(crate) static PROBE_STATES: std::sync::LazyLock<Mutex<HashMap<ProbeId, ProbeState>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

const MAX_PROBE_RETRIES: usize = 5;

pub struct AttachedProbe {
    pub(crate) id: ProbeId,
    pub(crate) chip: String,
    pub(crate) session: Session,
}

impl AttachedProbe {
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
            if probe.vendor_id == probe_id.vid && probe.product_id == probe_id.pid {
                assert!(
                    found.is_none(),
                    "found more than one probe with matching vendor and product ID"
                );

                found = Some(probe);
            }
        }

        let Some(probe) = found else {
            return Err(ProbeError {
                kind: ProbeErrorKind::MissingProbe,
            });
        };

        let probe_aquired = {
            let mut probe_states = PROBE_STATES
                .lock()
                .expect("Probe state map has been poisoned in some thread");

            match probe_states.get_mut(&probe_id) {
                Some(state) => {
                    if state == &ProbeState::Free {
                        *state = ProbeState::Opened;
                        true
                    } else {
                        false
                    }
                }
                None => {
                    probe_states.insert(probe_id, ProbeState::Opened);
                    true
                }
            }
        };

        if !probe_aquired {
            return Err(ProbeError {
                kind: ProbeErrorKind::ProbeTaken,
            });
        }

        let chip = chip.into();
        for _ in 1..=MAX_PROBE_RETRIES {
            match probe.open() {
                Ok(probe) => {
                    let session = probe
                        .attach_under_reset(&chip, Permissions::default())
                        .expect("could not attach");
                    return Ok(Self {
                        id: probe_id,
                        chip,
                        session,
                    });
                }
                Err(err) => {
                    log::warn!("Couldn't open probe - retrying. Cause: {err}");
                    continue;
                }
            }
        }

        Err(ProbeError {
            kind: ProbeErrorKind::FailedOpening,
        })
    }

    pub fn flash_once_and_connect(
        mut self,
        binary_file: PathBuf,
    ) -> Result<Connection, ProbeError> {
        static FLASHED: Once = Once::new();

        if !binary_file.exists() {
            return Err(ProbeError {
                kind: ProbeErrorKind::NonExistingBinaryFile,
            });
        }

        halt_core(&mut self.session);

        FLASHED.call_once(|| {
            flash_binary(&mut self.session, &binary_file).expect("Flashing the app must succeed");
        });

        enable_vector_catch(&mut self.session);

        Ok(Connection::new(self, binary_file))
    }

    pub fn flash_and_connect(mut self, binary_file: PathBuf) -> Result<Connection, ProbeError> {
        if !binary_file.exists() {
            return Err(ProbeError {
                kind: ProbeErrorKind::NonExistingBinaryFile,
            });
        }

        halt_core(&mut self.session);
        flash_binary(&mut self.session, &binary_file)?;
        enable_vector_catch(&mut self.session);

        Ok(Connection::new(self, binary_file))
    }
}

#[derive(Debug, Clone)]
pub struct ProbeError {
    kind: ProbeErrorKind,
}

#[derive(Debug, Clone)]
pub enum ProbeErrorKind {
    MissingProbe,
    FailedOpening,
    ProbeTaken,
    NonExistingBinaryFile,
    /// Failed erasing all non-volatile memory
    ErasingNonVolatileMemory,
    DownloadingFile,
}

fn flash_binary(session: &mut Session, binary_file: &Path) -> Result<(), ProbeError> {
    if let Err(err) = flashing::erase_all(session, &mut FlashProgress::empty(), false) {
        return Err(ProbeError {
            kind: ProbeErrorKind::ErasingNonVolatileMemory,
        });
    }

    if let Err(err) = flashing::download_file(
        session,
        binary_file,
        flashing::ElfLoader(ElfOptions::default()),
    ) {
        return Err(ProbeError {
            kind: ProbeErrorKind::DownloadingFile,
        });
    }

    Ok(())
}

fn halt_core(session: &mut Session) {
    let mut core = session.core(0).expect("could not select core 0");
    core.halt(PROBE_RS_TIMEOUT)
        .expect("timeout while halting core");
}

fn enable_vector_catch(session: &mut Session) {
    let mut core = session.core(0).expect("could not select core 0");
    core.enable_vector_catch(probe_rs::VectorCatchCondition::HardFault)
        .expect("could not install vector catch condition");
}
