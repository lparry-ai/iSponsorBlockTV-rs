//! Per-device listener: keeps a lounge session alive and skips segments
//! (port of `DeviceListener` in `main.py` plus the event handling in `ytlounge.py`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::Instrument;

use crate::api::{ApiHelper, Segment};
use crate::config::{Config, Device};
use crate::lounge::{Event, Lounge};

const RETRY: Duration = Duration::from_secs(10);
const STATE_PLAYING: i64 = 1;

#[derive(Clone, Debug, Default)]
struct PlaybackState {
    current_time: f64,
    video_id: String,
    state: i64,
}

#[derive(Default)]
struct DeviceState {
    playback: PlaybackState,
    playback_speed: f64,
    volume: HashMap<String, Value>,
    shorts_disconnected: bool,
    skip_task: Option<JoinHandle<()>>,
}

pub struct DeviceListener {
    pub lounge: Arc<Lounge>,
    api: Arc<ApiHelper>,
    name: String,
    /// Seconds to skip early.
    offset: f64,
    /// Seconds to play into a segment before skipping it.
    skip_delay: f64,
    /// Land this many seconds before the end of the segment.
    skip_end_margin: f64,
    mute_ads: bool,
    skip_ads: bool,
    auto_play: bool,
    state: Mutex<DeviceState>,
}

fn str_field<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key].as_str().unwrap_or_default()
}

fn num_field(v: &Value, key: &str) -> Option<f64> {
    match &v[key] {
        Value::String(s) => s.parse().ok(),
        other => other.as_f64(),
    }
}

impl DeviceListener {
    pub fn new(
        config: &Config,
        device: &Device,
        api: Arc<ApiHelper>,
        http: reqwest::Client,
    ) -> Arc<Self> {
        Arc::new(Self {
            lounge: Arc::new(Lounge::new(
                http,
                config.join_name.clone(),
                device.screen_id.clone(),
            )),
            api,
            name: device.name.clone(),
            offset: device.offset / 1000.0,
            skip_delay: config.skip_delay.max(0.0),
            skip_end_margin: config.skip_end_margin.max(0.0),
            mute_ads: config.mute_ads,
            skip_ads: config.skip_ads,
            auto_play: config.auto_play,
            state: Mutex::new(DeviceState {
                playback_speed: 1.0,
                ..Default::default()
            }),
        })
    }

