// Copyright 2026, The Android Open Source Project
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! This module manages dynamic camera frames streamed from a host media file
//! by spawning an FFmpeg child process, capturing raw frames from stdout,
//! and caching them into a thread-safe slot for O(1) retrieval.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

// Frame configuration matching the fixed I420@640x480 resolution
pub const WIDTH: usize = 640;
pub const HEIGHT: usize = 480;
pub const Y_SIZE: usize = WIDTH * HEIGHT; // 307200 bytes
pub const UV_SIZE: usize = (WIDTH * HEIGHT) / 4; // 76800 bytes
pub const FRAME_SIZE: usize = Y_SIZE + (2 * UV_SIZE); // 460800 bytes

/// Type of media source file, detected from the file extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    /// Video streams (e.g. MP4) decoded and looped at native frame rate.
    Video,
    /// Still images (e.g. JPG, PNG) decoded once to a static frame.
    StillImage,
}

/// Outcome of filling V4L2 planar buffer sinks from the media cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FillOutcome {
    /// Successfully populated buffer planes with a fresh decoded frame.
    Live,
    /// Population succeeded using a frozen, cached frame after decode failure.
    Frozen,
    /// No frame decoded yet (e.g., warmup, or critical decode failure).
    NoFrame,
}

/// Lifecycle state of the MediaSource background thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceState {
    /// Inactive state, child processes and threads are reaped.
    Idle,
    /// Active state, child process/thread is actively decoding/rendering.
    Running,
    /// Unrecoverable decoding error or retry threshold exceeded.
    Failed,
}

/// Handles robust, asynchronous V4L2 frame generation from a host-side file via FFmpeg.
pub struct MediaSource {
    path: PathBuf,
    ffmpeg: PathBuf,
    kind: SourceKind,
    state: Arc<Mutex<SourceState>>,
    latest: Arc<Mutex<Option<Arc<[u8]>>>>,
    stop: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
    reader: Option<JoinHandle<()>>,
}

impl MediaSource {
    /// Constructs a new MediaSource with lazy process initialization.
    pub fn new(path: PathBuf, ffmpeg: PathBuf) -> Self {
        let kind = Self::detect_kind(&path);
        Self {
            path,
            ffmpeg,
            kind,
            state: Arc::new(Mutex::new(SourceState::Idle)),
            latest: Arc::new(Mutex::new(None)),
            stop: Arc::new(AtomicBool::new(false)),
            child: Arc::new(Mutex::new(None)),
            reader: None,
        }
    }

    /// Accessor for the resolved SourceKind.
    #[allow(dead_code)]
    pub fn kind(&self) -> SourceKind {
        self.kind
    }

