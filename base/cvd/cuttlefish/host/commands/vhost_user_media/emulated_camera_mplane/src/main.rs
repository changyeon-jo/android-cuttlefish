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

use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use clap::Parser;
use log::error;
use thiserror::Error;
use vhost_user_backend::VhostUserDaemon;
use vhu_media::VhuMediaBackend;
use virtio_media::protocol::VirtioMediaDeviceConfig;
use vm_memory::{GuestMemoryAtomic, GuestMemoryMmap};

mod device;
use device::LensFacing;

mod media_source;

pub mod pattern;

#[derive(Debug, Error)]
pub(crate) enum Error {
    #[error("Invalid argument: {0}")]
    InvalidArgument(String),
    #[error("Could not create daemon: {0}")]
    CouldNotCreateDaemon(vhost_user_backend::Error),
    #[error("Fatal error: {0}")]
    ServeFailed(vhost_user_backend::Error),
    #[error("Media file is unavailable: {0}")]
    MediaUnavailable(String),
}

type Result<T> = std::result::Result<T, Error>;

#[derive(Parser, Debug)]
#[clap(author, version, about, long_about = None)]
struct CmdLineArgs {
    /// Location of vhost-user Unix domain socket.
    #[clap(short, long, value_name = "SOCKET")]
    socket_path: PathBuf,
    /// Log verbosity, one of Off, Error, Warning, Info, Debug, Trace.
    #[clap(short, long, default_value_t = log::LevelFilter::Debug)]
    verbosity: log::LevelFilter,
    /// Lens facing configuration: FRONT, BACK, or EXTERNAL.
    #[clap(long, value_name = "LENS_FACING", default_value = "EXTERNAL")]
    lens_facing: String,
    /// Host media file (video, e.g. .mp4, or still image, e.g. .jpg) to stream as the
    /// camera feed.
    #[clap(long, value_name = "FILE")]
    media_file: Option<PathBuf>,
    /// Path to the ffmpeg binary (default: resolve "ffmpeg" from PATH).
    #[clap(long, value_name = "BIN", default_value = "ffmpeg")]
    ffmpeg_path: PathBuf,
    /// Fail startup instead of degrading to test patterns when --media-file cannot be
    /// used (missing ffmpeg, unreadable/undecodable file). For CI/lab use.
    #[clap(long)]
    require_media: bool,
    /// Timeout in seconds for the strict-mode startup decode probe.
    #[clap(long, value_name = "SECS", default_value_t = 5)]
    media_probe_timeout: u64,
}

#[derive(PartialEq, Debug)]
struct Config {
    socket_path: PathBuf,
    lens_facing: LensFacing,
    media_file: Option<PathBuf>,
    ffmpeg_path: PathBuf,
    require_media: bool,
    media_probe_timeout: u64,
}

impl TryFrom<CmdLineArgs> for Config {
    type Error = Error;

    fn try_from(args: CmdLineArgs) -> Result<Self> {
        let lens_facing = args
            .lens_facing
            .parse::<LensFacing>()
            .map_err(Error::InvalidArgument)?;
        Ok(Config {
            socket_path: args.socket_path,
            lens_facing,
            media_file: args.media_file,
            ffmpeg_path: args.ffmpeg_path,
            require_media: args.require_media,
            media_probe_timeout: args.media_probe_timeout,
        })
    }
}

fn init_logging(verbosity: log::LevelFilter) -> Result<()> {
    env_logger::builder()
        .format_timestamp_secs()
        .filter_level(verbosity)
        .init();
    Ok(())
}

const VFL_TYPE_VIDEO: u32 = 0;

fn start_backend(config: Config) -> Result<()> {
    let socket_path = config.socket_path.clone();
    let mut card = [0u8; 32];
    let card_name = "emulated_camera";
    card[0..card_name.len()].copy_from_slice(card_name.as_bytes());

    // Resolve startup/graceful decline on the media file ONCE before the serve loop
    let media_file = match config.media_file.as_ref() {
        None => None,
        Some(path) => match media_source::MediaSource::check_cheap(path, &config.ffmpeg_path) {
            Err(reason) if config.require_media => {
                return Err(Error::MediaUnavailable(reason));
            }
            Err(reason) => {
                log::warn!(
                    "--media-file {} is unusable: {}; camera will run with synthetic test patterns only",
                    path.display(),
                    reason
                );
                None
            }
            Ok(()) => {
                if config.require_media {
                    let timeout = Duration::from_secs(config.media_probe_timeout);
                    if let Err(reason) =
                        media_source::MediaSource::probe_decode(path, &config.ffmpeg_path, timeout)
                    {
                        return Err(Error::MediaUnavailable(reason));
                    }
                }
                Some(path.clone())
            }
        },
    };

    // When the main vm shuts down, the damon exits gracefully. Using an infinite loop to work
    // across VMs restarts rather than having to manually start the binary again.
    loop {
        use virtio_media::v4l2r::ioctl::Capabilities;
        let device_config = VirtioMediaDeviceConfig {
            device_caps: (Capabilities::VIDEO_CAPTURE_MPLANE | Capabilities::STREAMING).bits(),
            device_type: VFL_TYPE_VIDEO,
            card,
        };
        let lens_facing = config.lens_facing;
        let ffmpeg_path = config.ffmpeg_path.clone();
        let media_file_clone = media_file.clone();

        let backend = Arc::new(RwLock::new(VhuMediaBackend::new(
            device_config,
            move |event_queue, host_mapper| {
                crate::device::EmulatedCamera::new(
                    event_queue,
                    host_mapper,
                    lens_facing,
                    media_file_clone.clone(),
                    ffmpeg_path.clone(),
                )
            },
        )));
        let mut daemon = VhostUserDaemon::new(
            String::from("vhost-user-media-backend"),
            backend,
            GuestMemoryAtomic::new(GuestMemoryMmap::new()),
        )
        .map_err(Error::CouldNotCreateDaemon)?;
        log::info!("vhost-user-media-backend daemon started");
        daemon.serve(&socket_path).map_err(Error::ServeFailed)?;
        log::info!("vhost-user-media-backend daemon closed gracefully");
    }
}

fn main() -> Result<()> {
    crate::device::init_start_time();
    let args = CmdLineArgs::parse();

    init_logging(args.verbosity)?;

    start_backend(Config::try_from(args)?)
}
