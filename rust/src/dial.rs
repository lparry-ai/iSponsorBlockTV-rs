//! DIAL (SSDP) discovery of YouTube screens on the local network (port of `dial_client.py`).
//!
//! Chromecast (Cast protocol) discovery from the Python version is not ported; use a
//! "Link with TV code" pairing code for devices that don't answer DIAL.

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket as StdUdpSocket};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use rand::RngExt;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

use crate::api::ApiHelper;

const SSDP_ADDR: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(239, 255, 255, 250)), 1900);
const SEARCH_TARGET: &str = "urn:dial-multiscreen-org:service:dial:1";
const LISTEN_FOR: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct Discovered {
    pub screen_id: String,
    pub name: String,
}

/// Local IP of the interface that routes outward (needed for multi-homed hosts / Windows).
fn local_ip() -> IpAddr {
    let probe = || -> std::io::Result<IpAddr> {
        let s = StdUdpSocket::bind("0.0.0.0:0")?;
        s.connect("10.254.254.254:1")?;
        Ok(s.local_addr()?.ip())
    };
    probe().unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST))
}

/// Only follow URLs that point back at the device that answered (prevents a LAN
/// responder from making us request arbitrary hosts — upstream's SSRF fix).
fn url_matches_ip(url: &str, expected: IpAddr) -> bool {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|u| {
            u.host_str()
                .and_then(|h| h.trim_matches(['[', ']']).parse::<IpAddr>().ok())
        })
        .is_some_and(|ip| ip == expected)
}

fn xml_text(xml: &str, parent: &str, child: &str) -> Option<String> {
    let doc = roxmltree::Document::parse(xml).ok()?;
    doc.descendants()
        .filter(|n| n.tag_name().name() == parent)
        .flat_map(|n| n.children())
        .find(|n| n.tag_name().name() == child)
        .and_then(|n| n.text())
        .map(|t| t.trim().to_string())
}

fn pairing_code() -> String {
    let mut rng = rand::rng();
    (0..12)
        .map(|_| char::from(b'0' + rng.random_range(0..10u8)))
        .collect()
}

