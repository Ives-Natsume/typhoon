use crossbeam_channel::{bounded, Sender, Receiver};
use windows_capture::{
    capture::{CaptureControl, Context, GraphicsCaptureApiHandler},
    // encoder::ImageFormat,
    frame::Frame,
    graphics_capture_api::InternalCaptureControl,
    settings::{
        ColorFormat, CursorCaptureSettings, DrawBorderSettings,
        MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
        DirtyRegionSettings
    },
};
use std::{
    io::Write, path::{Path, PathBuf}, process::{Command, Stdio}, sync::Arc, thread::JoinHandle, time::{Duration, Instant}
};
use crate::{
    backend::capture::{
        audio::{AudioRecorder, AudioTrack},
        window::window_detect,
    },
    util::config,
};

#[derive(Clone)]
pub struct FrameChunk {
    pub data: Arc<[u8]>,
    pub width: u32,
    pub height: u32,
    pub timestamp: Instant,
}

type CaptureError = Box<dyn std::error::Error + Send + Sync>;

/// What the encoder thread produces once the video side has been finalized.
struct VideoOutput {
    /// Video-only file (no audio track yet).
    path: PathBuf,
    /// Wall-clock instant that presentation time 0 of `path` corresponds to.
    start: Instant,
}

pub struct Recorder {
    shutdown_tx: Sender<()>,
    capture_control: CaptureControl<CaptureHandler, CaptureError>,
    encoder_handle: JoinHandle<Option<VideoOutput>>,
    audio: Option<AudioRecorder>,
    final_output: PathBuf,
}

impl Recorder {
    pub async fn start() -> anyhow::Result<Self> {
        let config = config::read_config().await;
        let output_dir = config.capture.output_dir.clone();
        let fps = config.capture.fps;
        let record_duration = config.capture.duration;
        let duration_nanos = Duration::from_nanos(1_000_000_000 / fps as u64);
        let window_title = config.general.capture_target_window_title.clone();

        // Ring buffer between the capture producer and the FFmpeg consumer.
        // It only needs to absorb short-term timing jitter (a couple of
        // seconds), NOT the whole recording: each 1080p RGBA frame is ~8 MB,
        // so sizing it to `duration * fps` would waste gigabytes of RAM.
        let buffer_size = (fps as usize * 2).max(60);

        let (capture_tx, capture_rx) = bounded::<FrameChunk>(buffer_size);
        let (shutdown_tx, shutdown_rx) = bounded::<()>(1);

        // Make sure the output directory exists. The rolling segment files and
        // the final merged video are produced by the encoder thread.
        std::fs::create_dir_all(&output_dir).ok();

        let detect_result = window_detect(&window_title);
        let target_window = match detect_result {
            Ok(win) => win,
            Err(e) => {
                return Err(anyhow::anyhow!("Failed to detect window: {}", e));
            }
        };

        // WASAPI process loopback is selected by PID, not HWND. The window we
        // capture video from gives us the PID for free, so both halves of the
        // recording target the exact same process tree.
        let target_pid = match target_window.process_id() {
            Ok(pid) => Some(pid),
            Err(e) => {
                tracing::warn!(error = %e, "could not resolve target PID; audio will use global loopback");
                None
            }
        };

        // Start audio BEFORE video: audio needs a moment to activate the WASAPI
        // client, and a slightly early audio origin is trimmed during alignment
        // whereas a late one costs us the first frames of sound.
        let audio = if config.capture.audio {
            match AudioRecorder::start(target_pid, record_duration) {
                Ok(rec) => Some(rec),
                Err(e) => {
                    tracing::error!(error = %e, "audio capture disabled: failed to start");
                    None
                }
            }
        } else {
            None
        };

        let settings = Settings::new(
            target_window,
            // Default cursor capture settings (capture the cursor)
            CursorCaptureSettings::Default,
            // Default draw border settings (do not draw borders)
            DrawBorderSettings::Default,
            // Default secondary window settings (capture only the primary window)
            SecondaryWindowSettings::Default,
            // Throttle the capture to the configured FPS
            MinimumUpdateIntervalSettings::Custom(duration_nanos),
            // Default dirty region settings (capture the entire window)
            DirtyRegionSettings::Default,
            // RGBA8 color format
            ColorFormat::Rgba8,
            capture_tx,
        );

        let final_output = Path::new(&output_dir).join("recording.mp4");

        let encoder_handle = std::thread::spawn(move || {
            match ffmpeg_encoder_thread(capture_rx, shutdown_rx, &output_dir, fps, record_duration) {
                Ok(out) => out,
                Err(e) => {
                    tracing::error!("FFmpeg encoder thread error: {}", e);
                    None
                }
            }
        });

        let capture_control = CaptureHandler::start_free_threaded(settings)
            .map_err(|e| anyhow::anyhow!("Failed to start capture: {}", e))?;

        Ok(Self {
            shutdown_tx,
            capture_control,
            encoder_handle,
            audio,
            final_output,
        })
    }