    /// Detects whether the file path represents a still image or a video file.
    fn detect_kind(path: &Path) -> SourceKind {
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            let ext_lower = ext.to_lowercase();
            match ext_lower.as_str() {
                "jpg" | "jpeg" | "png" | "bmp" | "webp" | "gif" => SourceKind::StillImage,
                _ => SourceKind::Video,
            }
        } else {
            SourceKind::Video
        }
    }

    /// Executes Layer 1 boot-critical checks to ensure binary and file availability.
    /// This is designed to be sub-millisecond, non-blocking, and boot-safe.
    pub fn check_cheap(path: &Path, ffmpeg: &Path) -> Result<(), String> {
        if !path.exists() {
            return Err(format!("File '{}' does not exist", path.display()));
        }
        if !path.is_file() {
            return Err(format!("Path '{}' is not a regular file", path.display()));
        }

        // Spawn a fast version check to ensure ffmpeg is on the host PATH or resolved
        let child = Command::new(ffmpeg)
            .arg("-version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("Failed to execute '{}': {}", ffmpeg.display(), e))?;

        // Protect against any unexpected binary hangs with a 2-second timeout thread-killer
        let shared_child = Arc::new(Mutex::new(Some(child)));
        let shared_child_clone = shared_child.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(2));
            if let Ok(mut guard) = shared_child_clone.lock() {
                if let Some(mut c) = guard.take() {
                    let _ = c.kill();
                    let _ = c.wait();
                }
            }
        });

        if let Ok(mut guard) = shared_child.lock() {
            if let Some(mut c) = guard.take() {
                match c.wait() {
                    Ok(status) if status.success() => Ok(()),
                    Ok(status) => Err(format!("ffmpeg returned non-zero exit code: {:?}", status)),
                    Err(e) => Err(format!("Failed to wait for ffmpeg: {}", e)),
                }
            } else {
                Err("ffmpeg check timed out after 2 seconds".to_string())
            }
        } else {
            Err("Mutex poisoned during ffmpeg check".to_string())
        }
    }

    /// Performs an expensive codec and decode validation test, intended for Strict Mode only.
    pub fn probe_decode(path: &Path, ffmpeg: &Path, timeout: Duration) -> Result<(), String> {
        let mut cmd = Command::new(ffmpeg);
        cmd.args([
            "-nostdin",
            "-v",
            "error",
            "-i",
            path.to_str()
                .ok_or_else(|| "Invalid path encoding".to_string())?,
            "-frames:v",
            "1",
            "-f",
            "null",
            "-",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

        let child = cmd
            .spawn()
            .map_err(|e| format!("Failed to spawn probe process: {}", e))?;
        let shared_child = Arc::new(Mutex::new(Some(child)));
        let shared_child_clone = shared_child.clone();

        // Spawn watchdog watchdog thread to kill+reap process on timeout
        std::thread::spawn(move || {
            std::thread::sleep(timeout);
            if let Ok(mut guard) = shared_child_clone.lock() {
                if let Some(mut c) = guard.take() {
                    log::error!("Decode probe timed out! Killing child process.");
                    let _ = c.kill();
                    let _ = c.wait();
                }
            }
        });

        if let Ok(mut guard) = shared_child.lock() {
            if let Some(mut c) = guard.take() {
                let mut stderr_str = String::new();
                if let Some(mut stderr) = c.stderr.take() {
                    use std::io::Read;
                    let _ = stderr.read_to_string(&mut stderr_str);
                }
                match c.wait() {
                    Ok(status) if status.success() => Ok(()),
                    Ok(status) => Err(format!(
                        "ffmpeg exit code {:?}. Stderr output: {}",
                        status,
                        stderr_str.trim()
                    )),
                    Err(e) => Err(format!("Failed to wait for probe process: {}", e)),
                }
            } else {
                Err(format!("Decode probe timed out after {:?}", timeout))
            }
        } else {
            Err("Mutex poisoned during decode probe".to_string())
        }
    }

    /// Idempotently ensures the background decoder is running.
    /// Resets the Failed state and spawns a fresh child process / reader on stream restarts.
    pub fn ensure_started(&mut self) {
        let is_running = {
            let state_guard = self.state.lock().unwrap();
            *state_guard == SourceState::Running
        };

        if is_running {
            return;
        }

        // Teardown any previous stale runs cleanly
        self.stop();

        self.stop.store(false, Ordering::SeqCst);

        let path = self.path.clone();
        let ffmpeg = self.ffmpeg.clone();
        let state = self.state.clone();
        let latest = self.latest.clone();
        let stop = self.stop.clone();
        let child_shared = self.child.clone();

        *self.state.lock().unwrap() = SourceState::Running;

        match self.kind {
            SourceKind::StillImage => {
                let thread_handle = std::thread::spawn(move || {
                    let mut cmd = Command::new(&ffmpeg);
                    cmd.args([
                        "-nostdin",
                        "-loglevel",
                        "error",
                        "-i",
                        path.to_str().unwrap_or(""),
                        "-vf",
                        "scale=640:480",
                        "-pix_fmt",
                        "yuv420p",
                        "-frames:v",
                        "1",
                        "-f",
                        "rawvideo",
                        "pipe:1",
                    ])
                    .stdout(Stdio::piped())
                    .stderr(Stdio::inherit())
                    .stdin(Stdio::null());

                    let mut child = match cmd.spawn() {
                        Ok(c) => c,
                        Err(e) => {
                            log::error!("Failed to spawn one-shot ffmpeg: {}", e);
                            *state.lock().unwrap() = SourceState::Failed;
                            return;
                        }
                    };

                    let stdout = child.stdout.take().unwrap();
                    if let Ok(mut guard) = child_shared.lock() {
                        *guard = Some(child);
                    }

                    // Watchdog thread to prevent decoder stalls (5-second deadline)
                    let child_shared_clone = child_shared.clone();
                    let watchdog = std::thread::spawn(move || {
                        std::thread::sleep(Duration::from_secs(5));
                        if let Ok(mut guard) = child_shared_clone.lock() {
                            if let Some(mut c) = guard.take() {
                                log::error!("Still-image decoding timed out, aborting.");
                                let _ = c.kill();
                                let _ = c.wait();
                            }
                        }
                    });

                    let mut buf = vec![0u8; FRAME_SIZE];
                    use std::io::Read;
                    let mut reader = stdout;
                    match reader.read_exact(&mut buf) {
                        Ok(()) => {
                            *latest.lock().unwrap() = Some(Arc::from(buf.as_slice()));
                            // Complete and reap process cleanly
                            if let Ok(mut guard) = child_shared.lock() {
                                if let Some(mut c) = guard.take() {
                                    let _ = c.wait();
                                }
                            }
                        }
                        Err(e) => {
                            log::error!("Failed to read raw frame from image decoder: {}", e);
                            *state.lock().unwrap() = SourceState::Failed;
                            if let Ok(mut guard) = child_shared.lock() {
                                if let Some(mut c) = guard.take() {
                                    let _ = c.kill();
                                    let _ = c.wait();
                                }
                            }
                        }
                    }
                    let _ = watchdog.join();
                });
                self.reader = Some(thread_handle);
            }
            SourceKind::Video => {
                let mut cmd = Command::new(&ffmpeg);
                cmd.args([
                    "-nostdin",
                    "-loglevel",
                    "error",
                    "-stream_loop",
                    "-1",
                    "-re",
                    "-i",
                    path.to_str().unwrap_or(""),
                    "-vf",
                    "scale=640:480",
                    "-an",
                    "-sn",
                    "-dn",
                    "-pix_fmt",
                    "yuv420p",
                    "-f",
                    "rawvideo",
                    "pipe:1",
                ])
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .stdin(Stdio::null());

                let mut child = match cmd.spawn() {
                    Ok(c) => c,
                    Err(e) => {
                        log::error!("Failed to spawn persistent video decoder: {}", e);
                        *self.state.lock().unwrap() = SourceState::Failed;
                        return;
                    }
                };

                let stdout = child.stdout.take().unwrap();
                if let Ok(mut guard) = child_shared.lock() {
                    *guard = Some(child);
                }

                // First-frame watchdog thread (5-second deadline)
                let first_frame_received = Arc::new(AtomicBool::new(false));
                let first_frame_received_clone = first_frame_received.clone();
                let child_shared_clone = child_shared.clone();
                let state_clone = state.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_secs(5));
                    if !first_frame_received_clone.load(Ordering::SeqCst) {
                        if let Ok(mut guard) = child_shared_clone.lock() {
                            if let Some(mut c) = guard.take() {
                                log::error!("Video stream first-frame timeout! Aborting decoder.");
                                let _ = c.kill();
                                let _ = c.wait();
                                *state_clone.lock().unwrap() = SourceState::Failed;
                            }
                        }
                    }
                });

                let thread_handle = std::thread::spawn(move || {
                    let mut reader = stdout;
                    let mut buf = vec![0u8; FRAME_SIZE];
                    let mut frames_decoded: u64 = 0;
                    let mut retry_count = 0;

                    loop {
                        if stop.load(Ordering::SeqCst) {
                            break;
                        }

                        use std::io::Read;
                        match reader.read_exact(&mut buf) {
                            Ok(()) => {
                                frames_decoded += 1;
                                first_frame_received.store(true, Ordering::SeqCst);
                                *latest.lock().unwrap() = Some(Arc::from(buf.as_slice()));
                            }
                            Err(e) => {
                                if stop.load(Ordering::SeqCst) {
                                    break;
                                }

                                // Still-Image Respawn Defense:
                                // If a video file successfully yields exactly 1 frame and hits clean EOF,
                                // we treat it as a static scene and immediately finalize without burning retries.
                                if frames_decoded == 1 {
                                    log::info!(
                                        "Video source yielded exactly 1 frame; stabilizing as a static scene."
                                    );
                                    break;
                                }

                                // Loop on clean EOF:
                                // If we successfully decoded frames (> 1) and then hit EOF of the pipe,
                                // we treat it as a natural end-of-play loop transition.
                                let is_natural_eof = frames_decoded > 1;
                                if is_natural_eof {
                                    log::info!(
                                        "Video reached EOF ({} frames decoded). Looping stream cleanly.",
                                        frames_decoded
                                    );
                                    retry_count = 0;
                                    frames_decoded = 0;
                                } else {
                                    if retry_count >= 5 {
                                        log::error!(
                                            "FFmpeg reader exhausted retry budget ({} attempts). Marking Failed.",
                                            retry_count
                                        );
                                        *state.lock().unwrap() = SourceState::Failed;
                                        break;
                                    }

                                    retry_count += 1;
                                    let backoff_ms = 100 * retry_count;
                                    log::warn!(
                                        "FFmpeg pipe closed. Retrying in {}ms (attempt {}/5). Error: {}",
                                        backoff_ms,
                                        retry_count,
                                        e
                                    );
                                    std::thread::sleep(Duration::from_millis(backoff_ms));
                                }

                                // Kill and reap the previous dead child handle
                                if let Ok(mut guard) = child_shared.lock() {
                                    if let Some(mut c) = guard.take() {
                                        let _ = c.kill();
                                        let _ = c.wait();
                                    }
                                }

                                // Spawn fresh video decoder subprocess
                                let mut cmd = Command::new(&ffmpeg);
                                cmd.args([
                                    "-nostdin",
                                    "-loglevel",
                                    "error",
                                    "-stream_loop",
                                    "-1",
                                    "-re",
                                    "-i",
                                    path.to_str().unwrap_or(""),
                                    "-vf",
                                    "scale=640:480",
                                    "-an",
                                    "-sn",
                                    "-dn",
                                    "-pix_fmt",
                                    "yuv420p",
                                    "-f",
                                    "rawvideo",
                                    "pipe:1",
                                ])
                                .stdout(Stdio::piped())
                                .stderr(Stdio::inherit())
                                .stdin(Stdio::null());

                                match cmd.spawn() {
                                    Ok(mut fresh_child) => {
                                        if let Some(stdout_pipe) = fresh_child.stdout.take() {
                                            reader = stdout_pipe;
                                            if let Ok(mut guard) = child_shared.lock() {
                                                *guard = Some(fresh_child);
                                            }
                                        } else {
                                            log::error!(
                                                "Failed to extract stdout pipe from respawned ffmpeg"
                                            );
                                            *state.lock().unwrap() = SourceState::Failed;
                                            break;
                                        }
                                    }
                                    Err(err) => {
                                        log::error!("Failed to spawn child during retry: {}", err);
                                        *state.lock().unwrap() = SourceState::Failed;
                                        break;
                                    }
                                }
                            }
                        }
                    }
                });
                self.reader = Some(thread_handle);
            }
        }
    }

    /// Stops the decoder background threads and child processes.
    /// Always wait and reap the child to prevent zombie processes.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);

        // Kill and reap child process synchronously
        if let Ok(mut guard) = self.child.lock() {
            if let Some(mut c) = guard.take() {
                let _ = c.kill();
                let _ = c.wait();
            }
        }

        // Wait for background reader thread to join
        if let Some(handle) = self.reader.take() {
            let _ = handle.join();
        }

        *self.state.lock().unwrap() = SourceState::Idle;
    }

    /// Copies the cached decoded frame into the planar V4L2 output buffers.
    /// This runs in O(1) time and is non-blocking.
    pub fn fill<WY: std::io::Write, WU: std::io::Write, WV: std::io::Write>(
        &self,
        mut y: WY,
        mut u: WU,
        mut v: WV,
    ) -> Result<FillOutcome, i32> {
        let frame_opt = self.latest.lock().unwrap().clone();
        let Some(frame) = frame_opt else {
            return Ok(FillOutcome::NoFrame);
        };

        y.write_all(&frame[..Y_SIZE]).map_err(|_| libc::EIO)?;
        u.write_all(&frame[Y_SIZE..Y_SIZE + UV_SIZE])
            .map_err(|_| libc::EIO)?;
        v.write_all(&frame[Y_SIZE + UV_SIZE..FRAME_SIZE])
            .map_err(|_| libc::EIO)?;

        if *self.state.lock().unwrap() == SourceState::Failed {
            Ok(FillOutcome::Frozen)
        } else {
            Ok(FillOutcome::Live)
        }
    }
}

impl Drop for MediaSource {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
#[allow(dead_code)]
impl MediaSource {
    pub fn set_latest_frame_for_test(&self, frame: Arc<[u8]>) {
        *self.latest.lock().unwrap() = Some(frame);
    }
    pub fn set_failed_for_test(&self) {
        *self.state.lock().unwrap() = SourceState::Failed;
    }
}
