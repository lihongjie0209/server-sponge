# Changelog

## [0.6.3] - 2026-08-28

### Changed

- `install` no longer sends service stdout/stderr to `journalctl` by default.
- Add `install --journal` to explicitly enable systemd journal output.

## [0.6.2] - 2026-08-27

### Security

- Bind the monitoring web server to loopback by default so unauthenticated stress controls are not exposed on all network interfaces.
- Reject non-finite memory, CPU, panic-threshold, and PID configuration values.
- Remove mutex locking from the signal handler and make process-name FFI input NUL-terminated and UTF-8 safe.

### Fixed

- Preserve logging, HugePages, and stealth options when generating the systemd service command.
- Quote systemd executable and argument paths containing whitespace.
- Stabilize the global log-buffer deduplication test under parallel execution.
- Resolve strict Clippy warnings and format the source tree with rustfmt.
