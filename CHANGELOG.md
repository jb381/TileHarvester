# Changelog

All notable changes to this project are documented here, following the Keep a Changelog format.

## [Unreleased]

### Changed

- Leave Strava descriptions untouched for activities with no new Squadrats. When `TH_INCLUDE_SQUADRATINHOS=true`, new Squadratinhos also qualify an activity for annotation.

## [0.1.0] - 2026-11-26

### Changed

- Rewrote TileHarvester from Python to Rust as a single native binary with bundled SQLite 3 and rustls TLS, retaining SQLite schema and CLI compatibility.
- Added drift-free Web Mercator DDA traversal, antimeridian unwrapping, single-pass z17 → z14 derivation, and Liang–Barsky oracle verification.
- Improved GPS cleaning with moving/distance telemetry, spike-burst and endpoint glitch filtering, sub-50 m z17 lateral bounce and coffee-stop drift filtering, and corroborated tunnel bridging. Handle short Strava pagination pages and raise the default `TH_STREAM_MAX_POINTS` to 250,000.
- Added graceful SIGTERM/SIGHUP shutdown and timestamped, immediately flushed continuous sync logs.
- Hardened the non-root Docker image with dependency caching and a health check; simplified Compose with log rotation and a read-only root filesystem. Added multi-architecture GHCR images and release binaries for Linux musl and macOS arm64.
- Updated Rust Docker, CI, Dependabot, and pre-commit pipelines.

### Added

- Online SQLite snapshots with `backup <path> [--force]`.
- Rootless systemd user timers, `--run-as`, and system unit sandboxing.
- Opt-in loopback OAuth callback capture with `auth --listen`.
- Exact Yard, Übersquadrat, Yardinho, and Übersquadratinho spatial analytics in `stats`.
- Edge-bucketed Squadrats KML rasterization, handling dissolved polygons with over 34,000 vertices in approximately 146 ms, and `compare-kml --details <N>`.
- Safe GPX 1.0/1.1 parsing and the offline `inspect-gpx <file>` command.
- Optional Squadratinhos in Strava descriptions with `TH_INCLUDE_SQUADRATINHOS=true`.
- 138 automated tests and a Criterion benchmark suite.
