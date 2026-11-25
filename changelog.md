# Changelog

## [1.1.0] - 2026-11-25

### Added
- **S1** (`crates/server/src/signaling.rs`): WebSocket signaling server (`--ws`, `--ws-start-delay-ms`, `--ws-punch-duration-ms`) supporting room-based peer pairing (`MAX_ROOMS = 64`), 120s waiting TTL, coordinated `StartPunch`, and explicit `LeaveRoom` -> `PeerLeft` forwarding.
- **C2** (`crates/client-core/src/signaling_client.rs`): Signaling WebSocket client with `pair_in_room` and cancel-safe `SignalGuard` (`wait_peer_left`, `send_leave_and_close`, `close`).
- **C3** (`crates/client-core/src/p2p_engine.rs`): Dual-stack isolated UDP socket P2P probe engine (`SessionHandle`) with `HostGlobalV6`/`HostLocal`/reflexive candidate collection, 15s keepalive, `StandardUdpV6First` simultaneous hole punching, fixed-window bidirectional sampling, and `P2pVerdict` grading.
- **C1 / C6 / U1**: `Orchestrator::start_p2p_assessment` 5-stage lifecycle, `has_p2p` / `p2p_verdict` summary fields in `LocalStore`, and `neteval-cli p2p` subcommand.
- **protocol**: Forward-compatible `S1`/`C3` wire types, `P2pTaskInput`, `P2pReport`, `validate_room_code`, and optional `p2p_report` fields on `AssessmentReport` / `ReportSummary`.
- **CI/CD**: `.github/workflows/release.yml` building Linux (`x86_64-unknown-linux-musl`) and Windows (`x86_64`) server & CLI release binaries on `v*` tags.

### Changed
- Replaced hand-rolled RFC 6455 WebSocket stacks in `S1`, `C2`, and `e2e_v1_1_p2p` with `tokio-tungstenite 0.30` and `futures-util 0.3`.
- Synchronized workspace version in `Cargo.toml` and `README.md` to `1.1.0`.

## [1.0.0] - 2026-11-23

### Added
- **protocol**: Initial core data contracts (`NatReport`, `AssessmentReport`, `S2WireMessage`, `ProbeError`, `ReportSummary`).
- **S2** (`crates/server/src/nat_echo.rs`): Dual-UDP-port echo responder (`neteval-server`) supporting `reply_from_alt_port` and authenticated `change_ip_hint` forwarding with amplification caps.
- **C4** (`crates/client-core/src/nat_prober.rs`): RFC 5389 public STUN + self-hosted S2 NAT profiler distinguishing Cone NAT1/NAT2/NAT3 vs Symmetric NAT4 and IPv6 reachability.
- **C6 / C1 / U1**: Local JSON report storage (`LocalStore`), single-task assessment orchestrator (`Orchestrator`), and `neteval-cli` (`nat`, `history`).
