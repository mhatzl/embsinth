use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use defmt_decoder::{DecodeError, Frame, Location};
use defmt_parser::Level;
use probe_rs::Session;

use crate::connection::RttActiveUpChannel;

/// Like [`defmt_decoder::Frame`] but without lifetimes.
#[derive(Debug)]
pub struct DefmtFrame {
    pub index: u64,
    pub level: Option<defmt_parser::Level>,
    pub message: String,
    pub timestamp: Option<String>,
    pub location: Option<defmt_decoder::Location>,
}

impl From<(Frame<'_>, Option<Location>)> for DefmtFrame {
    fn from((frame, location): (Frame<'_>, Option<Location>)) -> Self {
        Self {
            index: frame.index(),
            level: frame.level(),
            location,
            message: frame.display_message().to_string(),
            timestamp: frame
                .display_timestamp()
                .map(|timestamp| timestamp.to_string()),
        }
    }
}

pub(crate) fn spawn_defmt_thread(
    main_thread: std::thread::Thread,
    binary_bytes: Vec<u8>,
    shared: &Arc<DefmtThreadCommunication>,
    msg_tx: std::sync::mpsc::Sender<DefmtFrame>,
    done_tx: std::sync::mpsc::SyncSender<()>,
    upchannel: RttActiveUpChannel,
) {
    let shared = shared.clone();
    std::thread::spawn(move || {
        read_defmt_msgs(
            main_thread,
            binary_bytes,
            shared,
            msg_tx,
            done_tx,
            upchannel,
        )
    });
}

pub(crate) struct DefmtThreadCommunication {
    pub(crate) session: Mutex<Session>,
    pub(crate) hard_faulted: AtomicBool,
    pub(crate) stop_defmt_tx: AtomicBool,
}

impl DefmtThreadCommunication {
    pub(crate) fn new(session: Session) -> Self {
        Self {
            session: Mutex::new(session),
            hard_faulted: AtomicBool::new(false),
            stop_defmt_tx: AtomicBool::new(false),
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("Defmt thread failed")]
pub struct DefmtThreadError {
    kind: DefmtThreadErrorKind,
    source: Option<anyhow::Error>,
}

impl DefmtThreadError {
    /// The kind of error raised in the defmt background thread.
    pub fn kind(&self) -> DefmtThreadErrorKind {
        self.kind
    }

    fn new(kind: DefmtThreadErrorKind) -> Self {
        Self { kind, source: None }
    }

    fn parsing_table(source: impl Into<anyhow::Error>) -> Self {
        Self {
            kind: DefmtThreadErrorKind::ParsingTable,
            source: Some(source.into()),
        }
    }

    fn missing_locations(source: impl Into<anyhow::Error>) -> Self {
        Self {
            kind: DefmtThreadErrorKind::MissingLocations,
            source: Some(source.into()),
        }
    }
}

/// Error kinds that may be raised in the defmt background thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DefmtThreadErrorKind {
    #[error("Failed decoding the defmt data")]
    ParsingTable,
    #[error("No defmt data found in given binary file")]
    MissingDefmtData,
    #[error("Wrong encoding of defmt data. Encoding must be rzcobs")]
    WrongEncoding,
    #[error("Missing defmt location info")]
    MissingLocations,
}

fn read_defmt_msgs(
    main_thread: std::thread::Thread,
    binary_bytes: Vec<u8>,
    shared: Arc<DefmtThreadCommunication>,
    frame_tx: std::sync::mpsc::Sender<DefmtFrame>,
    done_tx: std::sync::mpsc::SyncSender<()>,
    mut upchannel: RttActiveUpChannel,
) -> Result<(), DefmtThreadError> {
    let table = defmt_decoder::Table::parse(&binary_bytes)
        .map_err(DefmtThreadError::parsing_table)?
        .ok_or(DefmtThreadError::new(
            DefmtThreadErrorKind::MissingDefmtData,
        ))?;

    if defmt_decoder::Encoding::Rzcobs != table.encoding() {
        return Err(DefmtThreadError::new(DefmtThreadErrorKind::WrongEncoding));
    }

    let locs = table
        .get_locations(&binary_bytes)
        .map_err(DefmtThreadError::missing_locations)?;

    let DefmtThreadCommunication {
        ref session,
        ref hard_faulted,
        ref stop_defmt_tx,
    } = *shared;

    let mut decoder = table.new_stream_decoder();

    main_thread.unpark(); // everything ready to receive defmt messages => continue main thread

    let mut exiting = false;
    loop {
        {
            let mut session = session.lock().unwrap();
            let mut core = session.core(0).expect("could not select core 0");
            if crate::connection::has_hard_faulted(&mut core) {
                hard_faulted.store(true, Ordering::Relaxed);
                // Avoid mutex poisoning by dropping the MutexGuard before panicking.
                drop(core);
                drop(session);
                panic!("Core has hard faulted.");
            }

            // match fatal_error_timestamp {
            //     Some(fatal_error_timestamp) => {
            //         // Give the RTT channel time to flush before panicking so that we can read out the
            //         // panic message.
            //         if (Instant::now() - fatal_error_timestamp) > Duration::from_secs(1) {
            //             fatal_error.store(true, Ordering::Relaxed);
            //             // Avoid mutex poisoning by dropping the MutexGuard before panicking.
            //             drop(core);
            //             drop(session);
            //             panic!("Core encountered fatal error");
            //         }
            //     }
            //     None => {
            //         if has_fatal_error(&mut core) {
            //             fatal_error_timestamp = Some(Instant::now());
            //         }
            //     }
            // }

            if stop_defmt_tx.load(Ordering::Relaxed) && !exiting {
                log::warn!("Probe dropped. Halting device to read remaining logs.");
                core.halt(Duration::from_secs(1))
                    .expect("could not halt device");
                exiting = true;
            }

            upchannel
                .poll(&mut core)
                .expect("Failed to read new defmt data");
        }

        let buffered_data = upchannel.buffered_data();

        if buffered_data.is_empty() {
            if exiting {
                // device halted so there'll be no more new data; exit
                break;
            } else {
                std::thread::sleep(Duration::from_millis(1));
            }
        } else {
            decoder.received(buffered_data);

            loop {
                match decoder.decode() {
                    Ok(frame) => {
                        let location = locs.get(&frame.index()).cloned();
                        let defmt_msg: DefmtFrame = (frame, location).into();

                        log_defmt_msg(&defmt_msg);

                        frame_tx
                            .send(defmt_msg)
                            .expect("unreachable: given synchronization with ProbeRs' destructor");
                    }
                    Err(DecodeError::UnexpectedEof) => break,
                    Err(DecodeError::Malformed) if table.encoding().can_recover() => {
                        // If recovery is possible, skip the current frame and continue with new data.
                    }
                    Err(DecodeError::Malformed) => {
                        panic!(
                            "Unrecoverable error while decoding Defmt data. Some data may have been lost!"
                        );
                    }
                }
            }
        }
    }

    // drop the shared data (including the probe-rs session) before notifying the main thread.
    drop(shared);

    // inform the foreground thread that the probe-rs session has been destroyed
    done_tx.send(()).ok();

    Ok(())
}

pub(crate) fn log_defmt_msg(msg: &DefmtFrame) {
    let (module, file, line) = msg.location.as_ref().map_or((None, None, None), |loc| {
        (Some(loc.module.as_str()), loc.file.to_str(), Some(loc.line))
    });

    // see: https://github.com/rust-lang/rust/pull/140748
    // & https://github.com/rust-lang/rust/issues/92698#issuecomment-1142146879
    #[allow(clippy::redundant_closure_call)]
    (|frame: &DefmtFrame, args: std::fmt::Arguments<'_>| {
        let log_record = log::Record::builder()
            .level(
                frame
                    .level
                    .map(|lvl| match lvl {
                        Level::Trace => log::Level::Trace,
                        Level::Debug => log::Level::Debug,
                        Level::Info => log::Level::Info,
                        Level::Warn => log::Level::Warn,
                        Level::Error => log::Level::Error,
                    })
                    .unwrap_or(log::Level::Info),
            )
            .args(args)
            .module_path(module)
            .file(file)
            .line(line.map(|l| {
                l.try_into()
                    .expect("Line number must be convertable to u32")
            }))
            .target(module.unwrap_or("target"))
            .build();
        log::logger().log(&log_record);
    })(msg, format_args!("{}", msg.message));
}
