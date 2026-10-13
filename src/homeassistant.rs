//! Home Assistant MQTT Discovery config builder.
//!
//! Port of `homeassistant.py` from Przemysław Szypowicz's MQTT / Home Assistant work
//! (see PORTING.md). Produces the retained discovery messages that make Home Assistant
//! create one device per TV with entities pointing at the topics `mqtt.rs` publishes.

use serde_json::{Value, json};

const MANUFACTURER: &str = "iSponsorBlockTV";
const MODEL: &str = "YouTube TV";

/// Event types the `event` entity can fire (must match what the app emits).
pub const EVENT_TYPES: [&str; 4] = ["segment_skipped", "ad_started", "ad_ended", "ad_skipped"];

/// `(config_topic, json_payload)` messages for one device.
pub fn discovery_messages(
    discovery_prefix: &str,
    base_topic: &str,
    availability_topic: &str,
    device_slug: &str,
    device_name: &str,
) -> Vec<(String, String)> {
    let node = format!("isponsorblocktv_{device_slug}");
    let dev_base = format!("{base_topic}/{device_slug}");
    let name = if device_name.is_empty() {
        device_slug
    } else {
        device_name
    };
    let device = json!({
        "identifiers": [node],
        "name": name,
        "manufacturer": MANUFACTURER,
        "model": MODEL,
    });
    let availability = json!([{
        "topic": availability_topic,
        "payload_available": "online",
        "payload_not_available": "offline",
    }]);

    let cfg = |component: &str, object_id: &str, extra: Value| {
        let topic = format!("{discovery_prefix}/{component}/{node}/{object_id}/config");
        let mut payload = json!({
            "unique_id": format!("{node}_{object_id}"),
            "device": device,
            "availability": availability,
        });
        payload
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        (topic, payload.to_string())
    };

    vec![
        cfg(
            "event",
            "skips",
            json!({
                "name": "Skips",
                "state_topic": format!("{dev_base}/event"),
                "event_types": EVENT_TYPES,
                "icon": "mdi:skip-next",
            }),
        ),
        cfg(
            "sensor",
            "title",
            json!({
                "name": "Title",
                "state_topic": format!("{dev_base}/title"),
                "icon": "mdi:youtube",
            }),
        ),
        cfg(
            "sensor",
            "channel",
            json!({
                "name": "Channel",
                "state_topic": format!("{dev_base}/channel"),
                "icon": "mdi:account-box",
            }),
        ),
        cfg(
            "sensor",
            "playback_state",
            json!({
                "name": "State",
                "state_topic": format!("{dev_base}/playback_state"),
                "icon": "mdi:play-pause",
            }),
        ),
        cfg(
            "sensor",
            "segments_skipped",
            json!({
                "name": "Segments skipped",
                "state_topic": format!("{dev_base}/segments_skipped"),
                "state_class": "total_increasing",
                "icon": "mdi:counter",
            }),
        ),
        cfg(
            "binary_sensor",
            "connected",
            json!({
                "name": "Connected",
                "state_topic": format!("{dev_base}/connected"),
                "payload_on": "ON",
                "payload_off": "OFF",
                "device_class": "connectivity",
            }),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_six_entities_grouped_under_one_device() {
        let msgs = discovery_messages(
            "homeassistant",
            "isponsorblocktv",
            "isponsorblocktv/availability",
            "tv1",
            "Lounge TV",
        );
        assert_eq!(msgs.len(), 6);
        let (topic, payload) = &msgs[0];
        assert_eq!(
            topic,
            "homeassistant/event/isponsorblocktv_tv1/skips/config"
        );
        let v: Value = serde_json::from_str(payload).unwrap();
        assert_eq!(v["unique_id"], "isponsorblocktv_tv1_skips");
        assert_eq!(v["state_topic"], "isponsorblocktv/tv1/event");
        assert_eq!(v["device"]["name"], "Lounge TV");
        assert_eq!(
            v["availability"][0]["topic"],
            "isponsorblocktv/availability"
        );
        let (topic, payload) = &msgs[5];
        assert_eq!(
            topic,
            "homeassistant/binary_sensor/isponsorblocktv_tv1/connected/config"
        );
        assert!(payload.contains("\"device_class\":\"connectivity\""));
    }
}