    fn st(&self) -> std::sync::MutexGuard<'_, DeviceState> {
        self.state.lock().unwrap()
    }

    /// Run forever: (re)link, wait for the screen, connect, subscribe.
    pub async fn run(self: Arc<Self>) {
        let span = tracing::info_span!("device", name = %self.name);
        async move {
            let (tx, rx) = mpsc::unbounded_channel();
            tokio::spawn(self.clone().handle_events(rx).in_current_span());
            tokio::spawn(self.clone().refresh_auth_loop().in_current_span());
            let lounge = &self.lounge;
            loop {
                while !lounge.linked() {
                    tracing::debug!("Refreshing auth");
                    if let Err(e) = lounge.refresh_auth().await {
                        tracing::debug!("Refreshing auth failed: {e:#}");
                        tokio::time::sleep(RETRY).await;
                    }
                }
                while !lounge.is_available().await.unwrap_or(false) {
                    tracing::debug!("Waiting for device to be available");
                    tokio::time::sleep(RETRY).await;
                }
                loop {
                    match lounge.connect(&tx).await {
                        Ok(true) => break,
                        Ok(false) => tracing::debug!("Waiting for device to be connected"),
                        Err(e) => tracing::debug!("Connect failed: {e:#}"),
                    }
                    if !lounge.linked() {
                        break;
                    }
                    tokio::time::sleep(RETRY).await;
                }
                if !lounge.connected() {
                    continue;
                }
                tracing::info!(
                    "Connected to device {} ({})",
                    lounge.screen_name().unwrap_or_default(),
                    self.name
                );
                if let Err(e) = lounge.subscribe(&tx).await {
                    tracing::debug!("Subscription ended: {e:#}");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
        .instrument(span)
        .await
    }

    async fn refresh_auth_loop(self: Arc<Self>) {
        loop {
            tokio::time::sleep(Duration::from_secs(24 * 60 * 60)).await;
            if let Err(e) = self.lounge.refresh_auth().await {
                tracing::debug!("Periodic auth refresh failed: {e:#}");
            }
        }
    }

    async fn handle_events(self: Arc<Self>, mut rx: mpsc::UnboundedReceiver<Event>) {
        while let Some((kind, args)) = rx.recv().await {
            tracing::debug!("event {kind} {args:?}");
            self.clone().handle_event(&kind, &args);
        }
    }

    /// Fire-and-forget a lounge command, logging failures.
    fn spawn_cmd<F>(&self, what: &'static str, fut: F)
    where
        F: std::future::Future<Output = anyhow::Result<bool>> + Send + 'static,
    {
        tokio::spawn(
            async move {
                if let Err(e) = fut.await {
                    tracing::debug!("{what} failed: {e:#}");
                }
            }
            .in_current_span(),
        );
    }

    fn mute(&self, mute: bool) {
        let volume = {
            let mut st = self.st();
            st.volume
                .insert("muted".into(), Value::String(mute.to_string()));
            match st.volume.get("volume") {
                Some(Value::String(s)) => s.clone(),
                Some(v) if !v.is_null() => v.to_string(),
                _ => "100".into(),
            }
        };
        let lounge = self.lounge.clone();
        self.spawn_cmd(
            "setVolume",
            async move { lounge.set_volume(volume, mute).await },
        );
    }

    fn skip_ad(&self) {
        let lounge = self.lounge.clone();
        self.spawn_cmd("skipAd", async move { lounge.skip_ad().await });
    }

    fn prefetch(&self, video_id: &str) {
        if video_id.is_empty() {
            return;
        }
        tracing::info!("Getting segments for next video: {video_id}");
        let (api, id) = (self.api.clone(), video_id.to_string());
        tokio::spawn(async move {
            let _ = api.get_segments(&id).await;
        });
    }

    /// Ad handling shared by `onAdStateChange` and `adPlaying`.
    fn handle_ad(&self, data: &Value) {
        if self.skip_ads && str_field(data, "isSkipEnabled") == "true" {
            tracing::info!("Ad can be skipped, skipping");
            self.skip_ad();
            if self.mute_ads {
                self.mute(false);
            }
        } else if self.mute_ads {
            tracing::info!("Ad has started, muting");
            self.mute(true);
        }
    }

    fn handle_event(self: Arc<Self>, kind: &str, args: &[Value]) {
        let data = args.first().cloned().unwrap_or(Value::Null);
        match kind {
            "onStateChange" => {
                if self.mute_ads && str_field(&data, "state") == "1" {
                    self.mute(false);
                }
                {
                    let mut st = self.st();
                    st.playback.current_time = num_field(&data, "currentTime").unwrap_or(0.0);
                    st.playback.state = num_field(&data, "state").map(|s| s as i64).unwrap_or(-1);
                }
                self.on_playback_change();
            }
            "nowPlaying" => {
                if self.mute_ads && str_field(&data, "state") == "1" {
                    tracing::info!("Ad has ended, unmuting");
                    self.mute(false);
                }
                {
                    let mut st = self.st();
                    if let Some(t) = num_field(&data, "currentTime") {
                        st.playback.current_time = t;
                    }
                    st.playback.video_id = str_field(&data, "videoId").to_string();
                    st.playback.state = num_field(&data, "state").map(|s| s as i64).unwrap_or(-1);
                }
                self.on_playback_change();
            }
            "onAdStateChange" => {
                if str_field(&data, "adState") == "0" && str_field(&data, "currentTime") != "0" {
                    if self.mute_ads {
                        tracing::info!("Ad has ended, unmuting");
                        self.mute(false);
                    }
                } else {
                    self.handle_ad(&data);
                }
            }
            "adPlaying" => {
                self.prefetch(str_field(&data, "contentVideoId"));
                self.handle_ad(&data);
            }
            "onVolumeChanged" => {
                if let Value::Object(m) = data {
                    self.st().volume = m.into_iter().collect();
                }
            }
            "autoplayUpNext" => self.prefetch(str_field(&data, "videoId")),
            "onSubtitlesTrackChanged" => {
                let replay = std::mem::take(&mut self.st().shorts_disconnected);
                let video_id = str_field(&data, "videoId").to_string();
                if replay && !video_id.is_empty() {
                    let lounge = self.lounge.clone();
                    self.spawn_cmd(
                        "setPlaylist",
                        async move { lounge.play_video(&video_id).await },
                    );
                }
            }
            "loungeScreenDisconnected" => {
                // Watching a Short disconnects us; replay the video once we reconnect.
                if str_field(&data, "reason") == "disconnectedByUserScreenInitiated" {
                    self.st().shorts_disconnected = true;
                }
            }
            "onAutoplayModeChanged" => {
                let mode = str_field(&data, "autoplayMode");
                let wanted = if self.auto_play {
                    "ENABLED"
                } else {
                    "DISABLED"
                };
                if mode != "UNSUPPORTED" && mode != wanted {
                    let (lounge, on) = (self.lounge.clone(), self.auto_play);
                    self.spawn_cmd("setAutoplayMode", async move {
                        lounge.set_auto_play_mode(on).await
                    });
                }
            }
            "onPlaybackSpeedChanged" => {
                if let Some(speed) = num_field(&data, "playbackSpeed").filter(|s| *s > 0.0) {
                    self.st().playback_speed = speed;
                }
                let lounge = self.lounge.clone();
                self.spawn_cmd(
                    "getNowPlaying",
                    async move { lounge.get_now_playing().await },
                );
            }
            _ => {}
        }
    }

    /// Cancel any pending skip and schedule a new one for the current state.
    fn on_playback_change(self: Arc<Self>) {
        let started = Instant::now();
        let playback = self.st().playback.clone();
        let this = self.clone();
        let task = tokio::spawn(
            async move { this.process_playstatus(playback, started).await }.in_current_span(),
        );
        if let Some(old) = self.st().skip_task.replace(task) {
            old.abort();
        }
    }

    async fn process_playstatus(&self, state: PlaybackState, started: Instant) {
        let mut segments = vec![];
        if !state.video_id.is_empty() {
            match self.api.get_segments(&state.video_id).await {
                Ok(s) => segments = s,
                Err(e) => tracing::warn!("Failed to get segments for {}: {e:#}", state.video_id),
            }
        }
        if state.state == STATE_PLAYING {
            tracing::info!(
                "Playing video {} with {} segments",
                state.video_id,
                segments.len()
            );
            if let Some((wait, target, seg)) =
                self.next_skip(&segments, state.current_time, started)
            {
                tokio::time::sleep(wait).await;
                tracing::info!("Skipping segment: seeking to {target}");
                let (seek, _) = tokio::join!(
                    self.lounge.seek_to(target),
                    self.api.mark_viewed_segments(&seg.uuids)
                );
                if let Err(e) = seek {
                    tracing::warn!("Seek failed: {e:#}");
                }
            }
        }
    }

    /// Find the next segment to skip, how long to wait, and where to seek to.
    fn next_skip(
        &self,
        segments: &[Segment],
        position: f64,
        started: Instant,
    ) -> Option<(Duration, f64, Segment)> {
        let timing = SkipTiming {
            elapsed: started.elapsed().as_secs_f64(),
            speed: self.st().playback_speed,
            offset: self.offset,
            delay: self.skip_delay,
            end_margin: self.skip_end_margin,
        };
        next_skip(segments, position, &timing)
    }
}

struct SkipTiming {
    /// Seconds since the playback state was reported.
    elapsed: f64,
    speed: f64,
    /// Device latency compensation (skip this much earlier).
    offset: f64,
    /// `skip_delay` from the config.
    delay: f64,
    /// `skip_end_margin` from the config.
    end_margin: f64,
}

/// Pure version of the skip timing logic (port of `time_to_segment`, extended with
/// `skip_delay` / `skip_end_margin`). With both at 0 it behaves like the Python version.
fn next_skip(
    segments: &[Segment],
    position: f64,
    t: &SkipTiming,
) -> Option<(Duration, f64, Segment)> {
    for seg in segments {
        // A segment at the very start of the video that we're already inside.
        let within_start =
            position < 1.0 && 1.0 < seg.end && seg.start <= position && position < seg.end;
        // The point in the video where we skip. Comparing against this (not the segment
        // start) keeps a pending delayed skip alive when the TV reports progress mid-delay.
        let skip_at = seg.start + t.delay;
        if !(within_start || skip_at > position) {
            continue;
        }
        let skip_at = skip_at.max(position);
        let target = seg.end - t.end_margin;
        if target <= skip_at {
            // Delay + margin leave nothing worth skipping; let this segment play.
            continue;
        }
        let wait = ((skip_at - position - t.elapsed) / t.speed) - t.offset;
        return Some((Duration::from_secs_f64(wait.max(0.0)), target, seg.clone()));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(start: f64, end: f64) -> Segment {
        Segment {
            start,
            end,
            uuids: vec!["u".into()],
        }
    }

    fn timing(speed: f64, offset: f64, delay: f64, end_margin: f64) -> SkipTiming {
        SkipTiming {
            elapsed: 0.0,
            speed,
            offset,
            delay,
            end_margin,
        }
    }

    #[test]
    fn waits_until_next_segment_adjusted_for_speed_and_offset() {
        let segs = [seg(5.0, 8.0), seg(30.0, 40.0)];
        let (wait, target, s) = next_skip(&segs, 10.0, &timing(2.0, 0.5, 0.0, 0.0)).unwrap();
        assert_eq!(s.start, 30.0);
        assert_eq!(target, 40.0);
        assert!((wait.as_secs_f64() - 9.5).abs() < 1e-9);
    }

    #[test]
    fn skips_immediately_when_inside_opening_segment() {
        let (wait, target, _) =
            next_skip(&[seg(0.0, 12.0)], 0.0, &timing(1.0, 0.0, 0.0, 0.0)).unwrap();
        assert_eq!(target, 12.0);
        assert_eq!(wait, Duration::ZERO);
    }

    #[test]
    fn nothing_after_last_segment() {
        assert!(next_skip(&[seg(5.0, 8.0)], 20.0, &timing(1.0, 0.0, 0.0, 0.0)).is_none());
    }

    #[test]
    fn delay_and_end_margin() {
        let t = timing(1.0, 0.0, 3.0, 1.0);
        let (wait, target, _) = next_skip(&[seg(30.0, 60.0)], 10.0, &t).unwrap();
        assert!((wait.as_secs_f64() - 23.0).abs() < 1e-9);
        assert_eq!(target, 59.0);
    }

    #[test]
    fn progress_update_during_delay_keeps_the_skip() {
        // TV reports 31.5s: inside the segment but before start + delay.
        let t = timing(1.0, 0.0, 3.0, 1.0);
        let (wait, target, _) = next_skip(&[seg(30.0, 60.0)], 31.5, &t).unwrap();
        assert!((wait.as_secs_f64() - 1.5).abs() < 1e-9);
        assert_eq!(target, 59.0);
        // After the skip lands at 59s, the same segment isn't skipped again.
        assert!(next_skip(&[seg(30.0, 60.0)], 59.0, &t).is_none());
    }

    #[test]
    fn delay_applies_to_opening_segment() {
        let t = timing(1.0, 0.0, 3.0, 1.0);
        let (wait, target, _) = next_skip(&[seg(0.0, 20.0)], 0.4, &t).unwrap();
        assert!((wait.as_secs_f64() - 2.6).abs() < 1e-9);
        assert_eq!(target, 19.0);
    }

    #[test]
    fn segment_too_short_for_delay_and_margin_is_left_alone() {
        let t = timing(1.0, 0.0, 3.0, 1.0);
        let segs = [seg(30.0, 33.5), seg(50.0, 70.0)];
        let (_, target, s) = next_skip(&segs, 10.0, &t).unwrap();
        assert_eq!(s.start, 50.0);
        assert_eq!(target, 69.0);
    }
}
