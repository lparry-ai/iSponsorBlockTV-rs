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
