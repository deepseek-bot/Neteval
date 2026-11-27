# Changelog

## [1.3.3] - 2026-11-27

### Added
- **Dual-Dimension P2P Link Quality Assessment (`crates/protocol`, `client-core`, `cli`, `apps/gui`)**: Split P2P link evaluation into an average-RTT latency tier (`UltraLow` `<30ms`, `Low` `30–80ms`, `Medium` `80–150ms`, `High` `>150ms`) and a 4-tier stability grade (`RockSolid`, `Good`, `MildJitter`, `SevereJitter`, plus `Failed`) backed by `p50_rtt_ms`, `iqr_ms` (`P75 - P25`), `tail_bloat_ms` (`P95 - P50`), and `spike_ratio` (`RTT > P50 + 30ms`), displayed in CLI output and GUI `ResultModal` / `P2pView`.

### Changed
- **Simplified Self-Hosted Server UI (`apps/gui/src/components/AdvancedSettingsPanel.vue`, `appState.ts`)**: Removed UDP probe port (`34778`/`34779`) and helper node inputs from Advanced Settings so clients only configure server domain/IP (+ optional token, and signaling protocol/port for P2P) while keeping probe ports as internal defaults.

### Fixed
- **Cross-Ocean Tail Packet Loss (`crates/client-core/src/p2p_engine.rs`)**: Replaced the fixed 200ms tail grace timeout at 10 Hz in Phase C sampling with a dynamic grace deadline scaled from observed max RTT (`clamp(400ms, 2 * max_rtt, 1500ms)`) with early exit once all packets return.

## [1.3.2] - 2026-11-27

### Changed
- **Latency Chart Y-Axis Granularity (`apps/gui/src/components/LatencyChart.vue`)**: Replaced the 2-interval (`0`, `max/2`, `max`) Y-axis ticks with a dynamic nice-step tick generator (4 intervals / 5 ticks in compact mobile view, 5 intervals / 6 ticks on desktop, supporting `0.5 ms` steps under `4 ms`), dashed reference grid lines, and wider left padding.
- **Mobile & Android Touch Sizing (`apps/gui/src/style.css`, `App.vue`, `NatView.vue`, `P2pView.vue`, `HistoryView.vue`, `AdvancedSettingsPanel.vue`, `ResultModal.vue`, `appState.ts`)**: Enlarged Android/mobile typography, bottom navigation tabs, top bar, buttons (`min-height: 44px`), inputs, selects, sliders, and action cards, and locked Android User-Agent devices to mobile layout across orientations via `html.is-mobile`.
- **NAT Output & Advanced Settings Formatting (`apps/gui/src/utils/format.ts`, `NatView.vue`, `ResultModal.vue`, `AdvancedSettingsPanel.vue`)**: Split `Observed IPv4 mappings:` result notes into clean line-by-line endpoint-to-mapped rows in both `NatView` and `ResultModal`, and replaced the raw `helper_v4_p1` identifier in Advanced Settings with plain UI labels.

## [1.3.1] - 2026-11-26

### Fixed
- **Mobile & Responsive Chart UI (`apps/gui/src/components/LatencyChart.vue`, `P2pView.vue`, `ResultModal.vue`)**: Replaced fixed `800x260` `preserveAspectRatio="none"` SVG scaling with a `ResizeObserver`-driven 1:1 `viewBox`, anchored X-axis edge labels against clipping, and constrained portrait mobile chart height to prevent vertical stretching of axis text and curves.

## [1.3.0] - 2026-11-26

### Added
- **Server CLI (`crates/server/src/main.rs`)**: Added `neteval-server start [ws:<port>]` subcommand with dual-stack defaults (`34778/udp` primary probe/echo, `34779/udp` secondary probe, `34780/tcp` WS signaling), `--probe`, `--no-probe`, `--no-ipv6`, and full `--help` examples while keeping legacy flags backward-compatible.
- **Punch Strategy (`crates/protocol`, `client-core`, `cli`, `apps/gui`)**: Extended `PunchStrategy` with `Ipv4Only` and `Ipv6Only` (`StandardUdpV6First` default), enforced strict stack filtering during candidate gathering and punching (`prepare_session_with_strategy` + `execute_probe`), added `neteval-cli p2p --strategy`, and exposed a strategy selector in GUI Advanced Settings.
- **Android APK Release Signing (`.github/workflows/release.yml`, `gui-ci.yml`)**: Added `zipalign` + `apksigner sign` + `apksigner verify` steps to ship signed arm64 APKs.

