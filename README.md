# nikki

A macOS daemon that records what was on screen and what was being done, and ships it to the nikki
service. One binary per Mac, delivered as an `LSUIElement` app bundle.

## Build

```
mise run build      # debug binary
mise run check      # fmt check, clippy with warnings denied, tests
./scripts/bundle.sh # target/nikki.app
```

## Configuration

`~/.config/nikki/config.toml`, or the path in `NIKKI_CONFIG`. `NIKKI_STATE_DIR` overrides where the
buffer database lives (default `~/Library/Application Support/nikki`).

```toml
service_url = "http://alpha:8080"   # required, absolute http or https
device = "mbp-21"                   # required, non-empty
tick_interval = 30                  # seconds, within [1, 3600]
history_poll_interval = 300         # seconds
revisit_window = 500                # visit rows re-read on each poll

[browser]
profile = "MBP_21"                  # display name, not the directory name

[buffer]
max_rows = 200000
max_bytes = 536870912

[[redact]]
url_host = "*"
keep = "host"
```

`nikki --check-config` loads and validates the configuration and exits.

Permissions, the provider model and the wire contract are documented in
`docs/plans/2026-08-25-nikki-daemon.md` until Task 11 moves them here.
