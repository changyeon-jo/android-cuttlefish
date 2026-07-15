# Emulated Camera (V4L2 Multi-Planar)

This crate provides an emulated `virtio-media` video capture device backend for Cuttlefish, serving true V4L2 Multi-Planar API video streams via the `YM12` (YUV 4:2:0) pixel format.

## Features

- **Multi-Planar Output:** Implements the V4L2 Multi-Planar API for high-fidelity Y, U, and V plane sink separation.
- **Fixed Stream Properties:** Default video capture stream configured at a standard **640x480** resolution and **30 FPS**.
- **Dynamic V4L2 Controls:** Supports the `V4L2_CID_IMAGE_PROC_CLASS` control class and runtime `V4L2_CID_TEST_PATTERN` switching via `VIDIOC_S_EXT_CTRLS`.
- **Integrated Event Subscriptions:** Wires up `VIDIOC_SUBSCRIBE_EVENT` for atomic control and event delivery to the guest OS.
- **Real-Time Host Media Source:** Supports streaming custom host-side video files (e.g., `.mp4`, `.mkv`) and single-shot still images (e.g., `.jpg`, `.png`) as the live camera feed using an FFmpeg-backed pipeline.
- **Self-Healing Lazy-Start Architecture:** Seamlessly recovers from cameraserver connection resets, API handshakes (API v1 to v2 connect/disconnect sequences), and display sleep/dimming events by dynamically and idempotently spinning up decoder threads on the very first queued buffer.
- **Continuous C-Level Looping (`-stream_loop -1`):** Native C-level looping inside the FFmpeg subprocess keeps the stream pipe permanently open, eliminating EOF process-respawn overhead and preventing transient visual test-pattern splashes.
- **Active Memory Reclamation:** Non-fatal recovery for `do_munmap` invalid offset lookup misses, allowing the guest kernel to cleanly complete mapping teardowns across session boundary recreations.

## Dynamic Test Patterns

When no media file is specified (or when selected via V4L2 controls), the emulated camera provides three hardware-accelerated/synthetic video generation patterns that can be dynamically switched at runtime:

- **Pattern 0 (Pulse):** A uniform, smooth color-cycling mode.
- **Pattern 1 (SMPTE Bars + Bouncing Box):** Standard color SMPTE bars with an animated inverse-color bounding box overlay. Ideal for visually validating multi-sink synchronization, chroma decoding (`YM12`/`I420`), and frame tearing.
- **Pattern 2 (Animated Julia Set Fractal):** A CPU-heavy, animated Julia Set fractal pattern designed to simulate high-load or jittery host scheduling environments.
- **Pattern 3 (Media File Source):** Feeds live decoded frames from the specified host media file. If no file is provided, degrades gracefully to a solid neutral black screen (`Y=16`, `U=128`, `V=128`).

## Guest OS Usage

To list the supported controls and dynamically change the active test pattern on the fly from the guest OS, leverage `v4l2-ctl`:

```bash
# Query available test pattern options
v4l2-ctl -d /dev/video1 --list-ctrls

# Switch to the SMPTE Bars & Bouncing Box overlay (Pattern 1)
v4l2-ctl -d /dev/video1 -c test_pattern=1

# Switch to the Julia Set Fractal (Pattern 2)
v4l2-ctl -d /dev/video1 -c test_pattern=2

# Switch to the Custom Media File Feed (Pattern 3)
v4l2-ctl -d /dev/video1 -c test_pattern=3
```

## Android Guest Lifecycle & Session Resets

During continuous preview, the Android guest OS may close and immediately reopen the camera device file descriptor `/dev/video1`. This typically happens in two scenarios:
1. **Initial Handshake (API v1 to v2 Transition):** On launching the camera app, the `cameraserver` briefly opens the node in Camera API v1 mode, closes it, and reopens it in modern Camera API v2 mode.
2. **Idle Display Sleep/Dimming (exactly 120s):** If the device screen is left untouched for 120 seconds, the Android `ActivityTaskManager` pauses the camera activity, releasing the camera session and closing `/dev/video1`. Waking the screen immediately resumes the activity and reopens the device.

Since the single-threaded vhost-user Unix domain socket accepts only one active client at a time, each reopen causes a connection teardown and instantiates a brand-new camera backend. Thanks to our **Self-Healing Lazy-Start Architecture**, the new backend safely reclaims old mappings and dynamically spins up a fresh FFmpeg decoder on the very first queued frame in under 500ms, showing a brief, smooth test-pattern color splash transition before seamless playback resumes.

