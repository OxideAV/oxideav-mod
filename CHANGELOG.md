# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.0.4](https://github.com/OxideAV/oxideav-mod/compare/v0.0.3...v0.0.4) - 2026-04-25

### Other

- fix clippy 1.95 lints
- drop oxideav-codec/oxideav-container shims, import from oxideav-core
- add decoder-path playback tests covering vibrato / break / vol slide / speed
- replace "main effects" blurb with spec-coverage matrix
- fill in full ProTracker 1.1B effect coverage
- add vibrato, tone porta, pattern jumps + Exy fine effects
- add vibrato, tone porta, pattern jumps, restart + fine effects
- add volume+panning envelopes, fadeout, and key-off handling
- shared tracker mixer + STM & XM basic playback
- add FastTracker 2 (.xm) structural parser + container
- add Scream Tracker v1 (.stm) structural parser + container
- bump oxideav-container dep to "0.1"
- drop Cargo.lock — this crate is a library
- bump to oxideav-core 0.1.1 + codec 0.1.1
- migrate register() to CodecInfo builder
- bump oxideav-core + oxideav-codec deps to "0.1"
- thread &dyn CodecResolver through open()
