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
use crate::mqtt::Notifier;

const RETRY: Duration = Duration::from_secs(10);
const STATE_PLAYING: i64 = 1;
const STATE_STOPPED: i64 = -1;

/// ` "Title" (Channel)` for log lines, or empty if the title couldn't be resolved.
fn video_label(meta: Option<crate::api::VideoMetadata>) -> String {
    let Some(meta) = meta else {
        return String::new();
    };
    let mut out = String::new();
    if let Some(title) = meta.title.filter(|t| !t.is_empty()) {
        out.push_str(&format!(" {title:?}"));
    }
    if let Some(channel) = meta.channel.filter(|c| !c.is_empty()) {
        out.push_str(&format!(" ({channel})"));
    }
    out
}

/// Lounge playback state -> value of the MQTT `playback_state` sensor.
fn playback_state_name(state: i64) -> &'static str {
    match state {
        -1 => "stopped",
        0 => "buffering",
        1 => "playing",
        2 => "paused",
        3 => "starting",
        1081 => "advertisement",
        _ => "unknown",
    }
}

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
    /// MQTT: video id whose title we last published ("" = idle).
    last_title_id: String,
    /// MQTT: last published playback_state.
    last_playback_state: &'static str,
    metadata_task: Option<JoinHandle<()>>,
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
    notifier: Option<Notifier>,
    screen_id: String,
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
        notifier: Option<Notifier>,
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
            notifier,
            screen_id: device.screen_id.clone(),
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
                if let Some(n) = &self.notifier {
                    n.set_connected(&self.screen_id, true);
                }
                let result = lounge.subscribe(&tx).await;
                self.publish_disconnected();
                if let Err(e) = result {
                    tracing::debug!("Subscription ended: {e:#}");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
        .instrument(span)
        .await
    }

    /// MQTT: connection dropped -> connected OFF, blank title/channel, state stopped.
    fn publish_disconnected(&self) {
        let Some(n) = &self.notifier else { return };
        n.set_connected(&self.screen_id, false);
        n.set_title(&self.screen_id, "");
        n.set_channel(&self.screen_id, "");
        let mut st = self.st();
        st.last_title_id.clear();
        if st.last_playback_state != "stopped" {
            st.last_playback_state = "stopped";
            n.set_playback_state(&self.screen_id, "stopped");
        }
    }

    fn emit_ad(&self, event_type: &str) {
        if let Some(n) = &self.notifier {
            n.emit(event_type, &self.screen_id, &self.name, &[]);
        }
    }

    /// MQTT: update playback_state / title / channel for a new playback state.
    fn publish_playback(&self, playback: &PlaybackState) {
        let Some(n) = &self.notifier else { return };
        let mut st = self.st();
        let name = playback_state_name(playback.state);
        if name != st.last_playback_state {
            st.last_playback_state = name;
            n.set_playback_state(&self.screen_id, name);
        }
        let id = &playback.video_id;
        if playback.state == STATE_PLAYING && !id.is_empty() && *id != st.last_title_id {
            // Show the id now; the lookup replaces it with the real title and channel.
            st.last_title_id = id.clone();
            n.set_title(&self.screen_id, id);
            if let Some(old) = st.metadata_task.take() {
                old.abort();
            }
            st.metadata_task = Some(self.resolve_metadata(n.clone(), id.clone()));
        } else if !st.last_title_id.is_empty() && (playback.state == STATE_STOPPED || id.is_empty())
        {
            st.last_title_id.clear();
            n.set_title(&self.screen_id, "");
            n.set_channel(&self.screen_id, "");
        }
    }

    fn resolve_metadata(&self, n: Notifier, video_id: String) -> JoinHandle<()> {
        let api = self.api.clone();
        let screen_id = self.screen_id.clone();
        tokio::spawn(async move {
            let Some(meta) = api.get_video_metadata(&video_id).await else {
                return;
            };
            if let Some(title) = meta.title.filter(|t| !t.is_empty()) {
                n.set_title(&screen_id, &title);
            }
            if let Some(channel) = meta.channel.filter(|c| !c.is_empty()) {
                n.set_channel(&screen_id, &channel);
            }
        })
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
            self.emit_ad("ad_skipped");
        } else if self.mute_ads {
            tracing::info!("Ad has started, muting");
            self.mute(true);
            self.emit_ad("ad_started");
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
                    self.emit_ad("ad_ended");
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
                    self.emit_ad("ad_ended");
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
        // Before the (cancellable) segment fetch, so a newer state can't drop it.
        self.publish_playback(&playback);
        let this = self.clone();
        let task = tokio::spawn(
            async move { this.process_playstatus(playback, started).await }.in_current_span(),
        );
        if let Some(old) = self.st().skip_task.replace(task) {
            old.abort();
        }
    }

    async fn process_playstatus(self: Arc<Self>, state: PlaybackState, started: Instant) {
        let mut segments = vec![];
        let mut label = String::new();
        if !state.video_id.is_empty() {
            // Title lookup runs alongside the segment fetch and is capped, so it can't hold
            // up a skip (the skip timing accounts for elapsed time anyway).
            let (result, meta) = tokio::join!(
                self.api.get_segments(&state.video_id),
                tokio::time::timeout(
                    Duration::from_secs(2),
                    self.api.get_video_metadata(&state.video_id)
                ),
            );
            match result {
                Ok(s) => segments = s,
                Err(e) => tracing::warn!("Failed to get segments for {}: {e:#}", state.video_id),
            }
            label = video_label(meta.ok().flatten());
        }
        if state.state == STATE_PLAYING {
            tracing::info!(
                "Playing video {}{label} with {} segments",
                state.video_id,
                segments.len()
            );
            if let Some((wait, target, seg)) =
                self.next_skip(&segments, state.current_time, started)
            {
                // Only the wait is cancellable (a newer playback state reschedules it). The
                // seek makes the TV report a new state straight away, which would abort this
                // task, so the skip itself runs as its own task and always completes.
                tokio::time::sleep(wait).await;
                let from = (seg.start + self.skip_delay).max(state.current_time);
                tokio::spawn(
                    self.clone()
                        .perform_skip(state.video_id, seg, from, target)
                        .in_current_span(),
                );
            }
        }
    }

    /// Seek past a segment, then report it (MQTT event, SponsorBlock view count).
    async fn perform_skip(self: Arc<Self>, video_id: String, seg: Segment, from: f64, target: f64) {
        tracing::info!("Skipping segment: seeking to {target}");
        if let Err(e) = self.lounge.seek_to(target).await {
            tracing::warn!("Seek failed: {e:#}");
            return;
        }
        if let Some(n) = &self.notifier {
            let round = |x: f64| (x * 1000.0).round() / 1000.0;
            n.emit(
                "segment_skipped",
                &self.screen_id,
                &self.name,
                &[
                    (
                        "video_id",
                        Some(video_id.as_str())
                            .filter(|v| !v.is_empty())
                            .map_or(Value::Null, Value::from),
                    ),
                    (
                        "category",
                        seg.category.clone().map_or(Value::Null, Value::from),
                    ),
                    ("skipped_from", Value::from(round(from))),
                    ("skipped_to", Value::from(round(target))),
                ],
            );
        }
        self.api.mark_viewed_segments(&seg.uuids).await;
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
            category: None,
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
    fn video_label_formats_title_and_channel() {
        use crate::api::VideoMetadata;
        let meta = |t: Option<&str>, c: Option<&str>| {
            Some(VideoMetadata {
                title: t.map(String::from),
                channel: c.map(String::from),
            })
        };
        assert_eq!(
            video_label(meta(Some("A \"quoted\" title"), Some("Chan"))),
            r#" "A \"quoted\" title" (Chan)"#
        );
        assert_eq!(video_label(meta(Some("T"), None)), r#" "T""#);
        assert_eq!(video_label(None), "");
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
