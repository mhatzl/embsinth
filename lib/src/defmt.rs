use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use defmt_decoder::{Frame, Location};
use defmt_parser::Level;
use probe_rs::Session;

/// Like `defmt_decoder::Frame` but without lifetimes
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
    binary_file: PathBuf,
    shared: &Arc<DefmtThreadCommunication>,
    msg_tx: std::sync::mpsc::Sender<DefmtFrame>,
    done_tx: std::sync::mpsc::SyncSender<()>,
) {
    let shared = shared.clone();
    std::thread::spawn(move || read_defmt_msgs(binary_file, shared, msg_tx, done_tx));
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
    fn new(kind: DefmtThreadErrorKind) -> Self {
        Self { kind, source: None }
    }

    fn reading_binary(source: impl Into<anyhow::Error>) -> Self {
        Self {
            kind: DefmtThreadErrorKind::ReadingBinaryFile,
            source: Some(source.into()),
        }
    }

    fn parsing_table(source: impl Into<anyhow::Error>) -> Self {
        Self {
            kind: DefmtThreadErrorKind::ParsingTable,
            source: Some(source.into()),
        }
    }

    fn missing_defmt(source: impl Into<anyhow::Error>) -> Self {
        Self {
            kind: DefmtThreadErrorKind::MissingDefmtData,
            source: Some(source.into()),
        }
    }

    fn wrong_encoding(source: impl Into<anyhow::Error>) -> Self {
        Self {
            kind: DefmtThreadErrorKind::WrongEncoding,
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

#[derive(Debug, thiserror::Error)]
pub enum DefmtThreadErrorKind {
    #[error("Failed reading the binary file")]
    ReadingBinaryFile,
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
    binary_file: PathBuf,
    shared: Arc<DefmtThreadCommunication>,
    frame_tx: std::sync::mpsc::Sender<DefmtFrame>,
    done_tx: std::sync::mpsc::SyncSender<()>,
) -> Result<(), DefmtThreadError> {
    let binary_bytes = std::fs::read(&binary_file).map_err(DefmtThreadError::reading_binary)?;
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

    const RX_BUFSZ: usize = 1_024;

    let mut buf = [0; RX_BUFSZ];
    let mut upchannel = {
        let mut session = session.lock().unwrap();
        crate::connection::rtt_upchannel(&mut session, &binary_bytes)
    };
    let mut upchannel_buffer: Vec<u8> = Vec::new();

    // let mut fatal_error_timestamp = None;

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
        }

        // extract all the defmt frames (0-terminated) present in the buffer
        while let Some(delimiter) = upchannel_buffer.iter().position(|&x| x == 0) {
            let encoded = &upchannel_buffer[..delimiter];

            let decode_res = rzcobs::decode(encoded);

            // discard encoded frame
            upchannel_buffer.drain(0..delimiter + 1);

            let Ok(decoded) = decode_res else {
                // try next frame
                continue;
            };

            let (frame, _consumed) = table.decode(&decoded).expect("defmt decode error");

            let location = locs.get(&frame.index()).cloned();
            let defmt_msg: DefmtFrame = (frame, location).into();

            frame_tx
                .send(defmt_msg)
                .expect("unreachable: given synchronization with ProbeRs' destructor");
        }

        // no more frames available so (try to) pull more data out of the device
        {
            let mut session = session.lock().unwrap();
            let mut core = session.core(0).expect("could not select core 0");
            let read = upchannel
                .read(&mut core, &mut buf)
                .expect("error reading RTT upchannel");

            if read == 0 {
                if exiting {
                    // device halted so there'll be no more new data; exit
                    break;
                } else {
                    std::thread::sleep(Duration::from_millis(1));
                }
            } else {
                let mut new_data = &buf[..read];
                if upchannel_buffer.is_empty() {
                    while new_data.first() == Some(&0) {
                        new_data = &new_data[1..];
                    }
                }

                upchannel_buffer.extend_from_slice(new_data);
            }
        }
    }

    // drop the shared data (including the ProbeRs session) before notifying the main thread.
    drop(shared);

    // inform the foreground thread that the probe-rs Session has been destroyed
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
                    .unwrap_or(log::Level::Trace),
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
