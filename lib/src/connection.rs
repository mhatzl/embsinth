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
use probe_rs::{
    Core, Session,
    rtt::{ChannelMode, UpChannel},
};

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

        log::warn!("Dropping connection to probe: {:?}", self.probe_id);

        // Free the selected probe so it may be reused for new connections
        // The probe session is
        {
            let mut states = crate::probe::PROBE_STATES
                .lock()
                .expect("Probe state map has been poisoned in some thread");
            let state = states
                .get_mut(&self.probe_id)
                .expect("Probe ID must have been inserted when aquiring");
            *state = ProbeState::Free;
        }
    }
}

impl Connection {
    pub(crate) fn new(mut probe: AttachedProbe, binary_file: PathBuf) -> Self {
        let binary_bytes = std::fs::read(&binary_file).expect("Failed to read binary bytes");

        let upchannel = rtt_upchannel(&mut probe.session, &binary_bytes);

        let (msg_tx, msg_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
        let shared = std::sync::Arc::new(DefmtThreadCommunication::new(probe.session));

        crate::defmt::spawn_defmt_thread(
            std::thread::current(),
            binary_bytes,
            &shared,
            msg_tx,
            done_tx,
            upchannel,
        );

        // std::thread::park();

        Self {
            probe_id: probe.id,
            chip: probe.chip,
            shared,
            msg_rx,
            done_rx,
            panic_on_disconnected_error: true,
        }
    }

    pub fn start(&mut self) {
        let mut session = self
            .shared
            .session
            .lock()
            .expect("Session poisoned by bad log extraction");
        let _ = session.core(0).expect("Failed to access Core 0").run();
    }

    pub fn close(self) {
        drop(self);
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
        self.msg_rx
            .recv()
            .expect("Frame receiver disconnected. Likely due to hard fault.")
    }

    /// Reads a `defmt` message from the RTT buffer without blocking
    ///
    /// Returns `None` if there isn't a complete frame in the buffer
    // can't use `StreamDecoder` because lifetimes (would require a self-referential struct) so
    // re-do the `StreamDecoder` logic here
    pub fn try_next_msg(&mut self) -> Option<DefmtFrame> {
        self.msg_rx
            .try_recv()
            .inspect_err(|err| {
                if err == &std::sync::mpsc::TryRecvError::Disconnected
                    && self.panic_on_disconnected_error
                {
                    panic!("Defmt background thread disconnected. Likely due to a panic.")
                }
            })
            .ok()
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

pub(crate) fn rtt_upchannel(session: &mut Session, binary_bytes: &[u8]) -> RttActiveUpChannel {
    let mut core = session.core(0).expect("could not select core 0");
    core.reset_and_halt(Duration::from_secs(5))
        .expect("could not reset device");

    // to prevent a race condition between the host reading the RTT block and the
    // target initializing. we put a breakpoint on the symbol `main` and run the app until
    // that point  when `main` is reached, static variables, including the RTT block, have
    // all been initialized
    core.set_hw_breakpoint(clear_thumb_bit(get_symbol_address(binary_bytes, "main")))
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
        probe_rs::rtt::Rtt::attach_at(&mut core, get_symbol_address(binary_bytes, "_SEGGER_RTT"))
            .expect("did not find RTT block");

    assert_eq!(
        1,
        rtt.up_channels.len(),
        "expected exactly one RTT up channel"
    );

    let up_channel = rtt.up_channels.pop().unwrap();

    let mut active_channel = RttActiveUpChannel::new(up_channel);

    log::info!(
        "Defmt channel Size={}, Mode={:?}, name={:?}",
        active_channel.up_channel.buffer_size(),
        active_channel.up_channel.mode(&mut core),
        active_channel.up_channel.name()
    );

    core.clear_all_hw_breakpoints()
        .expect("Failed to clear hw breakpoints");
    core.run().expect("Failed to continue execution");

    active_channel
        .change_mode(&mut core, ChannelMode::BlockIfFull)
        .expect("could not change RTT channel mode");

    active_channel

    // upchannel
    //     .set_mode(&mut core, probe_rs::rtt::ChannelMode::BlockIfFull)
    //     .expect("could not change RTT channel mode");

    // log::info!(
    //     "RTT Mode: {:?}",
    //     upchannel.mode(&mut core).expect("RTT Mode has been set")
    // );

    // // resume execution
    // // core.run().expect("could not resume execution");

    // upchannel
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

#[derive(Debug)]
pub(crate) struct RttActiveUpChannel {
    up_channel: UpChannel,
    rtt_buffer: Box<[u8]>,
    bytes_buffered: usize,

    /// If set, the original mode of the channel before we first changed it. Upon exit we should do
    /// our best to restore the original mode.
    original_mode: Option<ChannelMode>,
}

impl RttActiveUpChannel {
    pub fn new(up_channel: UpChannel) -> Self {
        Self {
            rtt_buffer: vec![0; up_channel.buffer_size().max(1)].into_boxed_slice(),
            bytes_buffered: 0,
            up_channel,
            original_mode: None,
        }
    }

    pub fn change_mode(&mut self, core: &mut Core, mode: ChannelMode) -> Result<(), anyhow::Error> {
        if self.original_mode.is_none() {
            self.original_mode = Some(self.up_channel.mode(core)?);
        }

        Ok(self.up_channel.set_mode(core, mode)?)
    }

    pub fn channel_name(&self) -> String {
        self.up_channel
            .name()
            .map(ToString::to_string)
            .unwrap_or_else(|| format!("Unnamed RTT up channel - {}", self.up_channel.number()))
    }

    /// Returns the buffer size in bytes. Note that the usable size is one byte less due to how the
    /// ring buffer is implemented.
    pub fn buffer_size(&self) -> usize {
        self.up_channel.buffer_size()
    }

    pub fn number(&self) -> u32 {
        self.up_channel.number() as u32
    }

    /// Reads available channel data into the internal buffer.
    pub fn poll(&mut self, core: &mut Core) -> Result<(), anyhow::Error> {
        self.bytes_buffered = self.up_channel.read(core, self.rtt_buffer.as_mut())?;
        Ok(())
    }

    /// Returns the buffered data.
    pub fn buffered_data(&self) -> &[u8] {
        &self.rtt_buffer[..self.bytes_buffered]
    }

    /// Clean up temporary changes made to the channel.
    pub fn clean_up(&mut self, core: &mut Core) -> Result<(), anyhow::Error> {
        if let Some(mode) = self.original_mode.take() {
            self.up_channel.set_mode(core, mode)?;
        }
        Ok(())
    }
}
