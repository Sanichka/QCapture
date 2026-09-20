//! Phase 5b: ffmpeg audio input via Windows named pipe.
//!
//! A process has a single stdin (already the video rawvideo pipe), so mixed
//! PCM reaches ffmpeg through `\\.\pipe\qcapture-audio-<pid>-<n>`. The writer
//! thread accepts one client (ffmpeg), streams i16 stereo 48 kHz chunks, and
//! closes on channel disconnect — that EOF lets ffmpeg finalize its trailer.
//!
//! Shutdown ordering matters (see `run_ffmpeg_*`): capture end -> audio
//! pipeline shutdown -> pipe EOF -> ffmpeg exit -> pump join. Anything else
//! deadlocks with ffmpeg waiting on a live input.

use std::io::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::JoinHandle;

use super::EncodeError;

static PIPE_SEQ: AtomicU64 = AtomicU64::new(0);

pub struct FfmpegAudioPipe {
    pub name: String,
    tx: flume::Sender<Vec<u8>>,
    handle: Option<JoinHandle<Result<u64, String>>>,
}

impl FfmpegAudioPipe {
    pub fn create() -> Result<Self, EncodeError> {
        let n = PIPE_SEQ.fetch_add(1, Ordering::Relaxed);
        let name = format!(r"\\.\pipe\qcapture-audio-{}-{n}", std::process::id());
        let (tx, rx) = flume::bounded::<Vec<u8>>(120);
        let thread_name = name.clone();
        let handle = std::thread::Builder::new()
            .name("qcapture-audio-pipe".into())
            .spawn(move || pipe_main(&thread_name, rx))
            .map_err(|e| EncodeError::Probe(format!("audio pipe thread: {e}")))?;
        Ok(Self {
            name,
            tx,
            handle: Some(handle),
        })
    }

    pub fn sender(&self) -> flume::Sender<Vec<u8>> {
        self.tx.clone()
    }

    /// Close the pipe and wait for the writer thread. Call only after ffmpeg
    /// has connected at least once; on early errors, drop without finishing
    /// (the blocked accept dies with the process — logged, not joined).
    pub fn finish(mut self) -> Result<u64, String> {
        drop(self.tx);
        self.handle
            .take()
            .unwrap()
            .join()
            .map_err(|_| "audio pipe thread panicked".to_string())?
    }
}

fn pipe_main(name: &str, rx: flume::Receiver<Vec<u8>>) -> Result<u64, String> {
    use interprocess::os::windows::named_pipe::*;
    let listener = PipeListenerOptions::new()
        .path(name)
        .create_send_only::<pipe_mode::Bytes>()
        .map_err(|e| format!("pipe listen {name}: {e}"))?;
    // Blocks until ffmpeg opens the pipe. If ffmpeg never starts (bad args),
    // the caller must NOT join this thread — see `finish`.
    let mut stream = listener.accept().map_err(|e| format!("pipe accept: {e}"))?;
    let mut bytes = 0u64;
    for chunk in rx {
        // A failed write means ffmpeg went away: either it died mid-record
        // (the video pump's finish() will report the real error) or it exited
        // after a `-shortest` cut while we flush the tail. Either way there is
        // nothing useful left to do here — stop quietly instead of warning.
        if stream.write_all(&chunk).is_err() {
            break;
        }
        bytes += chunk.len() as u64;
    }
    let _ = stream.flush();
    drop(stream); // EOF -> ffmpeg finalizes audio
    Ok(bytes)
}

/// Forward mixed i16 chunks into the pipe sender. Ends when either side
/// disconnects; fire-and-forget (no join — both ends are finite).
pub fn forward_to_pipe(
    mixed: flume::Receiver<Vec<u8>>,
    dest: flume::Sender<Vec<u8>>,
) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("qcapture-audio-forward".into())
        .spawn(move || {
            for chunk in mixed {
                if dest.send(chunk).is_err() {
                    break;
                }
            }
        })
        .expect("audio forward thread spawn")
}