    pub fn stop(self) -> anyhow::Result<()> {
        // Stop the producer first so no new frames are queued, then let the
        // encoder drain whatever is still buffered before finalizing the file.
        self.capture_control
            .stop()
            .map_err(|e| anyhow::anyhow!("Failed to stop capture: {}", e))?;

        // Backstop in case the channel never disconnects on its own.
        let _ = self.shutdown_tx.send(());

        // Wait for FFmpeg to flush and write the MP4 trailer.
        let video = self
            .encoder_handle
            .join()
            .map_err(|_| anyhow::anyhow!("Encoder thread panicked"))?;

        // Stop audio only after the video side is finalized so the retained
        // audio window covers the very end of the footage too.
        let audio_track = self.audio.and_then(|a| a.stop());

        match (video, audio_track) {
            (Some(video), Some(track)) => {
                if let Err(e) = mux_audio(&video, &track, &self.final_output) {
                    tracing::error!("Failed to mux audio: {}; keeping video-only output", e);
                    promote_video_only(&video.path, &self.final_output);
                }
            }
            (Some(video), None) => promote_video_only(&video.path, &self.final_output),
            (None, _) => tracing::warn!("No video was produced; nothing to finalize"),
        }

        Ok(())
    }
}

/// Moves the video-only file into place when there is no audio to merge.
fn promote_video_only(video: &Path, final_output: &Path) {
    if video == final_output {
        return;
    }
    let _ = std::fs::remove_file(final_output);
    if let Err(e) = std::fs::rename(video, final_output) {
        tracing::error!("Failed to move {} to {}: {}", video.display(), final_output.display(), e);
    } else {
        tracing::info!("Recording written to {}", final_output.display());
    }
}

