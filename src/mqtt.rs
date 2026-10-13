//! MQTT notifications (opt-in, off by default).
//!
//! Port of `notifications.py` from Przemysław Szypowicz's MQTT / Home Assistant work
//! (<https://github.com/pszypowicz/iSponsorBlockTV/tree/feat/mqtt-notifications>, see
//! PORTING.md). Publishes events and retained state under `base_topic`; Home Assistant
//! discovery is layered on top in `homeassistant.rs`.
//!
//! Every `emit`/`set_*` call is fire-and-forget: it only enqueues, so it never blocks or
//! fails in the playback path. One background task owns the connection, reconnecting as
//! needed. Availability uses an MQTT Last Will so consumers see unclean exits.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rumqttc::{
    AsyncClient, Event, EventLoop, LastWill, MqttOptions, Outgoing, Packet, QoS, TlsConfiguration,
    Transport,
};
use serde_json::{Map, Value};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::config::Config;
use crate::homeassistant;

const QUEUE_SIZE: usize = 200;
const RECONNECT_DELAY: Duration = Duration::from_secs(10);
const STOP_TIMEOUT: Duration = Duration::from_secs(6);

/// Stable, topic-safe id from a screen id / name.
pub fn slugify(value: &str) -> String {
    let mut out = String::new();
    let mut gap = false;
    for c in value.chars() {
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
            if gap && !out.is_empty() {
                out.push('_');
            }
            gap = false;
            out.push(c.to_ascii_lowercase());
        } else {
            gap = true;
        }
    }
    let out = out.trim_matches('_').to_string();
    if out.is_empty() { "device".into() } else { out }
}

struct Inner {
    base_topic: String,
    queue: mpsc::Sender<(String, String, bool)>,
    /// Retained state, re-published after a reconnect in case the broker lost it.
    state: Mutex<HashMap<String, String>>,
    skip_counts: Mutex<HashMap<String, u64>>,
    client: AsyncClient,
    stopping: AtomicBool,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

#[derive(Clone)]
pub struct Notifier(Arc<Inner>);

impl Notifier {
    /// Connect in the background if `mqtt.enabled`; returns None when disabled.
    pub fn start(config: &Config) -> Option<Notifier> {
        let m = &config.mqtt;
        if !m.enabled {
            return None;
        }
        let base_topic = match m.base_topic.trim_matches('/') {
            "" => "isponsorblocktv".to_string(),
            t => t.to_string(),
        };
        let availability = format!("{base_topic}/availability");
        let mut opts = MqttOptions::new(
            format!("isponsorblocktv-{}", slugify(&base_topic)),
            &m.broker,
            m.port,
        );
        opts.set_keep_alive(Duration::from_secs(30));
        opts.set_last_will(LastWill::new(
            &availability,
            "offline",
            QoS::AtLeastOnce,
            true,
        ));
        if !m.username.is_empty() {
            opts.set_credentials(&m.username, &m.password);
        }
        if m.tls {
            crate::init_tls();
            use rustls_platform_verifier::ConfigVerifierExt;
            match rustls::ClientConfig::with_platform_verifier() {
                Ok(tls) => {
                    opts.set_transport(Transport::tls_with_config(TlsConfiguration::Rustls(
                        Arc::new(tls),
                    )));
                }
                Err(e) => {
                    tracing::error!("MQTT TLS setup failed ({e}); MQTT disabled");
                    return None;
                }
            }
        }
        let (client, eventloop) = AsyncClient::new(opts, 256);
        let (tx, rx) = mpsc::channel(QUEUE_SIZE);
        let notifier = Notifier(Arc::new(Inner {
            base_topic,
            queue: tx,
            state: Mutex::default(),
            skip_counts: Mutex::default(),
            client,
            stopping: AtomicBool::new(false),
            tasks: Mutex::default(),
        }));
        let discovery = if m.home_assistant.enabled {
            let prefix = match m.home_assistant.discovery_prefix.trim_matches('/') {
                "" => "homeassistant",
                p => p,
            };
            config
                .devices
                .iter()
                .flat_map(|d| {
                    homeassistant::discovery_messages(
                        prefix,
                        &notifier.0.base_topic,
                        &availability,
                        &slugify(&d.screen_id),
                        &d.name,
                    )
                })
                .collect()
        } else {
            vec![]
        };
        let device_count = config.devices.len();
        let run = tokio::spawn(notifier.clone().run(
            eventloop,
            availability,
            discovery,
            device_count,
            m.broker.clone(),
            m.port,
        ));
        let forward = tokio::spawn(notifier.clone().forward(rx));
        notifier.0.tasks.lock().unwrap().extend([run, forward]);
        tracing::debug!("MQTT notifier started (broker {}:{})", m.broker, m.port);
        Some(notifier)
    }

    fn device_topic(&self, device_id: &str, leaf: &str) -> String {
        format!("{}/{}/{}", self.0.base_topic, slugify(device_id), leaf)
    }

