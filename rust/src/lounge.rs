//! YouTube Lounge API client.
//!
//! Port of `pyytlounge` (see PORTING.md for the exact version) combined with the
//! overrides iSponsorBlockTV applies on top of it (`ytlounge.py`): the iOS app
//! identity, the custom `connect` body and the "connect as screen" fallback.

use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use futures_util::StreamExt;
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;

pub const API_BASE: &str = "https://www.youtube.com/api/lounge";
/// App identity iSponsorBlockTV presents to YouTube (overrides pyytlounge's "web"/"youtube-desktop").
const APP_ID: &str = "ytios-phone-20.15.1";
/// Clients we refuse to control (YouTube Kids breaks the lounge session).
const BLACKLISTED_CLIENTS: &[&str] = &["TVHTML5_FOR_KIDS"];
/// YouTube sends something at least every ~30s; treat 60s of silence as a dead stream.
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// A lounge event forwarded to the device handler: `(event_type, args)`.
pub type Event = (String, Vec<Value>);

#[derive(Default)]
struct Session {
    lounge_token: Option<String>,
    sid: Option<String>,
    gsession: Option<String>,
    last_event_id: Option<i64>,
    screen_name: Option<String>,
}

pub struct Lounge {
    http: reqwest::Client,
    device_name: String,
    screen_id: String,
    session: Mutex<Session>,
    /// Serialises commands and holds the command offset (`ofs`/`RID`).
    command_offset: tokio::sync::Mutex<u64>,
}

impl Lounge {
    pub fn new(http: reqwest::Client, device_name: String, screen_id: String) -> Self {
        Self {
            http,
            device_name,
            screen_id,
            session: Mutex::new(Session::default()),
            command_offset: tokio::sync::Mutex::new(1),
        }
    }

