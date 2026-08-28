# Design Review: Synchronized MP4 Camera & Sensor Playback for Cuttlefish (`playbackd`)

**Document reviewed:** `cuttlefish_mp4_synchronized_camera_sensor_playback_design.md`
(changyeon-jo/documents @ `a35216c`)
**Verified against:** `google/android-cuttlefish` `main` @ `b1c733f` (2026-08-28) and
[chromeos/virtio-media PR #31](https://github.com/chromeos/virtio-media/pull/31) (open, unmerged as of 2026-08-28)
**Review date:** 2026-08-28

---

## 1. Overall Assessment

The architecture direction is sound: host-side demux/decode, reuse of the two existing
virtio transport paths (virtio-media for camera, virtio-console for sensors), and a
two-phase delivery that separates plumbing from timing. The **camera half of the design is
accurate against today's upstream** — `v4l2_stream_proxy`, the `--media` flag grammar, and
the host-side `CLOCK_MONOTONIC` stamping the clock analysis is built on all check out
against `main`.

The design is **not ready for implementation as written**, for three blocking reasons:

1. The sensor injection path targets a wire protocol and a FIFO ownership model that do
   not match the codebase (§3 below).
2. The pacing algorithm conflates two unrelated timestamp domains and can stall or
   collapse; the recording-time alignment between video and IMU tracks — the actual hard
   problem of "synchronized playback" — is never specified (§4).
3. Phase 2's cornerstone (`virtio_media.timestamp_mode`) depends on an unmerged
   out-of-tree kernel patch that is not tracked as a dependency (§5.4).

Sections below are ordered: what was verified correct, then blockers, then major issues,
then minor issues and gaps, then a consolidated list of requested revisions.

---

## 2. Verified Accurate (no change requested)

The following claims were checked against upstream `main` @ `b1c733f` and are correct:

| Doc claim | Verified against |
| :--- | :--- |
| `v4l2_stream_proxy` FIFO-fed vhost-user daemon exists | `base/cvd/cuttlefish/host/commands/vhost_user_media/v4l2_stream_proxy/` (with README documenting the FIFO + ffmpeg workflow) |
| `--media=v4l2_stream_proxy:input_path=...:input_width=...:input_height=...:input_fps=...` grammar (§5.2) | `host/libs/config/media.cpp` (`kMediaTypeV4l2Stream`, colon-separated parser, required `input_*` keys) |
| `run_cvd` spawns the proxy with `--input_path/--input_width/--input_height/--input_fps` | `host/commands/run_cvd/launch/vhost_user_media_devices.cpp` |
| Host-side `CLOCK_MONOTONIC` stamping of dequeued buffers (§6.1) | `v4l2_stream_proxy/src/device.rs` ≈L173–177 |
| `/dev/hvc19` = sensors data channel, backed by `sensors_data_fifo_vm.{in,out}` | `host/libs/vm_manager/crosvm_manager.cpp` ≈L984–987; `vm_manager.h` |
| `timestamp_mode` modes 0/1/2 semantics (§6.2) | chromeos/virtio-media PR #31 (matches the described passthrough / guest-stamp / dynamic-translation behavior) |
| The ~190 s boot-delta frame-drop failure mode and why `ptp_kvm` can't fix `CLOCK_MONOTONIC` (§6.1) | Consistent with PR #31's own problem statement and the External Camera HAL staleness check |

The two-phase strategy, the choice of transports, and the §6 clock-domain analysis are the
strongest parts of the document.

---

## 3. BLOCKER — Sensor injection path does not match the codebase

### 3.1 The wire protocol in §3.3 is wrong

§3.3 specifies bare ASCII lines (`acceleration <x> <y> <z>\n`) written to
`sensors_data_fifo_vm.in`. In reality the hvc19 data channel carries a **framed binary
transport**, not raw lines:

- Messages are `transport::RawMessage`: a header with a 31-bit `command`, an
  `is_response` bit, and `payload_size`, followed by the payload
  (`common/libs/transport/channel.h:34`).
- Payloads are built by the host as
  `SensorIdToName(id) + INNER_DELIM + <value-report> + END_OF_MSG` and sent via
  `transport::SharedFdChannel::SendResponse`
  (`host/commands/sensors_simulator/sensors_hal_proxy.cpp`, `UpdateSensorsHal`,
  ≈L110–131).

Bytes written as bare ASCII would be parsed as a garbage message header and permanently
desynchronize the channel. Consequently the §7.3 verification step
`adb shell cat /dev/hvc19 | head` is doubly wrong: the stream is not line-formatted, and
`cat` would steal bytes from the HAL's channel and corrupt framing for the real consumer.

**Requested change:** rewrite §3.3 to describe the real framed protocol, and replace the
§7.3 step with a check that does not consume from the channel (e.g.
`dumpsys sensorservice` rates plus HAL logcat).

### 3.2 `playbackd` cannot write to `sensors_data_fifo_vm.in` — the FIFO is owned

`run_cvd` creates the sensor FIFOs and passes the guest-facing write end to the
`sensors_simulator` process (`host/commands/run_cvd/launch/sensors_simulator.cpp`
≈L43–59, `--data_to_guest_fd=`). That process:

- streams **all** continuous-mode sensors (accel, gyro, magnetic, pressure, light, …) to
  the guest from its own thread every `kIntervalMs = 1000 ms`
  (`sensors_hal_proxy.cpp:30`, streamer thread ≈L189–193);
- gates streaming on the HAL handshake (`list-sensors` → `hal_activated`);
- monitors kernel-log events to survive guest reboots (reboot monitor thread).

A second writer on the same FIFO interleaves bytes mid-message and corrupts the framing.
Even if the race were won, `sensors_simulator` would keep pushing its own synthetic
accel/gyro (derived from the WebRTC UI rotation state via `SetMotion`) concurrently with
the dataset — two sources fighting over the same sensors.

**Requested change (largest revision):** re-draw §2 and §5 so that sensor injection goes
**through `sensors_simulator`**, not around it. Concretely, one of:

- **(Recommended)** Add an injection API to `sensors_simulator` — e.g. a new `SensorsCmd`
  on its existing command channel (the WebRTC socket already demonstrates the pattern
  with `kUpdateRotationVec`), or a dedicated local socket — carrying raw IMU samples.
  Add a playback mode that suppresses the 1 Hz synthetic streamer for the injected
  sensor IDs while preserving (a) the `list-sensors` handshake, (b) synthetic streaming
  for non-injected sensors (magnetometer, pressure, light — the guest HAL expects the
  full mask), and (c) reboot-monitor behavior.
- Alternatively, a `--playback` flag that swaps the data source of the existing streamer.

This also resolves a question the doc never asks: **what happens to the other sensors in
the enabled mask during playback.** The guest HAL is told the full `HostEnabledSensors`
mask at handshake; silently starving the non-IMU sensors is an untested state.

---

## 4. BLOCKER — Pacing algorithm timestamp handling (§4.2)

### 4.1 Two unrelated clock domains are compared against one base

The loop computes `packet_pts_ns` from **container PTS** for the video track but from
`ExtractProtobufTimestamp(packet)` — the **recorder device's boot-monotonic
`event_timestamp_nanos`** — for the sensor tracks, then subtracts a single
`base_pts_ns` taken from whichever packet arrives first:

- If the first packet is a sensor packet, `base_pts_ns` is enormous (recorder uptime) and
  every video packet computes `relative_pts_ns = 0` → video is blasted unpaced.
- If the first packet is a video packet, `base_pts_ns ≈ 0` and the first sensor packet
  yields a `relative_pts_ns` of roughly the recorder's uptime → the loop sleeps
  effectively forever.

**Requested change:** keep **per-stream bases**, and pace **all** tracks off container PTS
(`packet.pts` rescaled per stream); use the protobuf timestamps only as payload. If
sensor-track container PTS is not trustworthy in ARCore recordings, then the doc must
specify the mapping between container time and the BTrace timestamp domain explicitly —
presumably via the camera metadata Track 1, which §3.1 lists and the rest of the doc never
uses. This recording-time alignment is the core of "synchronized playback" and currently
has zero coverage.

### 4.2 Time-base conversion drops `time_base.num`

`(packet.pts * 1'000'000'000ULL) / time_base.den` is only correct when `num == 1`. Use
`av_rescale_q(packet.pts, stream->time_base, {1, 1000000000})`.

### 4.3 Decode happens after the pacing sleep

The loop sleeps until the target PTS and *then* decodes the H.264 frame and writes it.
Decode latency (milliseconds, variable, resolution-dependent) lands entirely on the video
timeline while sensors are unaffected — a structural inter-stream skew that directly
contradicts the sub-millisecond goal. Decode ahead on a worker thread with a small queue;
the paced thread should only sleep-then-write. (Also note: if recordings can contain
B-frames, PTS is not monotonic in `av_read_frame` order; if the design assumes
baseline/constrained profiles without B-frames, say so.)

### 4.4 `sleep_for` will not deliver the claimed precision

For sub-millisecond pacing at up to 1 kHz sensor cadence (1 ms period), use
`clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME, ...)` against **absolute** deadlines so
scheduling error does not accumulate, and consider RT scheduling (`SCHED_FIFO`) with a
documented fallback. Claims of "sub-microsecond precision" (§6.2 table) should be removed
or backed by measurement.

---

## 5. Major Issues

### 5.1 Frame-boundary discipline on the video FIFO (§4.4 is unsafe as written)

Verified against `v4l2_stream_proxy/src/worker.rs` (`handle_streaming`, ≈L154–213) and
`device.rs` (`Format`, ≈L55–110):

- The only supported format is **`YUV420M`** (fourcc `YM12`, **three-planar**). The
  worker fills plane-sized buffers by **byte count alone** — there is no frame marker and
  no resync mechanism. §3.2's "YUV420p" should be corrected to the exact three-plane
  packed layout and sizes (`plane_sizes(w, h)`).
- Therefore §4.4's "non-blocking writes, drop late frames" is unsafe: a partial `write()`
  followed by a drop shifts every subsequent frame's bytes until the FIFO is closed and
  reopened. Default pipe capacity (64 KiB) is far below a frame (640×480 YUV420 ≈
  460 KiB), so partial non-blocking writes are the *normal* case, not the edge case.
- When the guest stops the camera stream, the proxy closes its read end and later
  reopens (`FIFO EOF ... Re-opening` path in `handle_unopened`). `playbackd` will get
  `EPIPE` mid-stream and must reopen and resume **at a frame boundary**. (The repo's own
  `tools/testutils/ffmpeg_v4l2_stream_proxy.sh` solves this by restarting ffmpeg in a
  loop; §4.4 needs the in-process equivalent.)

**Requested change:** specify all-or-nothing per-frame writes (raise pipe capacity with
`F_SETPIPE_SZ` to ≥ one frame; write a frame either completely or not at all), and an
explicit `EPIPE` → reopen → frame-aligned-resume state machine.

### 5.2 `timestamp_mode` is an unmerged, untracked dependency

chromeos/virtio-media PR #31 exists and matches §6.2's description, but it is **open and
unmerged**, and virtio-media is an out-of-tree guest driver. Phase 2 as written silently
depends on: (a) PR #31 (or a carried patch) landing, and (b) the Cuttlefish guest kernel
actually building virtio-media with it. Neither is listed as a dependency or has an owner.

**Requested change:** add a dependencies section naming both, with the fallback path
called out: **Mode 1 (guest-local stamping)** requires no kernel offset tracking and — by
the doc's own analysis — works well precisely because `playbackd` paces frames in real
time. Mode 1 should be the Phase-1/early-Phase-2 default, with Mode 2 as the refinement.

### 5.3 Sensor timestamp fidelity: "zero A/V/IMU drift" (§6.3) is asserted, not designed

The guest sensor HAL timestamps events **at arrival**. Host pacing jitter, FIFO
scheduling, virtio-console batching, and guest scheduling therefore all become IMU
*timestamp noise* — at 200–1000 Hz, exactly where EKF/VIO consumers are least tolerant.
The existing path was built for 1 Hz UI-driven updates; nothing in the doc establishes it
sustains three orders of magnitude more traffic with acceptable jitter.

**Requested change:** either (a) add a measured jitter/throughput budget with a benchmark
gate between Phase 1 and Phase 2 (e.g. "p99 inter-sample timestamp error < X µs at
500 Hz"), or (b) — stronger, and symmetric with camera Mode 2 — extend the injection
protocol to carry the recorded timestamps in-band and translate them host→guest, so the
HAL does not re-stamp at arrival. Replace "zero drift" with the actual expected bound.

### 5.4 Backpressure policy contradicts the sync goal

§4.4 drops late video frames so as not to block the IMU pipeline — but the v4l2 proxy
consumes at the guest's dequeue rate. If the guest consumes slower than the recording FPS,
frames are silently dropped with no accounting, while sensors continue at full rate; the
"synchronized" streams then diverge in *content* even if each is well-paced. Specify the
policy: drop-with-counter (and log), or pause-both, or pace-to-consumer; and surface drop
counts in Phase-1 verification.

---

## 6. Minor Issues and Gaps

1. **GPS (Track 5)** appears in the requirements (§1.1, §3.1) and then vanishes.
   Cuttlefish already has GNSS plumbing (`/dev/hvc6`/`hvc7`, `gnss_grpc_proxy`,
   `cvd_update_location`) — either scope it into a phase or explicitly defer it.
2. **Camera metadata Track 1** (exposure, rolling shutter) is specced and never consumed.
   The virtio-media path cannot inject per-frame exposure metadata into the HAL; state
   the limitation, since ARCore consumes intrinsics/exposure.
3. **Paths (§5.1):** the sensor FIFOs live under `PerInstanceInternalPath` — i.e.
   `.../instances/cvd-1/internal/sensors_data_fifo_vm.in`, not the paths shown; they are
   created mode 0660 by `run_cvd`. Also state which user `playbackd` runs as, and whether
   `process_sandboxer` policies need entries if it becomes a launched host service.
4. **HAL naming:** `android.hardware.camera.provider@2.4` / `sensors@2.1` are HIDL-era
   names; current Cuttlefish is on AIDL HALs. Cosmetic, but erodes confidence in the
   guest-side analysis.
5. **Verification plan (§7):** Stage 1 and Stage 4 rely on Google-internal tooling
   (`verify_dataset.py` in a citc client, google3 diagnostic app, `go/` links,
   `~/Forest` paths). If the doc targets the public repo, provide public equivalents:
   `ffprobe -show_streams` for Stage 1; a Camera2 sample app + `dumpsys sensorservice`
   for Stage 4. Fix §7.3 per §3.1 above.
6. **Loop/rewind (§4.3):** pacing reset is covered, but not what the guest observes — a
   hard content discontinuity that a VIO tracker will reject, and (under Mode 2) a
   timestamp stream that keeps increasing across a content jump. Fine for looped soak
   testing; say so, and note trackers must be restarted per loop for accuracy testing.
7. **API hygiene:** `av_init_packet` is deprecated — use `av_packet_alloc`/`av_packet_free`.
   `UnpackFixed32` via `memcpy` is correct; `std::bit_cast` (C++20) is the tidier spelling.
8. **Alternatives considered — section is missing entirely.** At minimum discuss:
   (a) `v4l2loopback` + the existing `--media=v4l2_proxy` (crosvm `--v4l2-proxy`)
   instead of a new FIFO writer; (b) injecting sensors via the existing WebRTC command
   socket into `sensors_simulator`. Rejecting them with reasons will strengthen the doc.
9. **Build/packaging:** `playbackd` introduces FFmpeg (`libavformat/avcodec/swscale`) and
   protobuf as host dependencies. Where does it build (Bazel target in-tree? separate
   repo?), and does it ship in the Debian packages? FFmpeg linkage has licensing
   implications (LGPL config vs GPL components) that need a sentence.
10. **Doc hygiene for upstreaming:** internal references (`go/...`, google3 paths,
    `/usr/local/google/home/...`, `~/Forest/...` file:// links) will not resolve for
    external readers; move them to an appendix or replace with public links.

---

## 7. Consolidated Requested Revisions

| # | Severity | Revision |
| :-- | :-- | :-- |
| 1 | Blocker | Redesign sensor injection to go **through `sensors_simulator`** (injection API + playback mode); rewrite §3.3 for the framed `RawMessage` protocol; fix §7.3 |
| 2 | Blocker | Fix §4.2: per-stream time bases, `av_rescale_q`, pace on container PTS; specify recording-time alignment between video PTS and BTrace timestamps (Track 1?) |
| 3 | Blocker | Add a dependencies section: virtio-media PR #31 (unmerged) + guest kernel adoption; promote Mode 1 as the default until Mode 2 lands |
| 4 | Major | §4.4: atomic per-frame FIFO writes (`F_SETPIPE_SZ`), `EPIPE`/reopen frame-aligned resync; correct §3.2 to three-planar `YUV420M` |
| 5 | Major | Decode-ahead worker thread; absolute-deadline `clock_nanosleep`; remove unbacked "sub-microsecond" claims |
| 6 | Major | Replace "zero A/V/IMU drift" with a measured jitter budget for 200–1000 Hz over virtio-console, or carry timestamps in-band |
| 7 | Major | Define the backpressure/drop accounting policy for slow guest consumers |
| 8 | Minor | GPS scope decision; Track 1 usage/limitation; correct paths; AIDL HAL names; public verification steps; alternatives-considered; packaging/licensing; strip internal links |

## 8. Suggested Phase Gate Adjustments

- **Phase 1 exit criteria** should additionally include: frame-aligned recovery after a
  guest camera stop/start cycle (EPIPE path), zero sensor-channel framing errors across a
  guest reboot (reboot-monitor cooperation), and drop counters exposed.
- **Phase 2 entry** should be gated on the sensor-jitter benchmark (§5.3) so the
  synchronization work targets a measured, not assumed, noise floor.

---

*The camera-side plumbing and the clock-domain analysis are solid and verified against
upstream `main`. The revision effort should concentrate on the sensor path (integration
point + protocol) and on making the pacing algorithm's timestamp story explicit.*