    fn enqueue(&self, topic: String, payload: String, retain: bool) {
        if self
            .0
            .queue
            .try_send((topic.clone(), payload, retain))
            .is_err()
        {
            tracing::debug!("MQTT queue full; dropping message to {topic}");
        }
    }

    fn publish_state(&self, topic: String, payload: String) {
        self.0
            .state
            .lock()
            .unwrap()
            .insert(topic.clone(), payload.clone());
        self.enqueue(topic, payload, true);
    }

    /// Publish an event (`segment_skipped`, `ad_started`, ...) to `<base>/<device>/event`.
    /// `None` fields are left out of the JSON payload.
    pub fn emit(
        &self,
        event_type: &str,
        device_id: &str,
        device_name: &str,
        fields: &[(&str, Value)],
    ) {
        let did = slugify(device_id);
        let mut payload = Map::new();
        payload.insert("event_type".into(), event_type.into());
        let name = if device_name.is_empty() {
            did.clone()
        } else {
            device_name.to_string()
        };
        payload.insert("device".into(), name.into());
        for (k, v) in fields.iter().filter(|(_, v)| !v.is_null()) {
            payload.insert((*k).into(), v.clone());
        }
        self.enqueue(
            self.device_topic(&did, "event"),
            Value::Object(payload).to_string(),
            false,
        );
        if event_type == "segment_skipped" {
            let count = {
                let mut counts = self.0.skip_counts.lock().unwrap();
                let c = counts.entry(did.clone()).or_default();
                *c += 1;
                *c
            };
            self.publish_state(
                self.device_topic(&did, "segments_skipped"),
                count.to_string(),
            );
        }
    }

    /// Connectivity binary sensor. State only, not sent to the event topic.
    pub fn set_connected(&self, device_id: &str, connected: bool) {
        let v = if connected { "ON" } else { "OFF" };
        self.publish_state(self.device_topic(device_id, "connected"), v.into());
    }

    /// Video title (or the raw video id until it resolves); empty when idle.
    pub fn set_title(&self, device_id: &str, value: &str) {
        self.publish_state(self.device_topic(device_id, "title"), value.into());
    }

    /// Channel name; empty when idle.
    pub fn set_channel(&self, device_id: &str, value: &str) {
        self.publish_state(self.device_topic(device_id, "channel"), value.into());
    }

    /// playing / paused / stopped / ...
    pub fn set_playback_state(&self, device_id: &str, value: &str) {
        self.publish_state(self.device_topic(device_id, "playback_state"), value.into());
    }

    /// Move queued messages into the MQTT client (which buffers them across reconnects).
    async fn forward(self, mut rx: mpsc::Receiver<(String, String, bool)>) {
        while let Some((topic, payload, retain)) = rx.recv().await {
            if let Err(e) = self
                .0
                .client
                .publish(topic, QoS::AtMostOnce, retain, payload)
                .await
            {
                tracing::debug!("MQTT publish failed: {e}");
            }
        }
    }

    async fn run(
        self,
        mut eventloop: EventLoop,
        availability: String,
        discovery: Vec<(String, String)>,
        device_count: usize,
        broker: String,
        port: u16,
    ) {
        let client = &self.0.client;
        loop {
            match eventloop.poll().await {
                Ok(Event::Incoming(Packet::ConnAck(_))) => {
                    tracing::info!("Connected to MQTT broker {broker}:{port}");
                    let mut burst =
                        vec![(availability.clone(), "online".to_string(), QoS::AtLeastOnce)];
                    burst.extend(
                        discovery
                            .iter()
                            .map(|(t, p)| (t.clone(), p.clone(), QoS::AtLeastOnce)),
                    );
                    let state: Vec<_> = self.0.state.lock().unwrap().clone().into_iter().collect();
                    burst.extend(state.into_iter().map(|(t, p)| (t, p, QoS::AtMostOnce)));
                    for (topic, payload, qos) in burst {
                        if let Err(e) = client.try_publish(topic, qos, true, payload) {
                            tracing::debug!("MQTT publish failed: {e}");
                        }
                    }
                    if !discovery.is_empty() {
                        tracing::info!(
                            "Published Home Assistant discovery: {} entities across {device_count} device(s)",
                            discovery.len()
                        );
                    }
                }
                Ok(Event::Outgoing(Outgoing::Disconnect)) => return,
                Ok(_) => {}
                Err(e) => {
                    if self.0.stopping.load(Ordering::SeqCst) {
                        return;
                    }
                    tracing::warn!(
                        "MQTT connection error ({e}); reconnecting in {}s",
                        RECONNECT_DELAY.as_secs()
                    );
                    tokio::time::sleep(RECONNECT_DELAY).await;
                }
            }
        }
    }