    fn s(&self) -> std::sync::MutexGuard<'_, Session> {
        self.session.lock().unwrap()
    }

    pub fn linked(&self) -> bool {
        self.s().lounge_token.is_some()
    }

    pub fn connected(&self) -> bool {
        let s = self.s();
        s.sid.is_some() && s.gsession.is_some()
    }

    pub fn screen_name(&self) -> Option<String> {
        self.s().screen_name.clone()
    }

    fn connection_lost(&self) {
        let mut s = self.s();
        s.sid = None;
        s.gsession = None;
        s.last_event_id = None;
    }

    fn token_expired(&self) {
        self.s().lounge_token = None;
    }

    /// Exchange the screen id for a fresh lounge token.
    pub async fn refresh_auth(&self) -> Result<bool> {
        let resp: Value = self
            .http
            .post(format!("{API_BASE}/pairing/get_lounge_token_batch"))
            .form(&[("screen_ids", self.screen_id.as_str())])
            .send()
            .await?
            .json()
            .await?;
        let token = resp["screens"][0]["loungeToken"]
            .as_str()
            .ok_or_else(|| anyhow!("no lounge token in refresh response"))?;
        self.s().lounge_token = Some(token.to_string());
        tracing::debug!("Refreshed lounge auth");
        Ok(true)
    }

    pub async fn is_available(&self) -> Result<bool> {
        let token = self
            .s()
            .lounge_token
            .clone()
            .ok_or_else(|| anyhow!("not linked"))?;
        let resp: Value = self
            .http
            .post(format!("{API_BASE}/pairing/get_screen_availability"))
            .form(&[("lounge_token", token.as_str())])
            .send()
            .await?
            .json()
            .await?;
        Ok(resp["screens"][0]["status"].as_str() == Some("online"))
    }

    fn connect_body(&self, token: &str, as_screen: bool) -> Vec<(&'static str, String)> {
        let mut body = vec![
            ("mdx-version", "3".into()),
            ("TYPE", "xmlhttp".into()),
            ("theme", "cl".into()),
            ("sessionSource", "MDX_SESSION_SOURCE_UNKNOWN".into()),
            (
                "connectParams",
                r#"{"setStatesParams": "{"playbackSpeed":0}"}"#.into(),
            ),
            ("RID", "1".into()),
            ("CVER", "1".into()),
            ("capabilities", "que,dsdtr,atp,vsp".into()),
            ("ui", "false".into()),
            ("app", APP_ID.into()),
            ("pairing_type", "manual".into()),
            ("VER", "8".into()),
            ("loungeIdToken", token.into()),
            ("name", self.device_name.clone()),
        ];
        if as_screen {
            body.push(("id", uuid::Uuid::new_v4().to_string()));
            body.push(("sessionNonce", uuid::Uuid::new_v4().to_string()));
            body.push(("device", "LOUNGE_SCREEN".into()));
        } else {
            body.push(("id", self.screen_id.clone()));
            body.push(("device", "REMOTE_CONTROL".into()));
        }
        body
    }

    /// Open a new lounge session. Events contained in the bind response are forwarded to `tx`.
    pub async fn connect(&self, tx: &UnboundedSender<Event>) -> Result<bool> {
        let token = self
            .s()
            .lounge_token
            .clone()
            .ok_or_else(|| anyhow!("not linked"))?;
        let resp = self
            .http
            .post(format!("{API_BASE}/bc/bind"))
            .form(&self.connect_body(&token, false))
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if status.as_u16() == 401 {
            if text.contains("Connection denied") {
                tracing::warn!("Connection denied, attempting to circumvent the issue");
                self.connect_as_screen(&token).await?;
            }
            return Ok(false);
        }
        if !status.is_success() {
            tracing::warn!("Unknown reply to connect {status}");
            return Ok(false);
        }
        let mut parser = ChunkParser::default();
        for line in text.lines() {
            if let Some(chunk) = parser.push_line(line) {
                self.process_chunk(chunk, tx)?;
            }
        }
        *self.command_offset.lock().await = 1;
        Ok(self.connected())
    }

    /// iSponsorBlockTV's workaround for "Connection denied": join as a screen, then ask the
    /// user to force-close the YouTube app and exit (mirrors the Python behaviour).
    async fn connect_as_screen(&self, token: &str) -> Result<()> {
        self.http
            .post(format!("{API_BASE}/bc/bind"))
            .form(&self.connect_body(token, true))
            .send()
            .await?
            .text()
            .await?;
        tracing::error!(
            "Connected as screen: please force close the app on the device for iSponsorBlockTV to work properly"
        );
        tracing::warn!("Exiting in 5 seconds");
        tokio::time::sleep(Duration::from_secs(5)).await;
        std::process::exit(0);
    }

    fn common_params(&self) -> Vec<(&'static str, String)> {
        let s = self.s();
        let mut p = vec![
            ("name", self.device_name.clone()),
            ("device", "REMOTE_CONTROL".into()),
            ("app", APP_ID.into()),
            ("VER", "8".into()),
            ("v", "2".into()),
        ];
        if let Some(t) = &s.lounge_token {
            p.push(("loungeIdToken", t.clone()));
        }
        if let Some(sid) = &s.sid {
            p.push(("SID", sid.clone()));
        }
        if let Some(aid) = s.last_event_id {
            p.push(("AID", aid.to_string()));
        }
        if let Some(g) = &s.gsession {
            p.push(("gsessionid", g.clone()));
        }
        p
    }

    /// Returns false (after resetting state) if the session is no longer valid.
    fn handle_session_status(&self, status: reqwest::StatusCode) -> bool {
        match status.as_u16() {
            400 | 410 => {
                self.connection_lost();
                false
            }
            401 => {
                self.connection_lost();
                self.token_expired();
                false
            }
            _ => true,
        }
    }

    /// Long-poll the event stream until it ends, goes idle, or the session drops.
    pub async fn subscribe(&self, tx: &UnboundedSender<Event>) -> Result<()> {
        if !self.connected() {
            bail!("not connected");
        }
        let mut params = self.common_params();
        params.extend([
            ("RID", "rpc".into()),
            ("CI", "0".into()),
            ("TYPE", "xmlhttp".into()),
        ]);
        let resp = self
            .http
            .get(format!("{API_BASE}/bc/bind"))
            .query(&params)
            .send()
            .await?;
        if !self.handle_session_status(resp.status()) {
            return Ok(());
        }
        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        let mut parser = ChunkParser::default();
        loop {
            let next = tokio::time::timeout(STREAM_IDLE_TIMEOUT, stream.next())
                .await
                .map_err(|_| anyhow!("no lounge events for {}s", STREAM_IDLE_TIMEOUT.as_secs()))?;
            let Some(bytes) = next else { break };
            buf.extend_from_slice(&bytes.context("reading lounge stream")?);
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = buf.drain(..=pos).collect();
                let line = String::from_utf8_lossy(&line[..line.len() - 1]);
                if let Some(chunk) = parser.push_line(&line) {
                    self.process_chunk(chunk, tx)?;
                    if !self.connected() {
                        return Ok(());
                    }
                }
            }
        }
        tracing::debug!("Subscribe stream ended");
        Ok(())
    }

    /// Handle protocol-level events and forward the rest to the device handler.
    fn process_chunk(&self, chunk: Value, tx: &UnboundedSender<Event>) -> Result<()> {
        let events = chunk
            .as_array()
            .ok_or_else(|| anyhow!("lounge chunk is not an array"))?;
        for event in events {
            let id = event[0].as_i64();
            let Some(body) = event[1].as_array() else {
                continue;
            };
            let kind = body
                .first()
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let args: Vec<Value> = body.iter().skip(1).cloned().collect();
            match kind.as_str() {
                "c" => self.s().sid = args.first().and_then(Value::as_str).map(String::from),
                "S" => self.s().gsession = args.first().and_then(Value::as_str).map(String::from),
                "noop" => {}
                "loungeStatus" => self.handle_lounge_status(&args),
                "loungeScreenDisconnected" => {
                    let _ = tx.send((kind, args));
                    self.connection_lost();
                    self.token_expired();
                }
                _ => {
                    let _ = tx.send((kind, args));
                }
            }
            if id.is_some() {
                self.s().last_event_id = id;
            }
        }
        Ok(())
    }

    fn handle_lounge_status(&self, args: &[Value]) {
        let devices: Vec<Value> = args
            .first()
            .and_then(|a| a["devices"].as_str())
            .and_then(|d| serde_json::from_str(d).ok())
            .unwrap_or_default();
        let Some(device) = devices.iter().find(|d| d["type"] == "LOUNGE_SCREEN") else {
            return;
        };
        if let Some(name) = device["name"].as_str() {
            self.s().screen_name = Some(name.to_string());
        }
        let info: Value = device["deviceInfo"]
            .as_str()
            .and_then(|i| serde_json::from_str(i).ok())
            .unwrap_or(Value::Null);
        if let Some(client) = info["clientName"]
            .as_str()
            .filter(|c| BLACKLISTED_CLIENTS.contains(c))
        {
            tracing::info!("Screen is running unsupported client {client}, disconnecting");
            let mut s = self.s();
            s.sid = None;
            s.gsession = None;
        }
    }

    /// Send a command such as `seekTo` with `req0_*` parameters.
    pub async fn command(&self, command: &str, params: &[(&str, String)]) -> Result<bool> {
        let mut offset = self.command_offset.lock().await;
        if !self.connected() {
            bail!("not connected");
        }
        let mut body = vec![
            ("count".to_string(), "1".to_string()),
            ("ofs".to_string(), offset.to_string()),
            ("req0__sc".to_string(), command.to_string()),
        ];
        body.extend(params.iter().map(|(k, v)| (format!("req0_{k}"), v.clone())));
        *offset += 1;
        let mut query = self.common_params();
        query.push(("RID", offset.to_string()));
        let resp = self
            .http
            .post(format!("{API_BASE}/bc/bind"))
            .query(&query)
            .form(&body)
            .send()
            .await?;
        let status = resp.status();
        let _ = resp.text().await;
        if !self.handle_session_status(status) {
            return Ok(false);
        }
        if !status.is_success() {
            bail!("command {command} failed: {status}");
        }
        Ok(true)
    }

    pub async fn disconnect(&self) -> Result<()> {
        let offset = self.command_offset.lock().await;
        if !self.connected() {
            return Ok(());
        }
        let mut query = self.common_params();
        query.extend([
            ("CVER", "1".into()),
            ("RID", offset.to_string()),
            ("auth_failure_option", "send_error".into()),
        ]);
        self.http
            .post(format!("{API_BASE}/bc/bind"))
            .query(&query)
            .form(&[
                ("ui", ""),
                ("TYPE", "terminate"),
                (
                    "clientDisconnectReason",
                    "MDX_SESSION_DISCONNECT_REASON_DISCONNECTED_BY_USER",
                ),
            ])
            .send()
            .await?;
        self.connection_lost();
        Ok(())
    }

    pub async fn seek_to(&self, time: f64) -> Result<bool> {
        self.command("seekTo", &[("newTime", time.to_string())])
            .await
    }

    pub async fn skip_ad(&self) -> Result<bool> {
        self.command("skipAd", &[]).await
    }

    pub async fn play_video(&self, video_id: &str) -> Result<bool> {
        self.command("setPlaylist", &[("videoId", video_id.to_string())])
            .await
    }

    pub async fn get_now_playing(&self) -> Result<bool> {
        self.command("getNowPlaying", &[]).await
    }

    pub async fn set_auto_play_mode(&self, enabled: bool) -> Result<bool> {
        let mode = if enabled { "ENABLED" } else { "DISABLED" };
        self.command("setAutoplayMode", &[("autoplayMode", mode.into())])
            .await
    }

    pub async fn set_volume(&self, volume: String, muted: bool) -> Result<bool> {
        self.command(
            "setVolume",
            &[("volume", volume), ("muted", muted.to_string())],
        )
        .await
    }
}

