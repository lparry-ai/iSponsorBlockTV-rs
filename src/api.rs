//! SponsorBlock, YouTube Data API and lounge pairing calls (port of `api_helpers.py`).

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use crate::config::Config;
use crate::lounge::API_BASE;

const YOUTUBE_API: &str = "https://www.googleapis.com/youtube/v3/";
const SEGMENT_TTL: Duration = Duration::from_secs(300);

#[derive(Clone, Debug, PartialEq)]
pub struct Segment {
    pub start: f64,
    pub end: f64,
    pub uuids: Vec<String>,
    /// SponsorBlock category (`sponsor`, `intro`, ...), reported in MQTT events.
    pub category: Option<String>,
}

/// Title and channel of a video, for the MQTT sensors.
#[derive(Clone, Debug)]
pub struct VideoMetadata {
    pub title: Option<String>,
    pub channel: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PairedScreen {
    pub screen_id: String,
    pub name: Option<String>,
}

/// Small LRU with optional per-entry expiry (port of `AsyncConditionalTTL` / `AsyncLRU`).
struct Cache<V> {
    max: usize,
    entries: HashMap<String, (V, Option<Instant>)>,
    order: VecDeque<String>,
}

impl<V: Clone> Cache<V> {
    fn new(max: usize) -> Self {
        Self {
            max,
            entries: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn get(&mut self, key: &str) -> Option<V> {
        let (value, expiry) = self.entries.get(key)?;
        if expiry.is_some_and(|e| e < Instant::now()) {
            self.entries.remove(key);
            self.order.retain(|k| k != key);
            return None;
        }
        let value = value.clone();
        self.order.retain(|k| k != key);
        self.order.push_back(key.to_string());
        Some(value)
    }

    fn put(&mut self, key: &str, value: V, ttl: Option<Duration>) {
        self.order.retain(|k| k != key);
        self.order.push_back(key.to_string());
        self.entries
            .insert(key.to_string(), (value, ttl.map(|t| Instant::now() + t)));
        while self.order.len() > self.max {
            if let Some(old) = self.order.pop_front() {
                self.entries.remove(&old);
            }
        }
    }
}

pub struct ApiHelper {
    http: reqwest::Client,
    apikey: String,
    skip_categories: Vec<String>,
    whitelist_ids: Vec<String>,
    skip_count_tracking: bool,
    minimum_skip_length: f64,
    sponsorblock_api_url: String,
    segments: Mutex<Cache<Vec<Segment>>>,
    whitelisted: Mutex<Cache<bool>>,
    metadata: Mutex<Cache<Option<VideoMetadata>>>,
}

impl ApiHelper {
    pub fn new(config: &Config, http: reqwest::Client) -> Self {
        Self {
            http,
            apikey: config.apikey.clone(),
            skip_categories: config.skip_categories.clone().unwrap_or_default(),
            whitelist_ids: config
                .channel_whitelist
                .iter()
                .map(|c| c.id.clone())
                .collect(),
            skip_count_tracking: config.skip_count_tracking,
            minimum_skip_length: config.minimum_skip_length,
            sponsorblock_api_url: config.sponsorblock_api_url.clone(),
            segments: Mutex::new(Cache::new(10)),
            whitelisted: Mutex::new(Cache::new(100)),
            metadata: Mutex::new(Cache::new(100)),
        }
    }

    /// Resolve a "Link with TV code" pairing code to a screen.
    pub async fn pair_with_code(&self, code: &str) -> Result<Option<PairedScreen>> {
        let code: String = code
            .chars()
            .filter(|c| *c != '-' && !c.is_whitespace())
            .collect();
        let resp = self
            .http
            .post(format!("{API_BASE}/pairing/get_screen"))
            .form(&[("pairing_code", code)])
            .send()
            .await?;
        if !resp.status().is_success() {
            return Ok(None);
        }
        let json: Value = resp.json().await?;
        let screen = &json["screen"];
        Ok(screen["screenId"].as_str().map(|id| PairedScreen {
            screen_id: id.to_string(),
            name: screen["name"].as_str().map(String::from),
        }))
    }

    async fn is_whitelisted(&self, video_id: &str) -> Result<bool> {
        if self.apikey.is_empty() || self.whitelist_ids.is_empty() {
            return Ok(false);
        }
        if let Some(v) = self.whitelisted.lock().await.get(video_id) {
            return Ok(v);
        }
        let json: Value = self
            .http
            .get(format!("{YOUTUBE_API}videos"))
            .query(&[("id", video_id), ("key", &self.apikey), ("part", "snippet")])
            .send()
            .await?
            .json()
            .await?;
        let channel = json["items"][0]["snippet"]["channelId"].as_str();
        let result = channel.is_some_and(|c| self.whitelist_ids.iter().any(|w| w == c));
        self.whitelisted.lock().await.put(video_id, result, None);
        Ok(result)
    }

    /// Fetch skippable segments for a video, using SponsorBlock's k-anonymity endpoint
    /// (only the first 4 hex chars of the SHA-256 of the video id are sent).
    pub async fn get_segments(&self, video_id: &str) -> Result<Vec<Segment>> {
        if self.skip_categories.is_empty() {
            return Ok(vec![]);
        }
        if let Some(v) = self.segments.lock().await.get(video_id) {
            return Ok(v);
        }
        if self.is_whitelisted(video_id).await? {
            self.segments.lock().await.put(video_id, vec![], None);
            return Ok(vec![]);
        }
        let hash = hex::encode(Sha256::digest(video_id.as_bytes()));
        let prefix = &hash[..4];
        let mut query: Vec<(&str, &str)> = self
            .skip_categories
            .iter()
            .map(|c| ("category", c.as_str()))
            .collect();
        query.push(("actionType", "skip"));
        query.push(("service", "youtube"));
        let resp = self
            .http
            .get(format!(
                "{}skipSegments/{prefix}",
                self.sponsorblock_api_url
            ))
            .header("Accept", "application/json")
            .query(&query)
            .send()
            .await?;
        let (segments, permanent) = match resp.status().as_u16() {
            200 => {
                let json: Value = resp.json().await?;
                let entry = json
                    .as_array()
                    .and_then(|a| a.iter().find(|v| v["videoID"].as_str() == Some(video_id)));
                match entry {
                    Some(e) => process_segments(&e["segments"], self.minimum_skip_length),
                    None => (vec![], false),
                }
            }
            404 => (vec![], false),
            status => {
                let body = resp.text().await.unwrap_or_default();
                tracing::warn!(
                    "Error getting segments for {video_id} (prefix {prefix}): {status} {body}"
                );
                return Ok(vec![]);
            }
        };
        let ttl = if permanent { None } else { Some(SEGMENT_TTL) };
        self.segments
            .lock()
            .await
            .put(video_id, segments.clone(), ttl);
        Ok(segments)
    }

    /// Tell SponsorBlock the segments were skipped (only if `skip_count_tracking` is on).
    pub async fn mark_viewed_segments(&self, uuids: &[String]) {
        if !self.skip_count_tracking {
            return;
        }
        for uuid in uuids {
            let url = format!("{}viewedVideoSponsorTime/", self.sponsorblock_api_url);
            if let Err(e) = self.http.post(url).query(&[("UUID", uuid)]).send().await {
                tracing::debug!("Failed to report skipped segment: {e}");
            }
        }
    }

    /// Search channels for the whitelist: returns `(id, title, subscriber count)`.
    /// Resolve a video's title and channel: YouTube Data API when an API key is set,
    /// otherwise (or if that fails) YouTube's keyless oEmbed endpoint. Cached; None on error.
    pub async fn get_video_metadata(&self, video_id: &str) -> Option<VideoMetadata> {
        if let Some(v) = self.metadata.lock().await.get(video_id) {
            return v;
        }
        let mut meta = None;
        if !self.apikey.is_empty() {
            meta = self.metadata_from_data_api(video_id).await;
        }
        if meta.is_none() {
            meta = self.metadata_from_oembed(video_id).await;
        }
        // Cache misses briefly so a transient failure is retried on the next play.
        let ttl = meta.is_none().then_some(SEGMENT_TTL);
        self.metadata.lock().await.put(video_id, meta.clone(), ttl);
        meta
    }

    async fn metadata_from_oembed(&self, video_id: &str) -> Option<VideoMetadata> {
        let url = format!("https://www.youtube.com/watch?v={video_id}");
        let resp = self
            .http
            .get("https://www.youtube.com/oembed")
            .query(&[("url", url.as_str()), ("format", "json")])
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let json: Value = resp.json().await.ok()?;
        Some(VideoMetadata {
            title: json["title"].as_str().map(String::from),
            channel: json["author_name"].as_str().map(String::from),
        })
    }

    async fn metadata_from_data_api(&self, video_id: &str) -> Option<VideoMetadata> {
        let resp = self
            .http
            .get(format!("{YOUTUBE_API}videos"))
            .query(&[("id", video_id), ("key", &self.apikey), ("part", "snippet")])
            .send()
            .await
            .ok()?;
        let json: Value = resp.json().await.ok()?;
        if json.get("error").is_some() {
            return None;
        }
        let snippet = &json["items"][0]["snippet"];
        (!snippet.is_null()).then(|| VideoMetadata {
            title: snippet["title"].as_str().map(String::from),
            channel: snippet["channelTitle"].as_str().map(String::from),
        })
    }

    pub async fn search_channels(&self, query: &str) -> Result<Vec<(String, String, String)>> {
        let json: Value = self
            .http
            .get(format!("{YOUTUBE_API}search"))
            .query(&[
                ("q", query),
                ("key", &self.apikey),
                ("part", "snippet"),
                ("type", "channel"),
                ("maxResults", "5"),
            ])
            .send()
            .await?
            .json()
            .await?;
        if let Some(err) = json.get("error") {
            return Err(anyhow!("YouTube API error: {}", err["message"]));
        }
        let mut out = vec![];
        for item in json["items"].as_array().into_iter().flatten() {
            let id = item["snippet"]["channelId"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let title = item["snippet"]["channelTitle"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let stats: Value = self
                .http
                .get(format!("{YOUTUBE_API}channels"))
                .query(&[
                    ("id", id.as_str()),
                    ("key", &self.apikey),
                    ("part", "statistics"),
                ])
                .send()
                .await?
                .json()
                .await?;
            let s = &stats["items"][0]["statistics"];
            let subs = if s["hiddenSubscriberCount"].as_bool() == Some(true) {
                "Hidden".to_string()
            } else {
                s["subscriberCount"].as_str().unwrap_or("?").to_string()
            };
            out.push((id, title, subs));
        }
        Ok(out)
    }
}

/// Merge overlapping / adjacent segments and drop short ones. Returns the segments and
/// whether they may be cached permanently (all segments locked by SponsorBlock moderators).
/// Faithful port of `ApiHelper.process_segments`.
pub fn process_segments(raw: &Value, minimum_skip_length: f64) -> (Vec<Segment>, bool) {
    struct Raw {
        start: f64,
        end: f64,
        uuid: String,
        locked: bool,
        category: Option<String>,
    }
    let mut segs: Vec<Raw> = raw
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| {
            Some(Raw {
                start: s["segment"][0].as_f64()?,
                end: s["segment"][1].as_f64()?,
                uuid: s["UUID"].as_str()?.to_string(),
                locked: s["locked"].as_i64() == Some(1),
                category: s["category"].as_str().map(String::from),
            })
        })
        .collect();

    // Extend ends of overlapping segments.
    segs.sort_by(|a, b| a.end.total_cmp(&b.end));
    for i in 0..segs.len() {
        for j in 0..segs.len() {
            if segs[j].start <= segs[i].end && segs[i].end <= segs[j].end {
                segs[i].end = segs[j].end;
            }
        }
    }
    // Extend starts of overlapping segments.
    segs.sort_by(|a, b| a.start.total_cmp(&b.start));
    for i in (0..segs.len()).rev() {
        for j in (0..segs.len()).rev() {
            if segs[j].start <= segs[i].start && segs[i].start <= segs[j].end {
                segs[i].start = segs[j].start;
            }
        }
    }

    let mut permanent = true;
    let mut out: Vec<Segment> = vec![];
    for s in &segs {
        permanent &= s.locked;
        let mut seg = Segment {
            start: s.start,
            end: s.end,
            uuids: vec![s.uuid.clone()],
            category: s.category.clone(),
        };
        // Less than 1 second apart: combine with the previous one and skip them together.
        if let Some(prev) = out.pop_if(|prev| seg.start - prev.end < 1.0) {
            seg.start = prev.start;
            seg.uuids.extend(prev.uuids);
        }
        if seg.end - seg.start > minimum_skip_length {
            out.push(seg);
        }
    }
    (out, permanent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn seg(s: f64, e: f64, id: &str, locked: i64) -> Value {
        json!({"segment": [s, e], "UUID": id, "locked": locked})
    }

    #[test]
    fn merges_overlapping_and_adjacent() {
        let raw = json!([
            seg(10.0, 20.0, "a", 1),
            seg(15.0, 30.0, "b", 1),
            seg(30.5, 40.0, "c", 1),
            seg(100.0, 110.0, "d", 1),
        ]);
        let (out, permanent) = process_segments(&raw, 1.0);
        assert!(permanent);
        assert_eq!(out.len(), 2);
        assert_eq!((out[0].start, out[0].end), (10.0, 40.0));
        assert_eq!(out[0].uuids.len(), 3);
        assert_eq!((out[1].start, out[1].end), (100.0, 110.0));
    }

    #[test]
    fn drops_short_segments_and_tracks_lock() {
        let raw = json!([seg(5.0, 5.5, "short", 1), seg(50.0, 60.0, "x", 0)]);
        let (out, permanent) = process_segments(&raw, 1.0);
        assert!(!permanent);
        assert_eq!(
            out,
            vec![Segment {
                start: 50.0,
                end: 60.0,
                uuids: vec!["x".into()],
                category: None,
            }]
        );
    }

    #[test]
    fn cache_expires_and_evicts() {
        let mut c: Cache<u8> = Cache::new(2);
        c.put("a", 1, None);
        c.put("b", 2, Some(Duration::ZERO));
        assert_eq!(c.get("b"), None);
        c.put("c", 3, None);
        c.put("d", 4, None);
        assert_eq!(c.get("a"), None);
        assert_eq!(c.get("d"), Some(4));
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;

    /// Hits the real SponsorBlock and YouTube APIs: `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn live_sponsorblock_and_pairing() {
        let config = Config {
            skip_categories: Some(vec!["sponsor".into(), "selfpromo".into(), "intro".into()]),
            ..Default::default()
        };
        let api = ApiHelper::new(&config, crate::http_client(false).unwrap());
        // A video with a 0–6.33s sponsor segment on SponsorBlock (as of 2026-10-10).
        let segs = api.get_segments("579MHCmgX7M").await.unwrap();
        println!("segments: {segs:?}");
        assert!(!segs.is_empty());
        assert!(api.pair_with_code("000000000000").await.unwrap().is_none());
        // No API key: title and channel come from oEmbed.
        let meta = api.get_video_metadata("579MHCmgX7M").await.unwrap();
        println!("metadata: {meta:?}");
        assert!(meta.title.is_some_and(|t| !t.is_empty()));
        assert!(meta.channel.is_some_and(|c| !c.is_empty()));
    }
}