/// Query one DIAL device for its YouTube screen id. In `active` mode, launch the YouTube app
/// with a generated pairing code when the device doesn't advertise a screen id.
async fn find_youtube_app(
    http: &reqwest::Client,
    api: &ApiHelper,
    location: &str,
    sender: IpAddr,
    active: bool,
) -> Result<Option<Discovered>> {
    if !url_matches_ip(location, sender) {
        tracing::debug!("Ignoring DIAL LOCATION {location} from {sender}");
        return Ok(None);
    }
    let resp = http.get(location).send().await?;
    let app_url = resp
        .headers()
        .get("application-url")
        .and_then(|v| v.to_str().ok())
        .map(String::from)
        .ok_or_else(|| anyhow!("no Application-URL header"))?;
    let desc = resp.text().await?;
    let name = xml_text(&desc, "device", "friendlyName").unwrap_or_else(|| sender.to_string());
    if !url_matches_ip(&app_url, sender) {
        tracing::debug!("Ignoring DIAL Application-URL {app_url} from {sender}");
        return Ok(None);
    }
    let youtube_url = format!("{}/YouTube", app_url.trim_end_matches('/'));

    if let Ok(resp) = http
        .get(&youtube_url)
        .header("Origin", "https://www.youtube.com")
        .send()
        .await
        && let Some(id) = resp
            .text()
            .await
            .ok()
            .and_then(|x| xml_text(&x, "additionalData", "screenId"))
    {
        return Ok(Some(Discovered {
            screen_id: id,
            name,
        }));
    }
    if !active {
        return Ok(None);
    }

    let code = pairing_code();
    tracing::debug!("Launching YouTube app on {name} to pair");
    let resp = http
        .post(&youtube_url)
        .header("Origin", "https://www.youtube.com")
        .form(&[("pairingCode", code.as_str()), ("theme", "cl")])
        .send()
        .await?;
    if resp.headers().get("location").is_none() {
        return Ok(None);
    }
    for _ in 0..10 {
        if let Some(screen) = api.pair_with_code(&code).await? {
            return Ok(Some(Discovered {
                screen_id: screen.screen_id,
                name,
            }));
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    Ok(None)
}

/// Discover YouTube screens via DIAL. Results are de-duplicated by screen id.
pub async fn discover(api: Arc<ApiHelper>, active: bool) -> Result<Vec<Discovered>> {
    crate::init_tls();
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()?;
    let socket = UdpSocket::bind(SocketAddr::new(local_ip(), 0))
        .await
        .context("binding SSDP socket")?;
    let search = format!(
        "M-SEARCH * HTTP/1.1\r\nHOST: {SSDP_ADDR}\r\nMAN: \"ssdp:discover\"\r\nMX: 3\r\nST: {SEARCH_TARGET}\r\n\r\n"
    );
    // UDP is lossy; send twice.
    for _ in 0..2 {
        socket.send_to(search.as_bytes(), SSDP_ADDR).await.context(
            "sending SSDP multicast (on macOS, check Local Network permission for your terminal)",
        )?;
    }

    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut seen_locations = HashSet::new();
    let deadline = tokio::time::Instant::now() + LISTEN_FOR;
    let mut buf = [0u8; 4096];
    let mut tasks = vec![];
    while let Ok(Ok((n, from))) =
        tokio::time::timeout_at(deadline, socket.recv_from(&mut buf)).await
    {
        let text = String::from_utf8_lossy(&buf[..n]);
        let location = text.lines().find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim()
                .eq_ignore_ascii_case("location")
                .then(|| v.trim().to_string())
        });
        let Some(location) = location else { continue };
        if !seen_locations.insert(location.clone()) {
            continue;
        }
        tracing::debug!("Discovered DIAL device at {location}");
        let (http, api, tx) = (http.clone(), api.clone(), tx.clone());
        tasks.push(tokio::spawn(async move {
            match find_youtube_app(&http, &api, &location, from.ip(), active).await {
                Ok(Some(d)) => {
                    let _ = tx.send(d);
                }
                Ok(None) => {}
                Err(e) => tracing::debug!("{location}: {e:#}"),
            }
        }));
    }
    drop(tx);
    for t in tasks {
        let _ = t.await;
    }
    let mut seen = HashSet::new();
    let mut out = vec![];
    while let Some(d) = rx.recv().await {
        if seen.insert(d.screen_id.clone()) {
            out.push(d);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_must_point_at_sender() {
        let ip: IpAddr = "192.168.1.20".parse().unwrap();
        assert!(url_matches_ip(
            "http://192.168.1.20:8008/ssdp/device-desc.xml",
            ip
        ));
        assert!(!url_matches_ip("http://192.168.1.21:8008/", ip));
        assert!(!url_matches_ip("http://example.com/", ip));
        assert!(!url_matches_ip("not a url", ip));
    }

    #[test]
    fn extracts_namespaced_xml() {
        let desc = r#"<?xml version="1.0"?><root xmlns="urn:schemas-upnp-org:device-1-0">
            <device><friendlyName> Living Room TV </friendlyName></device></root>"#;
        assert_eq!(
            xml_text(desc, "device", "friendlyName").unwrap(),
            "Living Room TV"
        );
        let svc = r#"<service xmlns="urn:dial-multiscreen-org:schemas:dial">
            <name>YouTube</name><additionalData><screenId>abc123</screenId></additionalData></service>"#;
        assert_eq!(
            xml_text(svc, "additionalData", "screenId").unwrap(),
            "abc123"
        );
    }

    #[test]
    fn pairing_code_is_12_digits() {
        let c = pairing_code();
        assert_eq!(c.len(), 12);
        assert!(c.chars().all(|c| c.is_ascii_digit()));
    }
}
