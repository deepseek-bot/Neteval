# NetEval

[中文](#中文) | [English](#english)

---

## 中文

NetEval 是一个轻量级的跨平台 NAT 画像与 P2P 联机可行性诊断工具。它面向个人开发者与小型联机圈子，用于在正式搭建或使用去中心化组网之前，快速测出本机 NAT 类型、IPv6 可达性，以及两台真实设备之间 UDP 打洞后的实际延迟与丢包稳定性。

### 核心特色

- **两种 NAT 体检模式**：
  - **零服务端公共 STUN 模式**：无需自备 VPS，利用 RFC 5389 公共 STUN 节点开箱区分锥形 NAT（NAT1–3）与对称型 NAT（NAT4），并检测公网 IPv6 可达性。
  - **自建 S2 增强模式**：仅需一台单 IP 双 UDP 端口 VPS，结合同 IP 换端口回包（`reply_from_alt_port`）即可区分 NAT1/2、端口受限 NAT3 与对称型 NAT4；可选配置辅助节点（`--helper`）进一步区分全锥形 NAT1 与地址受限 NAT2。
- **双端真实 P2P 打洞与延迟曲线**：
  - 两端通过轻量级 WebSocket 信令房间配对交换候选地址，使用独立隔离的 IPv4 / IPv6 UDP Socket 进行同步打洞（IPv6 优先，回退 IPv4，等待配对期间每 15 秒发送 UDP 保活防止 NAT 映射老化）。
  - 打洞成功后在固定采样窗口内互发小包，实时绘制 RTT、P95、抖动与丢包曲线，并给出联机结论（`excellent` / `good` / `unstable` / `failed`）。
- **无状态极简服务端**：单文件二进制运行，零数据库、零磁盘落盘、仅内存维护临时配对房间，适配 1C1G 低配 Linux VPS；客户端支持在本地保存历史检测报告与节点预设。
- **多端覆盖**：提供 Windows 桌面 GUI、Android APK 以及 Windows / Linux 命令行客户端。

### 明确边界

- 不提供 TUN/TAP 虚拟网卡、流量转发、中继代理或 VPN/组网内核。
- 不做 P2P 业务数据传输或带宽测速。
- 不内置官方公共信令或中继服务器（公共 STUN 仅作地址反射探测，P2P 配对与 S2 增强探测需使用自建服务端）。

### 下载安装

前往 [GitHub Releases](https://github.com/deepseek-bot/Neteval/releases) 下载对应平台产物：

| 文件名 | 平台 | 说明 |
| --- | --- | --- |
| `neteval-gui-windows-x86_64.exe` | Windows x86_64 | 桌面端图形界面（需系统已安装 WebView2 Runtime） |
| `neteval-gui-android-arm64.apk` | Android arm64 | 移动端图形界面 APK |
| `neteval-cli-windows-x86_64.exe` | Windows x86_64 | 命令行客户端 |
| `neteval-cli-linux-x86_64` | Linux x86_64 (musl) | 静态链接命令行客户端 |
| `neteval-server-linux-x86_64` | Linux x86_64 (musl) | 静态链接信令与 S2 回显服务端 |
| `neteval-server-windows-x86_64.exe` | Windows x86_64 | 服务端 Windows 构建（适合本地调试） |

### 使用方法

#### 1. 图形界面（Windows GUI / Android APK）

- **NAT 体检**：切换至 **NAT 体检** 页签，选择“公共 STUN 初步判定”或填写自建服务器域名/IP 后使用“自建服务器精细判定”，点击 **开始体检**。
- **P2P 打洞**：切换至 **P2P 打洞** 页签，选择协议并填写自建服务器域名/IP 与端口（默认 `ws://` 与 `34780`，也支持直接粘贴完整 `ws://host:port`）及相同的 **房间码**（4–16 位字母数字），点击 **开始测试** 即可查看实时打洞阶段与延迟折线图，结束后自动弹出报告详情。
- **节点预设与策略**：可在 **高级设置** 中保存/套用节点预设、自定义探测端口（默认 `34778`/`34779`）以及切换打洞策略（`IPv6 优先` / `仅 IPv4` / `仅 IPv6`）；所有本地历史记录可在 **历史记录** 页签查看或删除。

#### 2. 自建服务端部署（Linux VPS）

在服务器上授予可执行权限并用 `start` 子命令一键启动（默认双栈监听 **UDP `34778`、`34779`** 作为探测/回显端口与 **TCP `34780`** 作为 `ws` 信令端口，请在防火墙 / 安全组放行对应端口）：

```bash
chmod +x ./neteval-server-linux-x86_64
./neteval-server-linux-x86_64 start ws:34780 --token your-secret-token
```

常用可选参数（完整列表可运行 `neteval-server --help` 或 `neteval-server start --help` 查看）：
- `--probe <P1[,P2]>`：指定 UDP 探测端口，默认 `34778,34779`（单端口如 `34778` 会自动推断 `p2=34779`）；或用 `--no-probe` 仅运行信令。
- `--no-ipv6`：禁用 IPv6 监听（默认在双栈主机上同时监听 IPv4 与 IPv6，纯 IPv4 主机自动降级继续运行）。
- `--helper <HELPER_IP>:34778`：绑定外部辅助 S2 节点，用于启用 `change_ip_hint` 细分 NAT1 与 NAT2。
- `--ws-start-delay-ms` / `--ws-punch-duration-ms`：调整配对成功后的倒计时起步延迟与打洞窗口时长。
- 也兼容旧版显式参数写法（`--p1`、`--p2`、`--v6`、`--ws`、`--ws-v6`）。

#### 3. 命令行客户端（CLI）

**本机 NAT 体检**：

```bash
# 纯公共 STUN 零服务端模式
neteval-cli nat

# 自建 S2 双端口增强模式
neteval-cli nat \
  --server-p1 <VPS_HOST>:34778 \
  --server-p2 <VPS_HOST>:34779 \
  --server-v6 <VPS_HOST>:34778 \
  --token your-secret-token
```

**双端 P2P 打洞与延迟评估**（双方在各自设备上使用同一房间码执行，可通过 `--strategy <v6-first|ipv4-only|ipv6-only>` 指定打洞策略）：

```bash
# 对端 A
neteval-cli p2p \
  --ws ws://<VPS_HOST>:34780 \
  --room testroom01 \
  --label alice \
  --token your-secret-token \
  --echo-v4 <VPS_HOST>:34778 \
  --echo-v6 <VPS_HOST>:34778

# 对端 B
neteval-cli p2p \
  --ws ws://<VPS_HOST>:34780 \
  --room testroom01 \
  --label bob \
  --token your-secret-token \
  --echo-v4 <VPS_HOST>:34778 \
  --echo-v6 <VPS_HOST>:34778
```

**历史记录与 JSON 输出**：

```bash
neteval-cli --json nat
neteval-cli history list
neteval-cli history show <REPORT_ID>
neteval-cli history delete <REPORT_ID>
```

### 从源码构建

前置依赖：Rust 工具链（`stable`）、Node.js（构建 GUI 前端时使用）。

```bash
# 克隆仓库
git clone https://github.com/deepseek-bot/Neteval.git
cd Neteval

# 编译服务端与 CLI
cargo build --release --bin neteval-server --bin neteval-cli

# 编译桌面 GUI（需先构建前端产物并开启 custom-protocol）
cd apps/gui
npm ci
npm run build
cd ../..
cargo build --release -p neteval-gui --features custom-protocol

# 运行工作区全量测试
cargo test --workspace
```

---

## English

NetEval is a lightweight, cross-platform diagnostic tool for NAT profiling and peer-to-peer (P2P) connectivity testing. Built for individual developers and small groups, it measures local NAT behavior, IPv6 reachability, and real end-to-end UDP hole-punching latency and packet loss between two peers before you deploy a mesh or P2P application.

### Features

- **Flexible NAT Profiling**:
  - **Zero-VPS Public STUN Mode**: Uses RFC 5389 public STUN servers out of the box to distinguish Cone NAT (NAT1–3) from Symmetric NAT (NAT4) and check IPv6 reachability.
  - **Self-Hosted S2 Enhanced Mode**: Uses a single dual-UDP-port VPS (`reply_from_alt_port`) to differentiate NAT1/2, Port-Restricted Cone NAT3, and Symmetric NAT4, with optional `--helper` forwarding to separate Full Cone NAT1 from Address-Restricted NAT2.
- **Dual-Peer P2P Hole Punching & Latency Curves**:
  - Pairs two peers over a stateless WebSocket signaling room, exchanges local/reflexive/global-IPv6 candidates, and runs simultaneous hole punching on isolated IPv4 and IPv6 sockets (IPv6-first with IPv4 fallback and 15s keepalives while waiting).
  - Streams small fixed-window probes after punch success to calculate RTT, P95, jitter, packet loss, live chart series, and an overall verdict (`excellent` / `good` / `unstable` / `failed`).
- **Stateless Minimal Server**: Single static binary with no database and no disk persistence; fits easily on a 1C1G Linux VPS. Clients persist assessment history locally as JSON.
- **Cross-Platform Clients**: Available as a Windows Desktop GUI, Android arm64 APK, and Windows / Linux CLI.

### Non-Goals

- Does **not** create TUN/TAP interfaces, route traffic, relay packets, or act as a VPN/mesh client.
- Does **not** perform P2P file transfers or throughput/bandwidth speed tests.
- Does **not** ship built-in hosted signaling or relay servers (BYO server for S1 signaling and S2 enhanced NAT checks).

### Downloads

Prebuilt binaries are available on [GitHub Releases](https://github.com/deepseek-bot/Neteval/releases):

| Artifact | Platform | Description |
| --- | --- | --- |
| `neteval-gui-windows-x86_64.exe` | Windows x86_64 | Desktop GUI (requires WebView2 Runtime) |
| `neteval-gui-android-arm64.apk` | Android arm64 | Mobile GUI APK |
| `neteval-cli-windows-x86_64.exe` | Windows x86_64 | Command-line client |
| `neteval-cli-linux-x86_64` | Linux x86_64 (musl) | Statically linked CLI client |
| `neteval-server-linux-x86_64` | Linux x86_64 (musl) | Statically linked S1 signaling & S2 echo server |
| `neteval-server-windows-x86_64.exe` | Windows x86_64 | Windows server build |

### Usage

#### 1. Desktop & Mobile GUI

- **NAT Check**: Open the **NAT 体检** tab, choose either public STUN initial check or self-hosted server detailed check (by entering a single server host/domain), and start the check.
- **P2P Test**: Open the **P2P 打洞** tab on both devices, pick the signaling protocol (`ws://`), enter your server host/port (defaults to `34780`, or paste a full `ws://host:port` URL directly) and the same **room code** (4–16 alphanumeric characters), then start the test to monitor live hole-punching stages and the RTT/loss chart.
- **Presets, Strategy & History**: Save reusable server presets, customize probe ports (`34778`/`34779`), and choose the hole-punching strategy (`IPv6-first` / `IPv4-only` / `IPv6-only`) in **Advanced Settings**, and inspect or remove saved reports in the **History** tab.

#### 2. Self-Hosted Server (Linux VPS)

Open **UDP `34778`–`34779`** (NAT/echo probing) and **TCP `34780`** (`ws` signaling) in your firewall/security group, then start `neteval-server` with the simplified `start` subcommand:

```bash
chmod +x ./neteval-server-linux-x86_64
./neteval-server-linux-x86_64 start ws:34780 --token your-secret-token
```

Optional flags (run `neteval-server --help` or `neteval-server start --help` for the complete list):
- `--probe <P1[,P2]>`: Override UDP probe ports (defaults to `34778,34779`; passing a single port implies `p2=p1+1`), or `--no-probe` to disable UDP echo.
- `--no-ipv6`: Disable dual-stack IPv6 listeners (by default IPv4 + IPv6 bind the same port, degrading to IPv4-only with a warning if IPv6 is unavailable).
- `--helper <HELPER_IP>:34778`: Point to an auxiliary S2 node for `change_ip_hint` NAT1 vs NAT2 detection.
- `--ws-start-delay-ms` / `--ws-punch-duration-ms`: Tune coordinated punch start delay and punch timeout window.
- Legacy flags (`--p1`, `--p2`, `--v6`, `--ws`, `--ws-v6`) remain supported when invoked without `start`.

#### 3. CLI Usage

**Local NAT Profiling**:

```bash
# Zero-VPS public STUN mode
neteval-cli nat

# Self-hosted S2 dual-port mode
neteval-cli nat \
  --server-p1 <VPS_HOST>:34778 \
  --server-p2 <VPS_HOST>:34779 \
  --server-v6 <VPS_HOST>:34778 \
  --token your-secret-token
```

**Dual-Peer P2P Probe** (run on both peers with the same room code; use `--strategy <v6-first|ipv4-only|ipv6-only>` to control IP family preference):

```bash
# Peer A
neteval-cli p2p \
  --ws ws://<VPS_HOST>:34780 \
  --room testroom01 \
  --label alice \
  --token your-secret-token \
  --echo-v4 <VPS_HOST>:34778 \
  --echo-v6 <VPS_HOST>:34778

# Peer B
neteval-cli p2p \
  --ws ws://<VPS_HOST>:34780 \
  --room testroom01 \
  --label bob \
  --token your-secret-token \
  --echo-v4 <VPS_HOST>:34778 \
  --echo-v6 <VPS_HOST>:34778
```

**History & JSON Output**:

```bash
neteval-cli --json nat
neteval-cli history list
neteval-cli history show <REPORT_ID>
neteval-cli history delete <REPORT_ID>
```

### Build from Source

Prerequisites: Rust (`stable`) toolchain and Node.js (for GUI assets).

```bash
git clone https://github.com/deepseek-bot/Neteval.git
cd Neteval

# Build server & CLI
cargo build --release --bin neteval-server --bin neteval-cli

# Build desktop GUI (build frontend dist first, then enable custom-protocol)
cd apps/gui
npm ci
npm run build
cd ../..
cargo build --release -p neteval-gui --features custom-protocol

# Run workspace tests
cargo test --workspace
```
