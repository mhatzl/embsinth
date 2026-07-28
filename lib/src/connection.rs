use std::{
    path::PathBuf,
    sync::{
        Arc, MutexGuard,
        atomic::Ordering,
        mpsc::{Receiver, RecvTimeoutError},
    },
    time::{Duration, Instant},
};

use object::{Object, ObjectSymbol};
use probe_rs::{Session, rtt::UpChannel};

use crate::{
    defmt::{DefmtFrame, DefmtThreadCommunication, log_defmt_msg},
    probe::{AttachedProbe, ProbeId, ProbeState},
};

pub struct Connection {
    probe_id: ProbeId,
    chip: String,
    shared: Arc<DefmtThreadCommunication>,
    msg_rx: Receiver<DefmtFrame>,
    done_rx: Receiver<()>,
    panic_on_disconnected_error: bool,
}

impl Drop for Connection {
    fn drop(&mut self) {
        // tell background thread to stop ...
        self.shared.stop_defmt_tx.store(true, Ordering::Relaxed);
        // then wait until it has stopped
        // this returns immediately if the background thread panicked so we won't deadlock
        _ = self.done_rx.recv();
        // finally pass all the defmt logs in the `frame_rx` channel to the logger
        // bg thread has been terminated by us so don't panic after we emptied the channel
        self.panic_on_disconnected_error = false;
        self.flush_defmt_msgs(false);
        // Free the selected probe so it may be reused for new connections
        // The probe session is
        crate::probe::PROBE_STATES
            .lock()
            .expect("Probe state map has been poisoned in some thread")
            .entry(self.probe_id.clone())
            .and_modify(|s| *s = ProbeState::Free);
    }
}

impl Connection {
    pub(crate) fn new(probe: AttachedProbe, binary_file: PathBuf) -> Self {
        let (msg_tx, msg_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
        let shared = std::sync::Arc::new(DefmtThreadCommunication::new(probe.session));

        crate::defmt::spawn_defmt_thread(binary_file, &shared, msg_tx, done_tx);

        Self {
            probe_id: probe.id,
            chip: probe.chip,
            shared,
            msg_rx,
            done_rx,
            panic_on_disconnected_error: true,
        }
    }

    pub fn close(self) {
        drop(self)
    }

    pub fn probe_id(&self) -> &ProbeId {
        &self.probe_id
    }

    pub fn chip(&self) -> &str {
        &self.chip
    }

    /// Get the underlying probe-rs session.
    ///
    /// Note that this session is shared with the background thread that reads out the defmt
    /// messages.
    /// Therefore, this MutexGuard should only stay around for as short as possible to avoid
    /// blocking the defmt thread.
    pub fn session(&mut self) -> MutexGuard<'_, Session> {
        self.shared
            .session
            .lock()
            .expect("Probe session has been poisoned. Likely due to panic in background log thread")
    }

    /// Checks if the target has entered the `HardFault` handler
    pub fn has_hard_faulted(&mut self) -> bool {
        // let mut core = self.session.core(0).expect("could not select core 0");
        // has_hard_faulted(&mut core)
        self.shared.hard_faulted.load(Ordering::Relaxed)
    }

    // pub fn has_fatal_error(&mut self) -> bool {
    //     self.defmt_thread.shared.fatal_error.load(Ordering::Relaxed)
    // }

    /// Reads a `defmt` frame blocking if necessary
    ///
    /// # Panics
    ///
    /// Panics if the core has reached a `HardFault` and cannot emit more frames
    pub fn next_msg(&mut self) -> DefmtFrame {
        let msg = self
            .msg_rx
            .recv()
            .expect("Frame receiver disconnected. Likely due to hard fault.");
        log_defmt_msg(&msg);
        msg
    }

    /// Reads a `defmt` message from the RTT buffer without blocking
    ///
    /// Returns `None` if there isn't a complete frame in the buffer
    // can't use `StreamDecoder` because lifetimes (would require a self-referential struct) so
    // re-do the `StreamDecoder` logic here
    pub fn try_next_msg(&mut self) -> Option<DefmtFrame> {
        let msg_opt = self
            .msg_rx
            .try_recv()
            .inspect_err(|err| {
                if err == &std::sync::mpsc::TryRecvError::Disconnected
                    && self.panic_on_disconnected_error
                {
                    panic!("Defmt background thread disconnected. Likely due to a panic.")
                }
            })
            .ok();

        if let Some(frame) = &msg_opt {
            log_defmt_msg(frame);
        }

        msg_opt
    }