/// Merges the captured audio track into the video, aligned on the shared
/// monotonic clock.
///
/// The alignment itself happens while writing the WAV (silence padding /
/// trimming in the sample domain), so ffmpeg only has to mux two streams that
/// already start at the same instant. Video is stream-copied; only audio is
/// encoded.
fn mux_audio(video: &VideoOutput, track: &AudioTrack, final_output: &Path) -> anyhow::Result<()> {
    let wav_path = video.path.with_extension("wav");
    track.write_wav_aligned(&wav_path, video.start)?;

    let tmp_output = final_output.with_extension("muxing.mp4");
    let result = Command::new("ffmpeg")
        .args([
            "-y",
            "-i", video.path.to_string_lossy().as_ref(),
            "-i", wav_path.to_string_lossy().as_ref(),
            "-map", "0:v:0",
            "-map", "1:a:0",
            "-c:v", "copy",
            "-c:a", "aac",
            "-b:a", "192k",
            // Both inputs already share t=0; cut at whichever ends first so the
            // trailing silence/black does not extend the file.
            "-shortest",
            "-movflags", "+faststart",
            tmp_output.to_string_lossy().as_ref(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()?;

    let _ = std::fs::remove_file(&wav_path);

    if !result.status.success() {
        let stderr = String::from_utf8_lossy(&result.stderr);
        let _ = std::fs::remove_file(&tmp_output);
        return Err(anyhow::anyhow!(
            "ffmpeg mux failed ({}): {}",
            result.status,
            stderr.lines().last().unwrap_or("unknown error")
        ));
    }

    let _ = std::fs::remove_file(final_output);
    std::fs::rename(&tmp_output, final_output)?;
    let _ = std::fs::remove_file(&video.path);
    tracing::info!(
        audio_secs = track.duration().as_secs_f32(),
        "Recording with audio written to {}",
        final_output.display()
    );
    Ok(())
}

struct CaptureHandler {
    capture_tx: Sender<FrameChunk>,
}

impl GraphicsCaptureApiHandler for CaptureHandler {
    type Flags = Sender<FrameChunk>;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self { capture_tx: ctx.flags })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        _capture_control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        let mut buffer = frame.buffer()?;
        let chunk = FrameChunk {
            data: Arc::from(buffer.as_raw_buffer().to_vec().into_boxed_slice()),
            width: frame.width(),
            height: frame.height(),
            timestamp: Instant::now(),
        };

        match self.capture_tx.try_send(chunk) {
            Ok(()) => {}
            Err(crossbeam_channel::TrySendError::Full(_)) => {
                tracing::warn!("Ring buffer full, dropping frame");
            }
            Err(e) => return Err(e.into()),
        }

        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        tracing::info!("Capture session closed");
        Ok(())
    }
}

fn ffmpeg_encoder_thread(
    rx: Receiver<FrameChunk>,
    shutdown_rx: Receiver<()>,
    output_dir: &str,
    fps: u32,
    duration_secs: u32,
) -> anyhow::Result<Option<VideoOutput>> {
    // Block until the first frame so the real capture dimensions are known.
    let first_frame = match rx.recv() {
        Ok(frame) => frame,
        Err(_) => {
            tracing::warn!("Capture ended before any frame arrived; nothing to encode");
            return Ok(None);
        }
    };
    let width = first_frame.width;
    let height = first_frame.height;
    // Wall-clock time of the first encoded frame. Frames are fed to ffmpeg at a
    // constant `-r`, so presentation time 0 of the merged video corresponds to
    // this instant (adjusted below for the segments dropped by pruning).
    let first_frame_instant = first_frame.timestamp;

    // Pre-record ring buffer parameters.
    //
    // Instead of writing one ever-growing file, FFmpeg splits the stream into
    // fixed-length segments on disk. Only enough segments to cover roughly
    // `duration_secs` are kept; older ones are deleted as new ones appear. On
    // shutdown the surviving segments are concatenated (in order) into a single
    // output video. This bounds the footage to the most recent `duration_secs`
    // seconds no matter how long the program runs.
    let segment_seconds = duration_secs.div_ceil(10).max(1); // aim for ~10 segments
    let keep_segments = duration_secs.div_ceil(segment_seconds) as usize + 1;

    let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S").to_string();
    let segments_dir = Path::new(output_dir).join(format!(".segments_{timestamp}"));
    std::fs::create_dir_all(&segments_dir)?;
    let segment_pattern = segments_dir.join("segment_%05d.mp4");
    // Video-only intermediate; `Recorder::stop` either muxes audio into the
    // real output or renames this file into place.
    let video_only = Path::new(output_dir).join(format!(".video_{timestamp}.mp4"));

    let mut ffmpeg = Command::new("ffmpeg")
        .args([
            "-y",
            "-f", "rawvideo",
            "-vcodec", "rawvideo",
            "-pix_fmt", "rgba",
            "-s", &format!("{width}x{height}"),
            "-r", &fps.to_string(),
            "-thread_queue_size", "512", // input buffer size
            "-i", "-",                   // read from stdin
            "-c:v", "libx264",
            "-pix_fmt", "yuv420p",
            "-preset", "ultrafast",
            "-tune", "zerolatency",
            "-crf", "23",
            // Force a keyframe at every segment boundary so each segment is
            // independently decodable and can later be concatenated losslessly
            // with "-c copy".
            "-force_key_frames", &format!("expr:gte(t,n_forced*{segment_seconds})"),
            "-f", "segment",
            "-segment_time", &segment_seconds.to_string(),
            "-segment_format", "mp4",
            "-reset_timestamps", "1",
            segment_pattern.to_string_lossy().as_ref(),
        ])
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let mut stdin = ffmpeg.stdin.take().unwrap();
    let stderr = ffmpeg.stderr.take().unwrap();

    // stderr reading thread (for debugging FFmpeg output)
    let stderr_thread = std::thread::spawn(move || {
        use std::io::{BufRead, BufReader};
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            tracing::debug!("[FFmpeg] {}", line);
        }
    });

    // Write frames to FFmpeg as fast as they arrive. The capture side is
    // already throttled to the target FPS and FFmpeg paces the output via the
    // "-r" flag, so the consumer must NOT sleep here: throttling the consumer
    // would back the ring buffer up and drop frames.
    //
    // CONSTANT FRAME RATE PACING (required for A/V sync)
    //
    // The Windows capture API only emits a frame when the window actually
    // changes, so a mostly-static screen yields FEWER frames than `fps *
    // elapsed`. Because ffmpeg is told `-r <fps>`, every frame we hand over
    // occupies exactly 1/fps of video timeline: feeding it 27 fps worth of
    // frames while 30 fps worth of wall-clock time passes makes the video
    // timeline drift SLOWER than reality (a 1195 s session became 1079 s of
    // video, i.e. ~10% time compression).
    //
    // That breaks two things at once: playback speed, and any attempt to map a
    // video timestamp back to a wall-clock instant (which is what audio
    // alignment needs). So we duplicate the previous frame into every slot the
    // capture API skipped, turning the stream into true CFR anchored to the
    // wall clock. Duplicated frames of a static screen cost x264 almost nothing
    // (they encode as near-empty P-frames).
    let mut frame_count: u64 = 0;
    let mut duplicated: u64 = 0;
    let mut last_cleanup = Instant::now();
    let mut last_data: Arc<[u8]> = first_frame.data.clone();

    // How many frame slots should be filled by the time `t` is reached.
    let slot_at = |t: Instant| -> u64 {
        (t.saturating_duration_since(first_frame_instant).as_secs_f64() * fps as f64).round() as u64
    };

    match stdin.write_all(&first_frame.data) {
        Ok(()) => {
            frame_count += 1;
            loop {
                match rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(chunk) => {
                        // Fill the gap between the last written slot and this
                        // frame's real arrival time, then write the frame.
                        match pad_frames(&mut stdin, &last_data, &mut frame_count, slot_at(chunk.timestamp)) {
                            Ok(n) => duplicated += n,
                            Err(e) => {
                                tracing::error!("Failed to pad frames to FFmpeg: {}", e);
                                break;
                            }
                        }

                        if let Err(e) = stdin.write_all(&chunk.data) {
                            tracing::error!("Failed to write frame to FFmpeg: {}", e);
                            break;
                        }
                        frame_count += 1;
                        last_data = chunk.data;

                        // flush every 30 frames
                        if frame_count % 30 == 0 {
                            let _ = stdin.flush();
                        }
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                        // Nothing changed on screen; keep the video timeline
                        // moving so it stays locked to the wall clock.
                        match pad_frames(&mut stdin, &last_data, &mut frame_count, slot_at(Instant::now())) {
                            Ok(n) => duplicated += n,
                            Err(e) => {
                                tracing::error!("Failed to pad frames to FFmpeg: {}", e);
                                break;
                            }
                        }
                        let _ = stdin.flush();

                        if shutdown_rx.try_recv().is_ok() {
                            tracing::info!("Shutdown signal received, finalizing recording");
                            break;
                        }
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                        tracing::info!("Capture channel disconnected, finalizing recording");
                        break;
                    }
                }

                // Periodically drop stale segments so disk usage stays bounded
                // to roughly the last `duration_secs` seconds.
                if last_cleanup.elapsed() >= Duration::from_secs(1) {
                    if let Err(e) = prune_old_segments(&segments_dir, keep_segments) {
                        tracing::warn!("Failed to prune old segments: {}", e);
                    }
                    last_cleanup = Instant::now();
                }
            }
        }
        Err(e) => {
            tracing::error!("FFmpeg closed its input immediately: {}", e);
        }
    }

    drop(stdin); // EOF: FFmpeg flushes buffers and finalizes the last segment.
    let status = ffmpeg.wait()?;
    let _ = stderr_thread.join();
    tracing::info!(
        "FFmpeg segmenting finished: {} ({} frames, {} duplicated for CFR, {:.1}s of video)",
        status,
        frame_count,
        duplicated,
        frame_count as f64 / fps as f64
    );

    // Trim once more so the retained footage stays close to `duration_secs`.
    if let Err(e) = prune_old_segments(&segments_dir, keep_segments) {
        tracing::warn!("Failed to prune old segments: {}", e);
    }

    // Pruning drops the oldest segments, so the merged video no longer starts at
    // the first captured frame. The surviving set begins at
    // `first_segment_index * segment_seconds` on the original timeline, which is
    // exactly the shift the audio track must be aligned against.
    let first_index = list_segments(&segments_dir)
        .map(|s| s.first().map(|(i, _)| *i).unwrap_or(0))
        .unwrap_or(0);
    let video_start =
        first_frame_instant + Duration::from_secs(first_index as u64 * segment_seconds as u64);

    // Merge the surviving segments (in chronological order) into the final
    // output file, then remove the temporary segment directory.
    let merged = match concat_segments(&segments_dir, &video_only) {
        Ok(0) => {
            tracing::warn!("No segments were produced; nothing to merge");
            false
        }
        Ok(n) => {
            tracing::info!("Merged {} segment(s) into {}", n, video_only.display());
            true
        }
        Err(e) => {
            tracing::error!("Failed to merge segments: {}", e);
            false
        }
    };

    if let Err(e) = std::fs::remove_dir_all(&segments_dir) {
        tracing::warn!("Failed to remove temporary segment directory: {}", e);
    }

    Ok(merged.then_some(VideoOutput {
        path: video_only,
        start: video_start,
    }))
}