    /// Mark offline and disconnect cleanly (the Last Will only fires on unclean exits).
    pub async fn stop(&self) {
        self.0.stopping.store(true, Ordering::SeqCst);
        let availability = format!("{}/availability", self.0.base_topic);
        let _ = self
            .0
            .client
            .try_publish(availability, QoS::AtLeastOnce, true, "offline");
        let _ = self.0.client.try_disconnect();
        let tasks: Vec<_> = std::mem::take(&mut *self.0.tasks.lock().unwrap());
        let mut tasks = tasks.into_iter();
        if let Some(run) = tasks.next()
            && tokio::time::timeout(STOP_TIMEOUT, run).await.is_err()
        {
            tracing::debug!("MQTT did not disconnect cleanly in time");
        }
        for t in tasks {
            t.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::slugify;

    #[test]
    fn slugify_matches_python() {
        assert_eq!(slugify("Living Room TV"), "living_room_tv");
        assert_eq!(slugify("abc-DEF_123"), "abc-def_123");
        assert_eq!(slugify("  ##  "), "device");
        assert_eq!(slugify("__a!!b__"), "a_b");
    }
}

/// End-to-end check against an embedded broker (rumqttd): connect, availability,
/// Home Assistant discovery, events, retained state and the clean "offline" on stop.
#[cfg(test)]
mod broker_tests {
    use super::*;
    use std::collections::HashMap;

    fn start_broker() -> u16 {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let toml = format!(
            r#"
            id = 0
            [router]
            id = 0
            max_connections = 100
            max_outgoing_packet_count = 200
            max_segment_size = 1048576
            max_segment_count = 10
            [v4.1]
            name = "v4-1"
            listen = "127.0.0.1:{port}"
            next_connection_delay_ms = 1
            [v4.1.connections]
            connection_timeout_ms = 60000
            max_payload_size = 20480
            max_inflight_count = 100
            dynamic_filters = true
            "#
        );
        let config: rumqttd::Config = toml::from_str(&toml).unwrap();
        std::thread::spawn(move || rumqttd::Broker::new(config).start().unwrap());
        std::thread::sleep(Duration::from_millis(300));
        port
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn publishes_discovery_events_and_state() {
        let port = start_broker();

        // Subscriber that records the last payload per topic.
        let (sub, mut sub_loop) =
            AsyncClient::new(MqttOptions::new("test-sub", "127.0.0.1", port), 100);
        sub.subscribe("isponsorblocktv/#", QoS::AtLeastOnce)
            .await
            .unwrap();
        sub.subscribe("homeassistant/#", QoS::AtLeastOnce)
            .await
            .unwrap();
        let seen = Arc::new(Mutex::new(HashMap::<String, Vec<String>>::new()));
        let seen2 = seen.clone();
        tokio::spawn(async move {
            while let Ok(ev) = sub_loop.poll().await {
                if let Event::Incoming(Packet::Publish(p)) = ev {
                    let payload = String::from_utf8_lossy(&p.payload).to_string();
                    seen2
                        .lock()
                        .unwrap()
                        .entry(p.topic.clone())
                        .or_default()
                        .push(payload);
                }
            }
        });
        tokio::time::sleep(Duration::from_millis(300)).await;

        let config: Config = serde_json::from_value(serde_json::json!({
            "devices": [{"screen_id": "Screen ABC", "name": "Lounge TV"}],
            "mqtt": {"enabled": true, "broker": "127.0.0.1", "port": port}
        }))
        .unwrap();
        let n = Notifier::start(&config).unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;

        n.set_connected("Screen ABC", true);
        n.set_playback_state("Screen ABC", "playing");
        n.set_title("Screen ABC", "Some video");
        n.emit(
            "segment_skipped",
            "Screen ABC",
            "Lounge TV",
            &[("category", "sponsor".into()), ("video_id", Value::Null)],
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
        n.stop().await;
        tokio::time::sleep(Duration::from_millis(300)).await;

        let seen = seen.lock().unwrap();
        let last = |t: &str| {
            seen.get(t)
                .and_then(|v| v.last())
                .cloned()
                .unwrap_or_default()
        };
        assert_eq!(
            seen["isponsorblocktv/availability"].first().unwrap(),
            "online"
        );
        assert_eq!(last("isponsorblocktv/availability"), "offline");
        assert_eq!(last("isponsorblocktv/screen_abc/connected"), "ON");
        assert_eq!(last("isponsorblocktv/screen_abc/playback_state"), "playing");
        assert_eq!(last("isponsorblocktv/screen_abc/title"), "Some video");
        assert_eq!(last("isponsorblocktv/screen_abc/segments_skipped"), "1");
        let event: Value = serde_json::from_str(&last("isponsorblocktv/screen_abc/event")).unwrap();
        assert_eq!(event["event_type"], "segment_skipped");
        assert_eq!(event["device"], "Lounge TV");
        assert_eq!(event["category"], "sponsor");
        assert!(event.get("video_id").is_none());
        let discovery = seen
            .keys()
            .filter(|t| t.starts_with("homeassistant/"))
            .count();
        assert_eq!(
            discovery,
            6,
            "topics: {:?}",
            seen.keys().collect::<Vec<_>>()
        );
    }
}
