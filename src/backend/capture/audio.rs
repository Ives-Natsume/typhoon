//! Audio capture for the recording pipeline.
//!
//! Uses `flexaudio` to grab a loopback stream and keeps the most recent
//! `keep_secs` seconds of PCM in memory (mirroring the rolling video segment
//! buffer). On stop the retained samples are handed back as an [`AudioTrack`]
//! that carries the wall-clock [`Instant`] of its *first* sample, which is what
//! makes A/V alignment possible later on.
//!
//! # Per-process vs. global capture
//!
//! `flexaudio` exposes Windows' WASAPI *process loopback*
//! (`AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK`, Win10 2004+), which records
//! only the audio rendered by one process tree. It is selected by **PID**, not
//! by `HWND` — the PID comes from `windows-capture`'s `Window::process_id()`, so
//! the very same window we capture video from also determines the audio source.
//!
//! If process loopback is unavailable (older Windows, activation refused, ...)
//! we transparently fall back to system-wide loopback with `exclude_self` so we
//! do not record our own playback.

use std::{
    collections::VecDeque,
    io::Write,
    path::Path,
    thread::JoinHandle,
    time::{Duration, Instant},
};

use crossbeam_channel::{bounded, Sender};
use flexaudio::{
    core::monotonic_now_ns, ChunkFlags, Event, OutputFormat, ProcessMode, SourceKind, StreamConfig,
};

/// Which loopback source the recorder should use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioSource {
    /// Capture only the target process tree (WASAPI process loopback).
    Process(u32),
    /// Capture everything the default render endpoint plays (minus ourselves).
    System,
}

/// Captured PCM plus the wall-clock time of its first sample.
pub struct AudioTrack {
    /// Interleaved signed 16-bit samples.
    pub samples: Vec<i16>,
    pub sample_rate: u32,
    pub channels: u16,
    /// Wall-clock instant that `samples[0]` corresponds to.
    pub start: Instant,
}

impl AudioTrack {
    fn frames(&self) -> usize {
        if self.channels == 0 {
            0
        } else {
            self.samples.len() / self.channels as usize
        }
    }

    /// Duration of the retained audio.
    pub fn duration(&self) -> Duration {
        Duration::from_secs_f64(self.frames() as f64 / self.sample_rate.max(1) as f64)
    }

    /// Writes the track to `path` as a 16-bit PCM WAV file, shifted so that its
    /// first sample lines up with `video_start`.
    ///
    /// This is the whole A/V synchronisation step: both streams are anchored to
    /// the same monotonic clock, so the offset between the two origins is either
    /// padded with silence (audio started late) or trimmed away (audio started
    /// early). Doing it in the sample domain avoids relying on ffmpeg's
    /// `-itsoffset`, which happily produces negative timestamps that later get
    /// dropped by the muxer.
    pub fn write_wav_aligned(&self, path: &Path, video_start: Instant) -> anyhow::Result<()> {
        let channels = self.channels.max(1) as usize;
        let rate = self.sample_rate.max(1);

        // Signed offset of the audio origin relative to the video origin.
        let offset_secs = if self.start >= video_start {
            self.start.duration_since(video_start).as_secs_f64()
        } else {
            -video_start.duration_since(self.start).as_secs_f64()
        };
        let offset_frames = (offset_secs * rate as f64).round() as i64;

        let (lead_silence_samples, skip_samples) = if offset_frames >= 0 {
            ((offset_frames as usize) * channels, 0usize)
        } else {
            (0usize, ((-offset_frames) as usize) * channels)
        };
        let body = if skip_samples >= self.samples.len() {
            &[][..]
        } else {
            &self.samples[skip_samples..]
        };

        tracing::info!(
            offset_ms = (offset_secs * 1000.0).round() as i64,
            lead_silence_ms = (lead_silence_samples / channels) as u64 * 1000 / rate as u64,
            "aligning audio track to video timeline"
        );

        // Sanity guard. A lead silence longer than the audio itself means the
        // two timelines disagree wildly (e.g. the video timeline drifted away
        // from wall-clock time). Left unchecked, the real audio ends up beyond
        // the end of the video and `-shortest` silently discards it, producing a
        // mute file with no error anywhere. Fail loudly instead of silently.
        let lead_secs = (lead_silence_samples / channels) as f64 / rate as f64;
        if lead_secs > self.duration().as_secs_f64() {
            return Err(anyhow::anyhow!(
                "audio/video timelines disagree: {:.1}s of lead silence for only {:.1}s of audio \
                 (the video timeline is probably not locked to wall-clock time)",
                lead_secs,
                self.duration().as_secs_f64()
            ));
        }

        let total_samples = lead_silence_samples + body.len();
        let data_bytes = (total_samples * 2) as u32;

        let mut file = std::io::BufWriter::new(std::fs::File::create(path)?);
        write_wav_header(&mut file, rate, self.channels.max(1), data_bytes)?;
        // Leading silence, chunked so we never allocate the whole pad at once.
        let mut remaining = lead_silence_samples;
        let zeros = [0u8; 4096];
        while remaining > 0 {
            let n = remaining.min(zeros.len() / 2);
            file.write_all(&zeros[..n * 2])?;
            remaining -= n;
        }
        let mut buf = Vec::with_capacity(body.len() * 2);
        for s in body {
            buf.extend_from_slice(&s.to_le_bytes());
        }
        file.write_all(&buf)?;
        file.flush()?;
        Ok(())
    }
}