/// Parses the BrowserChannel framing: a line holding a length, followed by a JSON array
/// that may span several lines.
///
/// pyytlounge trusts the length prefix exactly (counting Python code points). The unit the
/// server uses is ambiguous for non-ASCII text, so this parser instead emits a chunk as soon
/// as the accumulated text parses as JSON, and only falls back to the length to resync.
#[derive(Default)]
pub struct ChunkParser {
    remaining: Option<i64>,
    buf: String,
}

impl ChunkParser {
    pub fn push_line(&mut self, line: &str) -> Option<Value> {
        let Some(remaining) = self.remaining else {
            if let Ok(len) = line.trim().parse::<i64>() {
                self.remaining = Some(len);
                self.buf.clear();
            }
            return None;
        };
        self.buf.push_str(line);
        let remaining = remaining - line.encode_utf16().count() as i64 - 1;
        self.remaining = Some(remaining);
        match serde_json::from_str::<Value>(&self.buf) {
            Ok(v) => {
                self.remaining = None;
                Some(v)
            }
            Err(_) if remaining <= 0 => {
                tracing::warn!("Discarding unparseable lounge chunk");
                self.remaining = None;
                None
            }
            Err(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_and_multiline_chunks() {
        let mut p = ChunkParser::default();
        let mut out = vec![];
        let input =
            "27\n[[0,[\"c\",\"SID\",\"\",8]]]\n31\n[[1,[\"S\",\n\"gsess\"]],\n[2,[\"noop\"]]]\n";
        for line in input.lines() {
            if let Some(v) = p.push_line(line) {
                out.push(v);
            }
        }
        assert_eq!(out.len(), 2);
        assert_eq!(out[0][0][1][1], "SID");
        assert_eq!(out[1][1][1][0], "noop");
    }

    #[test]
    fn tolerates_wrong_length_with_non_ascii() {
        let mut p = ChunkParser::default();
        // Length counted in bytes rather than characters must still parse.
        let json = "[[3,[\"loungeStatus\",{\"devices\":\"[]\",\"n\":\"Télé 📺\"}]]]";
        assert!(p.push_line(&json.len().to_string()).is_none());
        assert!(p.push_line(json).is_some());
        // Parser is back to expecting a length line.
        assert!(p.push_line("5").is_none());
    }
}
