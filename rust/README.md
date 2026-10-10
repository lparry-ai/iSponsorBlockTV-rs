# iSponsorBlockTV (Rust port)

A Rust port of [iSponsorBlockTV](https://github.com/dmunozv04/iSponsorBlockTV): it connects
to YouTube TV apps (Apple TV, smart TVs, Chromecast with Google TV…) the way the phone remote
does, and skips SponsorBlock segments. It optionally mutes or skips native YouTube ads.

See [PORTING.md](PORTING.md) for the exact upstream versions this matches, what isn't ported
and the intentional differences.

## Build

```bash
cargo build --release
```

The binary is `target/release/isponsorblocktv`.

## Use

```bash
isponsorblocktv setup      # pair devices (network discovery or TV code) and choose options
isponsorblocktv            # start skipping (same as `isponsorblocktv start`)
isponsorblocktv discover   # list devices on the network that advertise a YouTube screen id
```

Options: `-d/--data <dir>` (or `iSPBTV_data_dir`) sets the config directory; `--debug`
turns on verbose logs (or use `RUST_LOG`).

It reads and writes the same `config.json` as the Python version, in the same default
location (`~/Library/Application Support/iSponsorBlockTV` on macOS, `~/.local/share/iSponsorBlockTV`
on Linux), so you can switch between the two.

On macOS, network discovery needs the Local Network permission for your terminal.

## Test

```bash
cargo test                    # offline unit tests
cargo test -- --ignored       # also hit the live SponsorBlock / YouTube pairing APIs
```

## Docker

The image is a drop-in replacement for the Python one: same `/app/data` volume, same
`config.json`, same arguments.

```bash
docker run --rm -it -v /path/to/data:/app/data ghcr.io/lparry-ai/isponsorblocktv-rs:rust-port --setup-cli
docker run -d --restart unless-stopped -v /path/to/data:/app/data ghcr.io/lparry-ai/isponsorblocktv-rs:rust-port
```

Or swap the image name in your existing `docker-compose.yml` (see the one in this folder).
Network discovery during setup needs `--network host`, as with the Python image; pairing
with a TV code works without it.

The image is `FROM scratch`: a static binary and the CA bundle, nothing else. Like the Python
image it runs as root by default so it can write to existing data volumes; if your data dir is
owned by another user, add `--user <uid>:<gid>` (compose: `user:`).

To build locally: `docker buildx build --platform linux/amd64,linux/arm64 -t isponsorblocktv-rs rust/`