/// Repeats `last` until `frame_count` reaches `target_slot`, keeping the video
/// timeline locked to the wall clock when the capture API emits no frames.
///
/// Returns how many duplicate frames were written. The per-call fill is capped
/// so a long stall (window minimised, machine suspended) cannot blow up into a
/// huge synchronous write burst.
fn pad_frames(
    stdin: &mut impl Write,
    last: &[u8],
    frame_count: &mut u64,
    target_slot: u64,
) -> std::io::Result<u64> {
    if target_slot <= *frame_count {
        return Ok(0);
    }
    let missing = (target_slot - *frame_count).min(600); // <= 20 s at 30 fps
    for _ in 0..missing {
        stdin.write_all(last)?;
        *frame_count += 1;
    }
    Ok(missing)
}

/// Lists the segment files in `dir`, sorted ascending by their numeric index
/// (which is also chronological order).
fn list_segments(dir: &Path) -> std::io::Result<Vec<(u32, PathBuf)>> {
    let mut segments = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("mp4") {
            continue;
        }
        let index = path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.strip_prefix("segment_"))
            .and_then(|n| n.parse::<u32>().ok());
        if let Some(index) = index {
            segments.push((index, path));
        }
    }
    segments.sort_by_key(|(index, _)| *index);
    Ok(segments)
}