    /// Reads next defmt messages until either the given condition or timeout have been reached
    pub fn search_msg_for(
        &mut self,
        timeout: Duration,
        condition: impl Fn(&DefmtFrame) -> bool,
    ) -> Option<DefmtFrame> {
        let start = Instant::now();
        loop {
            let rem_timeout = timeout.saturating_sub(start.elapsed());

            match self.msg_rx.recv_timeout(rem_timeout) {
                Ok(msg) => {
                    log_defmt_msg(&msg);

                    if condition(&msg) {
                        return Some(msg);
                    }
                }
                Err(RecvTimeoutError::Disconnected) => {
                    panic!("Defmt background thread disconnected. Likely due to a panic.")
                }
                Err(RecvTimeoutError::Timeout) => return None,
            }

            // Try not to read too aggressively
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    pub fn flush_defmt_msgs(&mut self, print: bool) {
        while let Some(msg) = self.try_next_msg() {
            if print {
                println!("[defmt]: {}", msg.message);
            }
        }
    }

    pub fn set_panic_on_disconnected(&mut self, should_panic: bool) {
        self.panic_on_disconnected_error = should_panic;
    }
}

pub(crate) fn get_symbol_address(binary_file_data: &[u8], needle: &str) -> u64 {
    let file = object::File::parse(binary_file_data).expect("Parsing ELF binary should work.");

    for symbol in file.symbols() {
        if symbol.name() == Ok(needle) {
            return symbol.address();
        }
    }

    panic!("did not find symbol {needle}")
}

pub(crate) fn has_hard_faulted(core: &mut probe_rs::Core) -> bool {
    let status = core
        .status()
        .expect("could not retrieve the status of core");

    matches!(
        status,
        probe_rs::CoreStatus::Halted(probe_rs::HaltReason::Exception)
    )
}

pub(crate) fn rtt_upchannel(session: &mut Session, binary_bytes: &[u8]) -> UpChannel {
    let mut core = session.core(0).expect("could not select core 0");
    core.reset_and_halt(Duration::from_secs(5))
        .expect("could not reset device");

    // to prevent a race condition between the host reading the RTT block and the
    // target initializing. we put a breakpoint on the symbol `main` and run the app until
    // that point  when `main` is reached, static variables, including the RTT block, have
    // all been initialized
    core.set_hw_breakpoint(clear_thumb_bit(get_symbol_address(&binary_bytes, "main")))
        .expect("could not set breakpoint");
    core.run().expect("could not resume execution");
    core.wait_for_core_halted(Duration::from_secs(5))
        .expect("did not hit breakpoint");

    // TODO: check how this can be generalized without much need of adapting application code
    // // Set a hardware breakpoint when entering a fatal error, as probe-rs does not handle the
    // // reset condition very well.
    // // This way we can detect any fatal errors/panics and retrieve the error message.
    // core.set_hw_breakpoint(fatal_error_reset_address())
    //     .expect("could not set fatal_error_reset breakpoint");

    // attach to already initialized RTT block
    let mut rtt =
        probe_rs::rtt::Rtt::attach_at(&mut core, get_symbol_address(&binary_bytes, "_SEGGER_RTT"))
            .expect("did not find RTT block");

    assert_eq!(
        1,
        rtt.up_channels.len(),
        "expected exactly one RTT up channel"
    );

    let upchannel = rtt.up_channels.pop().unwrap();
    upchannel
        .set_mode(&mut core, probe_rs::rtt::ChannelMode::BlockIfFull)
        .expect("could not change RTT channel mode");

    // resume execution
    core.run().expect("could not resume execution");

    upchannel
}

pub struct ConnectionError {
    kind: ConnectionErrorKind,
}

pub enum ConnectionErrorKind {}

/// in the Cortex-M ISA, routines (functions) are always 2-byte aligned but
// in the ELF file; and machine code, they have the bit 0, the "thumb bit"
// set to 1 to indicate they contain THUMB instructions the probe-rs API
// expects the thumb bit to be cleared so we do that here.
fn clear_thumb_bit(addr: u64) -> u64 {
    addr & (!1u64)
}
