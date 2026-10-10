# Porting notes

This crate is a hand port of the Python code below. When YouTube changes something and
the Python project ships a fix, diff the upstream sources between the baseline recorded
here and the new release, then apply the same change to the matching Rust module.

## Baseline (what this port matches)

| Source | Version | Identifier |
|---|---|---|
| iSponsorBlockTV (`src/iSponsorBlockTV/`) | 2.11.0 | commit `005d248` (2026-08-21, "Bump version") |
| pyytlounge (PyPI, dependency of the above) | 3.3.0 | sdist `pyytlounge-3.3.0.tar.gz`, sha256 `d19aa1520214b87f67f930df9b977bcfd79ec7cf5e1d30f0805c33add844190a` |

The same baseline is compiled into the binary (`isponsorblocktv --version`, `PORTED_FROM`
in `src/main.rs`). Update all three places when you re-port.

## Checking for upstream changes

```bash
# iSponsorBlockTV: what changed in the Python app since the baseline
git diff 005d248 upstream/main -- src/iSponsorBlockTV requirements.txt

# pyytlounge: compare the ported version against a newer one
pip download --no-deps --no-binary :all: pyytlounge==3.3.0 pyytlounge==<new> -d /tmp/pyyt
cd /tmp/pyyt && for f in *.tar.gz; do tar xzf "$f"; done
diff -ru pyytlounge-3.3.0/src/pyytlounge pyytlounge-<new>/src/pyytlounge
```

Watch `requirements.txt` in iSponsorBlockTV: a `pyytlounge` version bump there is usually
the first sign the Lounge protocol changed.

## Module map

| Rust | Python source |
|---|---|
| `src/lounge.rs` | `pyytlounge/wrapper.py` (pairing, bind, subscribe, commands, chunk parsing) + `iSponsorBlockTV/ytlounge.py` overrides (`connect`, `connect_as_screen`, `_common_connection_parameters`, `_command` mutex, kids-client check) |
| `src/device.rs` | `iSponsorBlockTV/main.py` (`DeviceListener`) + `ytlounge.py` `_process_event` (ads, autoplay, shorts, speed) |
| `src/api.rs` | `iSponsorBlockTV/api_helpers.py` + `conditional_ttl_cache.py` |
| `src/dial.rs` | `iSponsorBlockTV/dial_client.py` |
| `src/config.rs` | `iSponsorBlockTV/helpers.py` (`Config`, `Device`) |
| `src/setup.rs` | `iSponsorBlockTV/config_setup.py` |
| `src/main.rs` | `iSponsorBlockTV/helpers.py` (CLI) + `main.py` (`main_async`, shutdown) |

Protocol constants worth checking on every re-port: the app id `ytios-phone-20.15.1`, the
`connect` body fields, `connectParams`, and the `RID`/`ofs` command-offset handling.

## Not ported

- Textual TUI setup wizard (`setup_wizard.py`): use `isponsorblocktv setup` (CLI prompts).
- Chromecast/Cast discovery (`chromecast_client.py`): devices that don't answer DIAL can
  be added with a "Link with TV code" pairing code.
- `--http-tracing`; the unused `get_vid_id`; PyApp `self update/remove/restore` commands.

## Intentional differences

- **Config file permissions**: written `0600` on Unix (it holds the API key and screen ids).
- **No tokens in logs**: lounge tokens and pairing codes aren't logged, even with `--debug`.
- **Chunk parsing**: emits a chunk as soon as the buffered text is valid JSON instead of
  trusting the length prefix exactly, so non-ASCII text can't desynchronise the stream.
- **Unmute only when `mute_ads` is on**: Python unmutes when ads end even if it never muted.
- **Autoplay**: only re-sends `setAutoplayMode` when the TV reports a different mode.
- **Segments at 0s**: Python skipped nothing when playback was reported at exactly `0.0`
  inside an opening segment (`if 0.0:` is falsy); this port skips it.
- **Segment cache**: "video not found" results expire after 5 minutes instead of being
  cached until evicted; 404s are cached too instead of erroring.
- **Discovery**: SSDP `MX: 3`, listens for 5s and sends the M-SEARCH twice.
- **Setup**: fixes the CLI wizard's crashes (`search_channels` arguments, `atvs` removal),
  adds network discovery, skips devices that are already configured.
- **Errors**: `start` exits with an error instead of sleeping 10s on a bad config.