fn write_wav_header<W: Write>(
    w: &mut W,
    sample_rate: u32,
    channels: u16,
    data_bytes: u32,
) -> std::io::Result<()> {
    let byte_rate = sample_rate * channels as u32 * 2;
    let block_align = channels * 2;
    w.write_all(b"RIFF")?;
    w.write_all(&(36 + data_bytes).to_le_bytes())?;
    w.write_all(b"WAVE")?;
    w.write_all(b"fmt ")?;
    w.write_all(&16u32.to_le_bytes())?; // PCM fmt chunk size
    w.write_all(&1u16.to_le_bytes())?; // WAVE_FORMAT_PCM
    w.write_all(&channels.to_le_bytes())?;
    w.write_all(&sample_rate.to_le_bytes())?;
    w.write_all(&byte_rate.to_le_bytes())?;
    w.write_all(&block_align.to_le_bytes())?;
    w.write_all(&16u16.to_le_bytes())?; // bits per sample
    w.write_all(b"data")?;
    w.write_all(&data_bytes.to_le_bytes())?;
    Ok(())
}

/// Handle to the running audio capture thread.
pub struct AudioRecorder {
    shutdown_tx: Sender<()>,
    handle: JoinHandle<Option<AudioTrack>>,
    source: AudioSource,
}

impl AudioRecorder {
    /// Starts audio capture, preferring per-process loopback for `pid` and
    /// falling back to system loopback when that is not available.
    ///
    /// `keep_secs` bounds the in-memory ring buffer (48 kHz stereo s16 is about
    /// 192 KiB per second, so a 30 s window costs ~5.6 MiB).
    pub fn start(pid: Option<u32>, keep_secs: u32) -> anyhow::Result<Self> {
        let (shutdown_tx, shutdown_rx) = bounded::<()>(1);
        // The `flexaudio::Stream` and its ring consumer live entirely inside the
        // capture thread, so nothing has to be `Send` across the boundary.
        let (ready_tx, ready_rx) = bounded::<Result<AudioSource, String>>(1);

        let keep_secs = keep_secs.max(1);
        let handle = std::thread::Builder::new()
            .name("typhoon-audio".into())
            .spawn(move || audio_thread(pid, keep_secs, ready_tx, shutdown_rx))?;

        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(source)) => {
                match source {
                    AudioSource::Process(pid) => {
                        tracing::info!(pid, "audio capture started (per-process loopback)")
                    }
                    AudioSource::System => {
                        tracing::warn!("audio capture started (global loopback fallback)")
                    }
                }
                Ok(Self {
                    shutdown_tx,
                    handle,
                    source,
                })
            }
            Ok(Err(e)) => {
                let _ = handle.join();
                Err(anyhow::anyhow!("failed to start audio capture: {e}"))
            }
            Err(_) => {
                let _ = shutdown_tx.send(());
                let _ = handle.join();
                Err(anyhow::anyhow!("audio capture did not start within 5s"))
            }
        }
    }

    pub fn source(&self) -> AudioSource {
        self.source
    }

    /// Stops capture and returns whatever audio is still in the ring buffer.
    pub fn stop(self) -> Option<AudioTrack> {
        let _ = self.shutdown_tx.send(());
        match self.handle.join() {
            Ok(track) => track,
            Err(_) => {
                tracing::error!("audio capture thread panicked");
                None
            }
        }
    }
}

/// Builds the two candidate stream configurations (process first, system as a
/// fallback) and returns the first one that actually starts.
fn open_stream(pid: Option<u32>) -> anyhow::Result<(flexaudio::Stream, AudioSource)> {
    let mut last_err: Option<String> = None;

    if let Some(pid) = pid {
        let config = StreamConfig {
            kind: SourceKind::ProcessLoopback,
            target_pid: Some(pid),
            // Include = the target process *tree*, so a game whose audio is
            // rendered by a child/helper process is still covered.
            mode: ProcessMode::Include,
            output: OutputFormat {
                sample_rate: 48_000,
                channels: 2,
            },
            ..Default::default()
        };
        match flexaudio::open(config).and_then(|mut s| s.start().map(|_| s)) {
            Ok(stream) => return Ok((stream, AudioSource::Process(pid))),
            Err(e) => {
                tracing::warn!(pid, error = %e, "process loopback unavailable, falling back to system loopback");
                last_err = Some(e.to_string());
            }
        }
    }

    let config = StreamConfig {
        kind: SourceKind::SystemLoopback,
        // Never record our own playback back into the capture.
        exclude_self: true,
        output: OutputFormat {
            sample_rate: 48_000,
            channels: 2,
        },
        ..Default::default()
    };
    match flexaudio::open(config).and_then(|mut s| s.start().map(|_| s)) {
        Ok(stream) => Ok((stream, AudioSource::System)),
        Err(e) => Err(anyhow::anyhow!(
            "system loopback failed: {e}{}",
            last_err
                .map(|p| format!(" (process loopback earlier failed: {p})"))
                .unwrap_or_default()
        )),
    }
}

