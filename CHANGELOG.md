# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.4.0] - 2026-01-01

### Breaking
- Update the `embassy-time` and `embassy-time-queue-utils` dependencies

### Fixed
- Update the `heapless` dependency

## [0.3.2] - 2026-01-01

### Fixed
- Reschedule timer everytime if it gets woken up too early

## [0.3.1] - 2025-08-06

### Fixed
- Change logs in tight places from `debug!` to `trace!`