## Media File Camera Emulation

You can emulate the camera feed using any video file or static image on the host filesystem.

### Standalone Command-Line Options

The backend binary supports the following options:

```text
Options:
  -s, --socket-path <SOCKET>        Location of vhost-user Unix domain socket
  -v, --verbosity <VERBOSITY>       Log verbosity, one of Off, Error, Warning, Info, Debug, Trace [default: DEBUG]
      --lens-facing <LENS_FACING>   Lens facing configuration: FRONT, BACK, or EXTERNAL [default: BACK]
      --media-file <FILE>           Host media file (video, e.g. .mp4, or still image, e.g. .jpg) to stream as the camera feed
      --ffmpeg-path <BIN>           Path to the ffmpeg binary (default: resolve "ffmpeg" from PATH) [default: ffmpeg]
      --require-media               Fail startup instead of degrading to test patterns when --media-file cannot be used (missing ffmpeg, unreadable/undecodable file). For CI/lab use
      --media-probe-timeout <SECS>  Timeout in seconds for the strict-mode startup decode probe [default: 5]
```

### Full Cuttlefish Native Workflow (Recommended)

Since the compiled binary is natively supervised by Cuttlefish, you can boot the virtual machine and let the system launch the daemon natively:

1. **Start the virtual machine with camera media emulation enabled:**
   ```bash
   cvd start --media=type=v4l2_emulated_camera_mplane,lens_facing=BACK --daemon
   ```
2. **Observe Logs:** Watch the continuous, self-healing stream startup sequence inside `launcher.log`:
   ```bash
   tail -f ~/cuttlefish/instances/cvd-1/logs/launcher.log | grep emulated_camera_mplane
   ```

### Standalone Overrides (Advanced Development)

To launch the daemon manually in the background during active host-side debugging or stand-alone testing:

1. **Locate your active Cuttlefish media socket path:**
   ```bash
   SOCKET_PATH=$(find /var/tmp/cvd/ -name "media_0.sock")
   ```

2. **Run the backend standalone feeding custom video:**
   ```bash
   ./bazel-bin/cuttlefish/host/commands/vhost_user_media/emulated_camera_mplane/emulated_camera_mplane_binary \
     --socket-path "$SOCKET_PATH" \
     --lens-facing BACK \
     --media-file ~/dataset.mp4
   ```

## Compilation

Build the backend binary with the Bazel toolchain:

```bash
bazel build //cuttlefish/host/commands/vhost_user_media/emulated_camera_mplane:emulated_camera_mplane_binary
```

## Architectural Diagram

```
   +-------------------------------------------------------------+
   |                        ANDROID GUEST                        |
   |                                                             |
   |  +--------------------+             +--------------------+  |
   |  | com.android.camera |             |   cameraserver     |  |
   |  +---------+----------+             +---------+----------+  |
   |            |                                  |             |
   |            v                                  v             |
   |  +--------------------------------------------+----------+  |
   |  |       ExternalCameraDevice (HAL Service)              |  |
   |  |   * Re-opens /dev/video1 on session/API resets        |  |
   |  +----------------------------+-------------------------+   |
   |                               |                             |
   |                               v                             |
   |                 +---------------------------+               |
   |                 | /dev/video1 (virtio-media)|               |
   |                 +-------------+-------------+               |
   +-------------------------------|-----------------------------+
                                   | (Virtio PCI / Socket Transport)
                                   v
   +-------------------------------------------------------------+
   |                         HOST SYSTEM                         |
   |                                                             |
   |  +-------------------------------------------------------+  |
   |  |          emulated_camera_mplane_binary                |  |
   |  |                                                       |  |
   |  |  +-------------------+        +--------------------+  |  |
   |  |  |   MmapManager     |        |   Lazy-Starter     |  |  |
   |  |  | * Safe unmap fns  |        | * ensure_started() |  |  |
   |  |  +---------+---------+        +---------+----------+  |  |
   |  |            |                            |             |  |
   |  +------------|----------------------------|-------------+  |
   |               |                            |                |
   |               | (YUV frame delivery)       v (Spawn once)   |
   |               |                  +------------------+       |
   |               +------------------+       FFmpeg     |       |
   |                                  |   -stream_loop -1|       |
   |                                  +------------------+       |
   +-------------------------------------------------------------+
```
