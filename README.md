# iSponsorBlockTV (Rust)

Skips sponsor segments in YouTube videos playing on a TV. It connects to the YouTube app on
Apple TV, smart TVs, Chromecast with Google TV and similar devices the same way the phone
remote does, looks segments up on [SponsorBlock](https://sponsor.ajay.app), and seeks past
them. It can also mute or skip native YouTube ads.

This is a Rust port of [dmunozv04/iSponsorBlockTV](https://github.com/dmunozv04/iSponsorBlockTV),
which is written in Python. It uses the same `config.json`, Docker volume layout and arguments,
so it's a drop-in replacement. See [PORTING.md](PORTING.md) for the exact upstream versions it
matches, what isn't ported and the intentional differences.

## Docker

```bash
docker run --rm -it -v /path/to/data:/app/data ghcr.io/lparry-ai/isponsorblocktv-rs --setup-cli
docker run -d --restart unless-stopped -v /path/to/data:/app/data ghcr.io/lparry-ai/isponsorblocktv-rs
```

Or use [docker-compose.yml](docker-compose.yml). Network discovery during setup needs
`--network host`; pairing with a TV code works without it. Images are published for
amd64, arm64, arm/v7, arm/v6 and 386.

The image is `FROM scratch`: a static binary and the CA bundle (about 2 MB). Like the Python
image it runs as root by default so it can write to existing data volumes; if your data dir is
owned by another user, add `--user <uid>:<gid>` (compose: `user:`).

To build it yourself:

```bash
docker build -t isponsorblocktv-rs .
```

## Without Docker

```bash
cargo build --release
./target/release/isponsorblocktv setup      # pair devices and choose options
./target/release/isponsorblocktv            # start skipping (same as `start`)
./target/release/isponsorblocktv discover   # list devices advertising a YouTube screen id
```

Options: `-d/--data <dir>` (or `iSPBTV_data_dir`) sets the config directory; `--debug`
turns on verbose logs (or use `RUST_LOG`). The default config location matches the Python
version: `~/Library/Application Support/iSponsorBlockTV` on macOS,
`~/.local/share/iSponsorBlockTV` on Linux. On macOS, network discovery needs the Local
Network permission for your terminal.

## Configuration

`setup` writes `config.json`; [config.json.template](config.json.template) shows every option.
The file holds your TV screen ids (enough to control playback on that TV) and any YouTube API
key, so it is written readable only by you. Keep it out of version control.

Skips happen instantly by default. If that's jarring, `skip_delay` keeps a segment playing
for that many seconds before skipping (so you can tell it was a sponsor), and
`skip_end_margin` lands that many seconds before the segment ends (so you don't miss the
start of the video coming back). For example `"skip_delay": 3, "skip_end_margin": 1`.
Segments too short to leave anything to skip after both are applied are left to play.

With `skip_count_tracking` on (the default), each skip tells SponsorBlock the segment was
used, which reveals the video you watched along with your IP address. Set it to `false` if
you'd rather not.

## MQTT / Home Assistant

Optional and off by default. Add an `mqtt` block to `config.json` (only `enabled` and
`broker` are required; everything else shows its default):

```json
"mqtt": {
    "enabled": true,
    "broker": "mqtt.example.lan",
    "port": 1883,
    "username": "",
    "password": "",
    "tls": false,
    "base_topic": "isponsorblocktv",
    "home_assistant": {
        "enabled": true,
        "discovery_prefix": "homeassistant"
    }
}
```

Topics, with `<device>` being the device's screen id made topic-safe:

| Topic | Retained | Payload |
|---|---|---|
| `<base_topic>/availability` | yes | `online` / `offline` (Last Will on unclean exit) |
| `<base_topic>/<device>/event` | no | JSON event: `segment_skipped` (with `video_id`, `category`, `skipped_from`, `skipped_to`), `ad_started`, `ad_ended`, `ad_skipped` |
| `<base_topic>/<device>/title` | yes | video title (the video id until resolved), empty when idle |
| `<base_topic>/<device>/channel` | yes | channel name, empty when idle |
| `<base_topic>/<device>/playback_state` | yes | `playing`, `paused`, `buffering`, `starting`, `advertisement`, `stopped`, `unknown` |
| `<base_topic>/<device>/segments_skipped` | yes | count since start |
| `<base_topic>/<device>/connected` | yes | `ON` / `OFF` |

With `home_assistant.enabled`, it also publishes
[MQTT Discovery](https://www.home-assistant.io/integrations/mqtt/#mqtt-discovery) configs,
so Home Assistant creates one device per TV with a **Skips** event entity, **Title**,
**Channel**, **State** and **Segments skipped** sensors and a **Connected** binary sensor,
no YAML needed. Title and channel are looked up with the YouTube Data API if `apikey` is
set, otherwise with YouTube's keyless oEmbed endpoint. Segments skipped starts at 0 for each
TV when the app starts.
`ad_started` is only sent when `mute_ads` is on and `ad_skipped` only when `skip_ads` is on.
With `tls`, the broker certificate is checked against the system CA store.

This feature is a port of
[Przemysław Szypowicz's MQTT work](https://github.com/pszypowicz/iSponsorBlockTV/tree/feat/mqtt-notifications)
for the Python version; topics, payloads and entities are the same.

## Development

```bash
cargo test                    # offline unit tests
cargo test -- --ignored       # also hit the live SponsorBlock / YouTube pairing APIs
cargo clippy --all-targets -- -D warnings
```

CI (`.github/workflows/build.yml`) runs clippy and the tests, then builds and pushes the
multi-arch image to `ghcr.io/<owner>/isponsorblocktv-rs`.

## License

GPL-3.0, like the upstream project. See [LICENSE.md](LICENSE.md) and
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