/// Deletes the oldest segments, keeping only the newest `keep` files.
///
/// The highest-indexed segment (the one FFmpeg is currently writing) is always
/// part of the kept set, so it is never removed while still in use.
fn prune_old_segments(dir: &Path, keep: usize) -> std::io::Result<()> {
    let segments = list_segments(dir)?;
    if segments.len() <= keep {
        return Ok(());
    }
    let remove_count = segments.len() - keep;
    for (_, path) in segments.into_iter().take(remove_count) {
        if let Err(e) = std::fs::remove_file(&path) {
            tracing::warn!("Failed to delete stale segment {}: {}", path.display(), e);
        }
    }
    Ok(())
}

/// Concatenates the remaining segments (in order) into `output` using FFmpeg's
/// concat demuxer with stream copy (no re-encode). Returns how many segments
/// were merged.
fn concat_segments(dir: &Path, output: &Path) -> anyhow::Result<usize> {
    let segments = list_segments(dir)?;
    if segments.is_empty() {
        return Ok(0);
    }

    // Build the concat list file the demuxer expects.
    let list_path = dir.join("concat_list.txt");
    let mut list = std::fs::File::create(&list_path)?;
    for (_, path) in &segments {
        // The concat demuxer resolves relative `file` paths against the list
        // file's own directory, not the process CWD, so write absolute paths
        // (with forward slashes) to keep it unambiguous on Windows.
        let abs = std::path::absolute(path).unwrap_or_else(|_| path.clone());
        let p = abs.to_string_lossy().replace('\\', "/");
        writeln!(list, "file '{p}'")?;
    }
    list.flush()?;
    drop(list);

    let result = Command::new("ffmpeg")
        .args([
            "-y",
            "-f", "concat",
            "-safe", "0",
            "-i", list_path.to_string_lossy().as_ref(),
            "-c", "copy",
            "-movflags", "+faststart",
            output.to_string_lossy().as_ref(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()?;

    if !result.status.success() {
        let stderr = String::from_utf8_lossy(&result.stderr);
        return Err(anyhow::anyhow!(
            "ffmpeg concat failed ({}): {}",
            result.status,
            stderr.lines().last().unwrap_or("unknown error")
        ));
    }

    Ok(segments.len())
}