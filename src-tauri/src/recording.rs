//! Screen recording — scrcpy H.264 video stream → local `.mp4` (decision #2).
//!
//! No `adb screenrecord` (3-min cap, on-device storage, extra pull) and no
//! FFmpeg (FFI crash / CLI zombie / bundle bloat). Instead we tap the H.264 NAL
//! stream phone-control already receives from scrcpy and mux it to MP4 with the
//! pure-Rust `muxide` crate — zero external deps, muxing runs right in the
//! scrcpy receive loop.
//!
//! The tap lives in `stream.rs::forward_h264_to_ws`: for each parsed packet it
//! looks up a [`Recorder`] for the serial (via Tauri managed state) and feeds
//! it. So recording is simply "register a Recorder, then run a stream".
//!
//! scrcpy gives us exactly what `muxide` wants: Annex-B NAL units, a per-packet
//! microsecond PTS, an explicit keyframe flag, and SPS/PPS as a leading config
//! packet. We buffer config, start the file on the first keyframe (MP4 must
//! begin on one), prepend SPS/PPS to keyframes, normalise PTS to start at 0, and
//! keep it strictly increasing (a `muxide` hard requirement).
//!
//! ## Screen rotation (known caveat, not yet handled)
//! If the app rotates mid-test, scrcpy resends new dimensions + SPS/PPS. MP4 can't
//! change a track's resolution without transcoding, so the correct fix is to
//! `finish()` the current file and open `part2.mp4` at the new size. Not yet
//! implemented — a rotation currently keeps writing at the original dimensions.

use std::collections::HashMap;
use std::fs::File;
use std::sync::{Arc, Mutex};

use muxide::api::{Muxer, MuxerBuilder, VideoCodec};

/// Registry of active recorders, keyed by device serial. Shared (via Tauri
/// managed state) between the control API (register/finish) and the scrcpy
/// receive loop (feed). Std mutex: the receive loop is synchronous and must not
/// `.await` while holding it — critical sections here never do.
pub type Recorders = Arc<Mutex<HashMap<String, Recorder>>>;

pub fn new_recorders() -> Recorders {
    Arc::new(Mutex::new(HashMap::new()))
}

/// A single in-progress recording. Feeds parsed scrcpy packets into a `muxide`
/// muxer, lazily created once the first keyframe with known dimensions arrives.
pub struct Recorder {
    pub task_id: String,
    path: String,
    muxer: Option<Muxer<File>>,
    last_config: Option<Vec<u8>>,
    base_pts_us: Option<u64>,
    last_pts: f64,
    frames: u64,
}

impl Recorder {
    pub fn new(path: String, task_id: String) -> Self {
        Self {
            task_id,
            path,
            muxer: None,
            last_config: None,
            base_pts_us: None,
            last_pts: -1.0,
            frames: 0,
        }
    }

    /// Feed one parsed scrcpy packet. `nal` is Annex-B; `pts_us` microseconds.
    /// `loop_config` is the stream's current SPS/PPS (the receive loop tracks it),
    /// so a recorder registered mid-stream still gets config it never saw arrive.
    pub fn feed(
        &mut self,
        is_config: bool,
        is_key: bool,
        pts_us: u64,
        width: u32,
        height: u32,
        nal: &[u8],
        loop_config: Option<&[u8]>,
    ) {
        if is_config {
            // SPS/PPS — remember it, don't write it as a frame.
            self.last_config = Some(nal.to_vec());
            return;
        }

        // Adopt the stream's SPS/PPS if we haven't captured our own yet — muxide
        // requires the first keyframe to carry SPS/PPS.
        if self.last_config.is_none() {
            if let Some(cfg) = loop_config {
                self.last_config = Some(cfg.to_vec());
            }
        }

        // Lazily open the file on the first keyframe with known dimensions and a
        // known SPS/PPS — an MP4 must start on such a keyframe.
        if self.muxer.is_none() {
            if !is_key || width == 0 || height == 0 || self.last_config.is_none() {
                return;
            }
            match File::create(&self.path).map_err(|e| e.to_string()).and_then(|f| {
                MuxerBuilder::new(f)
                    .video(VideoCodec::H264, width, height, 30.0)
                    .build()
                    .map_err(|e| e.to_string())
            }) {
                Ok(m) => self.muxer = Some(m),
                Err(e) => {
                    eprintln!("[REC] {} muxer init failed: {e}", self.task_id);
                    return;
                }
            }
        }

        // Prepend SPS/PPS to keyframes so muxide can build the avcC box.
        let frame: Vec<u8> = if is_key {
            match &self.last_config {
                Some(cfg) => {
                    let mut v = Vec::with_capacity(cfg.len() + nal.len());
                    v.extend_from_slice(cfg);
                    v.extend_from_slice(nal);
                    v
                }
                None => nal.to_vec(),
            }
        } else {
            nal.to_vec()
        };

        // Normalise PTS to start at 0, and enforce muxide's strictly-increasing
        // requirement (nudge duplicates/regressions by 1µs rather than drop).
        let base = *self.base_pts_us.get_or_insert(pts_us);
        let mut pts = pts_us.saturating_sub(base) as f64 / 1_000_000.0;
        if pts <= self.last_pts {
            pts = self.last_pts + 1e-6;
        }

        if let Some(m) = self.muxer.as_mut() {
            if let Err(e) = m.write_video(pts, &frame, is_key) {
                eprintln!("[REC] {} write_video failed: {e}", self.task_id);
                return;
            }
            self.last_pts = pts;
            self.frames += 1;
        }
    }

    /// Finalise the MP4 (writes the fast-start moov). Returns (path, frame_count).
    pub fn finish(self) -> Result<(String, u64), String> {
        let frames = self.frames;
        if let Some(m) = self.muxer {
            m.finish().map_err(|e| e.to_string())?;
        }
        Ok((self.path, frames))
    }
}