### Changed
- **Unified GUI Server Configuration (`apps/gui`)**: Consolidated NAT and P2P endpoint forms into a single `serverForm` (`ws://` protocol dropdown, paste-aware host/domain field, configurable `34780`/`34778`/`34779` ports, collapsed optional helper node) with automatic dual-stack endpoint assembly and legacy `localStorage` migration.
- **Default Ports & Docs (`README.md`)**: Updated default server ports (`34778/udp`, `34779/udp`, `34780/tcp`) and bilingual documentation.

### Fixed
- **Server Dual-Stack Binding (`crates/server/src/nat_echo.rs`, `signaling.rs`)**: Explicitly set `IPV6_V6ONLY = true` via `socket2` for both UDP and TCP IPv6 listeners so Linux hosts with `net.ipv6.bindv6only = 0` can bind IPv4 and IPv6 on the same port without `Address in use (os error 98)`.

## [1.2.1] - 2026-11-25

### Fixed
- **Windows GUI (`apps/gui/src-tauri`)**: Added `custom-protocol = ["tauri/custom-protocol"]` Cargo feature, a compile-time `build.rs` guard rejecting release builds without `custom-protocol` (`DEP_TAURI_DEV == "true"`), and `--features custom-protocol` in `.github/workflows/gui-ci.yml` and `.github/workflows/release.yml` so release Windows executables load embedded frontend assets instead of `localhost:1420`.
- **Frontend (`apps/gui/src/stores/appState.ts`, `NatView.vue`)**: Cleared stale `latestNatReport` (and P2P live state) only once a new task ID starts (`resetForNewTask`), addressing Codex PR #1 P2 feedback without dropping prior results on synchronous start validation errors.
- **Orchestrator (`crates/client-core/src/orchestrator.rs`)**: Retained an internal `watch::Receiver` on `OrchestratorInner` so `Orchestrator::state()` always reflects live transitions even when no external `subscribe()` listener is attached.

### Added
- **Tests (`apps/gui/src-tauri/tests/gui_state.rs`)**: Added end-to-end `GuiState` NAT assessment test verifying task execution against loopback S2 UDP responders, completion state, history listing, report loading, and report deletion.

## [1.2.0] - 2026-11-25

### Added
- **GUI (`apps/gui`)**: Tauri v2 + Vite + Vue 3 + TypeScript cross-platform application (`neteval-gui`) wrapping `client-core::Orchestrator` via 7 Tauri commands and `assessment://state` live progress events:
  - P2P Dual-Peer Assessment view with real-time SVG latency/loss chart (up to 1200 points, hover crosshair), room-code pairing countdown, peer status bar, and `PeerSidebar` preset manager.
  - Self-host NAT Assessment view, local History list/detail/delete view, `AdvancedSettingsPanel`, and `ResultModal` (desktop centered modal + mobile bottom sheet).
- **protocol / Orchestrator**: Backward-compatible `LivePeerInfo` struct and optional `live_peer` field on `AssessmentState::Running`, populated during `waiting_for_peer`, `punching`, `sampling`, and P2P `saving_report`.
- **CI/CD**: `.github/workflows/gui-ci.yml` and `.github/workflows/release.yml` extended with Windows GUI (`neteval-gui-windows-x86_64.exe`) and Android arm64 APK (`neteval-gui-android-arm64.apk`) builds.
- **Design & Spec**: Added `ui-design.md` and `UI设计草图.drawio`.

### Changed
- Added `x86_64-pc-windows-gnu` toolchain isolation/shims (`.cargo/config.toml`, `scripts/dlltool_shim.rs`, `scripts/windres_shim.rs`, `apps/gui/src-tauri/build.rs`) and `scripts/run-tests.ps1` updates for clean Windows GNU + Tauri v2 builds and integration tests (`apps/gui/src-tauri/tests/gui_state.rs`).
- Synchronized workspace, README, Tauri config, and npm package versions to `1.2.0`.

### Fixed
- Resolved Windows GNU `cdylib` PE export ordinal overflow (`-Wl,--exclude-all-symbols`), `COMCTL32.dll` Common Controls v6 test manifest linking (`cargo:rustc-link-arg-tests`), and Android CI `sdkmanager` NDK setup without deprecated `tools` package.

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