fn audio_thread(
    pid: Option<u32>,
    keep_secs: u32,
    ready_tx: Sender<Result<AudioSource, String>>,
    shutdown_rx: crossbeam_channel::Receiver<()>,
) -> Option<AudioTrack> {
    let (mut stream, source) = match open_stream(pid) {
        Ok(v) => v,
        Err(e) => {
            let _ = ready_tx.send(Err(e.to_string()));
            return None;
        }
    };

    let out = stream.config().output;
    let sample_rate = out.sample_rate;
    let channels = out.channels.max(1);
    let _ = ready_tx.send(Ok(source));

    let cap_frames = keep_secs as usize * sample_rate as usize;
    let mut ring: VecDeque<i16> = VecDeque::with_capacity((cap_frames + 1) * channels as usize);

    // Clock bookkeeping. `pts_ns` lives on flexaudio's own monotonic base; we
    // convert its origin into an `Instant` once so audio and video share a
    // single timeline.
    let mut anchor: Option<(i64, Instant)> = None;
    // Total frames pushed into the ring since the anchor (including generated
    // silence), i.e. the global frame index of the ring's tail.
    let mut total_frames: u64 = 0;
    // Frames dropped off the front of the ring; the global index of ring[0].
    let mut front_frame: u64 = 0;

    loop {
        let mut got_chunk = false;

        while let Some(chunk) = stream.poll_chunk() {
            got_chunk = true;

            let (anchor_pts, _) = *anchor.get_or_insert_with(|| {
                // Latency between "the samples were played" and "we polled
                // them"; subtract it so the anchor instant is the real time of
                // the first sample.
                let lag_ns = (monotonic_now_ns() - chunk.pts_ns).max(0) as u64;
                let inst = Instant::now() - Duration::from_nanos(lag_ns);
                (chunk.pts_ns, inst)
            });

            // Gap filling: if the device timeline moved further than the number
            // of frames we actually stored, the difference is a real hole
            // (device glitch, dropped chunks, ...). Pad it with silence so the
            // audio length keeps tracking wall-clock time and stays in sync.
            let expected_frames = (((chunk.pts_ns - anchor_pts).max(0) as u128
                * sample_rate as u128)
                / 1_000_000_000u128) as u64;
            let tolerance = (sample_rate as u64) / 50; // 20 ms
            if expected_frames > total_frames + tolerance {
                let missing = (expected_frames - total_frames) as usize;
                if chunk.flags.contains(ChunkFlags::DISCONTINUITY) {
                    tracing::debug!(missing_frames = missing, "filling audio gap with silence");
                }
                for _ in 0..missing * channels as usize {
                    ring.push_back(0);
                }
                total_frames += missing as u64;
            }

            for s in &chunk.data {
                ring.push_back((s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16);
            }
            total_frames += chunk.frames as u64;

            // Trim the ring down to the retention window.
            let ring_frames = ring.len() / channels as usize;
            if ring_frames > cap_frames {
                let drop_frames = ring_frames - cap_frames;
                ring.drain(..drop_frames * channels as usize);
                front_frame += drop_frames as u64;
            }
        }

        while let Some(event) = stream.poll_event() {
            match event {
                Event::PermissionDenied => {
                    tracing::error!("audio capture permission denied; stopping audio")
                }
                Event::DeviceLost => tracing::warn!("audio device lost"),
                Event::StreamStalled => tracing::warn!("audio stream stalled"),
                Event::StreamRecovered => tracing::info!("audio stream recovered"),
                Event::ChunkDropped { count } => {
                    tracing::warn!(count, "audio chunks dropped (ring full)")
                }
                other => tracing::warn!(?other, "audio stream event"),
            }
        }

        if shutdown_rx.try_recv().is_ok() {
            break;
        }
        if !got_chunk {
            // Chunks are 20 ms; polling at 5 ms keeps latency low without
            // busy-spinning.
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    stream.stop();
    // Drain whatever the ring still holds after the backend stopped. No gap
    // filling here: the timeline is finished, we just want the tail samples.
    while let Some(chunk) = stream.poll_chunk() {
        for s in &chunk.data {
            ring.push_back((s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16);
        }
    }
    let _ = total_frames;
    let ring_frames = ring.len() / channels as usize;
    if ring_frames > cap_frames {
        let drop_frames = ring_frames - cap_frames;
        ring.drain(..drop_frames * channels as usize);
        front_frame += drop_frames as u64;
    }

    let (_, anchor_instant) = anchor?;
    if ring.is_empty() {
        tracing::warn!("audio capture produced no samples");
        return None;
    }

    let start = anchor_instant + Duration::from_secs_f64(front_frame as f64 / sample_rate as f64);
    tracing::info!(
        frames = ring.len() / channels as usize,
        sample_rate,
        channels,
        "audio capture finished"
    );

    Some(AudioTrack {
        samples: ring.into_iter().collect(),
        sample_rate,
        channels,
        start,
    })
}
