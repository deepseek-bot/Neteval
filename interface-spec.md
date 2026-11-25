# NetEval 接口文档（v1.0.0 基准）

> **文档用途**：定义 NetEval 各模块（`protocol`、`server`、`client-core`）的对外 API、线缆协议、数据契约、状态机与调用时序约束。  
> **关联文档**：整体架构、模块职责划分与演进路线见 [架构设计文档](./architecture.md)。

---

## 一、编码与通用约定

| 约定项 | 规范内容 |
| :--- | :--- |
| **序列化格式** | 网络协议统一采用 `UTF-8 JSON`，字段名与枚举标签统一 `snake_case` |
| **UDP 报文上限** | 所有 UDP 协议报文严格 `<= 256 bytes`（`S2` 换 IP 回包严格 `<= 128 bytes`） |
| **向后兼容** | 解码时忽略未知可选字段，确保小版本（`v1.x.0`）平行扩展字段时旧端不崩溃 |
| **依赖方向** | `U1/U2 (cli/gui) -> C1 -> C2～C6 -> protocol`；`S1/S2 -> protocol`，禁止横向同级互调 |
| **取消与超时** | 所有网络操作必须接收显式超时与取消信号；用户取消须在 `<= 500ms` 内回收资源 |

---

## 二、公共错误与核心数据模型（`crates/protocol`）

### 2.1 `ProbeError`（通用错误枚举）

| 变体 | 参数 | 触发场景说明 |
| :--- | :--- | :--- |
| `invalid_input` | `str` | 入口参数校验失败（如 `room_code` 非法、频率/窗口超限） |
| `unauthorized` | — | `access_token` 缺失或校验失败 |
| `server_unreachable` | `str` | 服务端连接失败或超时不可达 |
| `room_not_found` | — | 房间不存在或已过期 |
| `room_full` | — | 房间人数已满（上限 2 人） |
| `peer_timeout` | — | 首位用户等待对端加入超时（`> 120s`） |
| `peer_left` | — | 对端主动离开或信令断开 |
| `punch_failed` | `str` | 双方进房后打洞超时或无法建立双向通道 |
| `cancelled` | — | 本端主动取消或切后台/切网中断（`v1.2+`） |
| `internal` | `str` | 内部未预期异常 |

### 2.2 核心数据类型

- **`Candidate`**（候选地址）
  - `kind`: `host_local | host_global_v6 | server_reflexive_v4 | server_reflexive_v6`（支持枚举本机原生公网 IPv6，即使信令/回显 VPS 为纯 IPv4 也能上报 IPv6 直连地址）
  - `addr`: `ip:port`
- **`LatencyPoint`**（实时延迟采样点）
  - `seq`: `u32`
  - `elapsed_ms`: `u32`
  - `rtt_ms`: `Option<f32>`（`null` 代表丢包）
- **`P2pVerdict`**（P2P 连通质量评级）
  - `excellent`: 丢包 `<= 1%`、P95 `<= 60ms`、抖动 `<= 10ms`
  - `good`: 丢包 `<= 3%`、P95 `<= 100ms`、抖动 `<= 25ms`
  - `unstable`: 已连通但超出 `good` 阈值
  - `failed`: 打洞失败
- **`AssessmentReport`**（总评估报告）
  - `report_id`: `str`
  - `created_unix_ms`: `u64`
  - `nat_report`: `Option<NatReport>` *(`v1.0`)*
  - `storage_warning`: `Option<str>`
  - *后续版本平行追加*：`p2p_report?`（`v1.1`）、`link_reports?`（`v1.3`）
- **`AssessmentState`**（客户端全局状态快照）
  - `idle`
  - `running { task_id: str, stage: str, progress_pct: u8, live_point: Option<LatencyPoint> }`
  - `completed(AssessmentReport)`
  - `cancelled { reason: Option<str> }`
  - `failed(ProbeError)`

---

## 三、服务端接口（`crates/server`）

### 3.1 `S1` 信令协调（`v1.1`，固定 1 个 WebSocket 端口）

- **调用方**：`C2`
- **输入消息（Client → Server）**：
  - `join_room`：`room_code`（4～16 位字母数字）、`token?`、`peer_label`、`candidates[]`
  - `leave_room`
- **输出消息（Server → Client）**：
  - `waiting_for_peer`：`room_code`、`ttl_secs`
  - `start_punch`：`session_id`、`peer_label`、`peer_candidates[]`、`start_delay_ms`（相对倒计时）、`punch_duration_ms`
  - `peer_left`
  - `error(ProbeError)`
- **契约约束**：
  - 纯内存房间（`MAX_ROOMS = 64`，单房 `<= 2` 人），零落盘。
  - 等待对端加入 TTL 为 `120s`；下发 `start_punch` 后房间保活 `punch_duration_ms + 2s`，用于打洞期转发 `peer_left`。

### 3.2 `S2` 地址回显（`v1.0` 固定双 UDP 端口；可选辅节点）

- **调用方**：`C4`、`C3`、主 `S2`
- **输入消息**：
  - `addr_echo_req`：`nonce`、`token?`（`mode: normal | reply_from_alt_port | reply_from_helper_ip`，自 `v1.0` 起主节点支持 `reply_from_alt_port` 同 IP 换端口回包）
  - `change_ip_hint`（可选双机场景，主 `S2 -> 辅 S2` 内部指令）：`token`、`nonce`、`target_client`
- **输出消息**：
  - `addr_echo_resp`：`nonce`、`observed_addr`（`ip:port`）、`responder_id?`
- **契约约束**：
  - 完全无状态。
  - `token` 错误或包体 `> 256B` 时静默丢弃。
  - 换端口/换 IP 回包严格只向已验证来源发送 1 个极小 UDP 包（`<= 128B`），零反射放大。

---

## 四、客户端核心模块接口（`crates/client-core`）

### 4.1 `C1` 编排中枢（`v1.0`，仅 `U1/U2` 调用）

- **对外方法**：
  - `start_nat_assessment(NatTaskInput)` *(v1.0)*
  - `start_p2p_assessment(P2pTaskInput)` *(v1.1)*
  - `start_link_assessment(LinkTaskInput)` *(v1.3)*
  - `cancel_current(reason?)`
  - `list_history()` / `get_report(id)` / `delete_report(id)`
- **输出**：持续推送 `AssessmentState` 状态快照流。
- **约束**：**单任务互斥**（`Running` 状态下重复调用 `start_*` 直接拒绝并返回 `task_already_running`）；取消操作须在 `<= 500ms` 内回收全部下属资源。

### 4.2 `C2` 信令客户端（`v1.1`，仅 `C1` 调用）

- **输入**：`pair_in_room(ws_url, room_code, token?, peer_label, local_candidates[], timeout_secs, cancel)`
- **输出**：成功返回 `(PairingResult, SignalGuard)`；`SignalGuard` 提供：
  - 监听 `peer_left` 事件
  - `send_leave_and_close()`
  - `close()`
- **约束**：打洞成功、超时或取消后立即关闭 WebSocket，不维持长连接。

### 4.3 `C3` P2P 通道体检（`v1.1` 基础；`v1.4+` 扩展，仅 `C1` 调用）

采用**多步黑盒句柄生命周期**，强制同协议族复用各自独立的本地 UDP Socket：

1. **`prepare_session(echo_udp_v4?, echo_udp_v6?, public_stun_servers[], token?, timeout_ms, cancel)`**
   - `SessionHandle` 内部封装**独立的 IPv4 UDP Socket (`0.0.0.0:0`) 与 IPv6 UDP Socket (`[::]:0`)**（双栈隔离，避免 Windows 默认 `IPV6_V6ONLY=true` 及路由器对 `::ffff:x.x.x.x` 映射地址的兼容缺陷），同时枚举本机公网 IPv6 (`host_global_v6`) 并向 `S2`/公共 STUN 获取反射地址。
   - 返回：`(SessionHandle, local_candidates[])`（`SessionHandle` 为不透明句柄）。
2. **`keepalive_session(session_handle, echo_udp_v4?, echo_udp_v6?, public_stun_servers[])`**
   - 在 `waiting_for_peer` 等候期（最长 `120s`）每 `15s` 发送一次 UDP 心跳保活包，防止运营商 CGNAT / 家用路由器在 `30s` 后回收端口映射。
3. **`execute_probe(session_handle, session_id, peer_candidates[], start_delay_ms, punch_timeout_ms, window_secs, packet_rate_hz, strategy, on_event, cancel)`**
   - 参数默认与范围：
     - `punch_timeout_ms`：`3～15s`，默认 `6s`
     - `window_secs`：`5～60s`，默认 `20s`
     - `packet_rate_hz`：`1～20Hz`，默认 `10Hz`
     - `strategy`：`v1.1` 为 `standard_udp_v6_first`；`v1.4+` 扩展 `quic` / `port_predict` / `tcp`
   - 过程事件（`on_event`）：推送 `punch_succeeded` 与 `latency_point`
   - 结束返回：`P2pReport`（含 `punched`、`active_peer_addr?`、`ip_family?`、`punch_elapsed_ms?`、`sent`、`received`、`loss_ratio`、`avg_rtt_ms?`、`p95_rtt_ms?`、`jitter_ms?`、`series[]`、`verdict`、`failure_reason?`、`interrupted_reason?`）
4. **`abort_session(session_handle)`**
   - 未进入或提前退出 `execute_probe` 时，强制消费句柄并关闭内部 v4/v6 双 Socket。

- **线缆报文（`P2pDirectMsg`，`<= 256B`）**：`session_id`、`kind: punch | punch_ack | ping | pong`、`seq`、`sent_offset_ms`，按 `(session_id, seq)` 幂等处理。
- **约束**：句柄一次性消费必关全部 Socket；默认 OS IPv6 优先。

### 4.4 `C4` NAT 画像（`v1.0` 核心模块，仅 `C1` 调用）

- **输入**：`run_nat_probe(public_stun_servers[], primary_v4_p1?, primary_v4_p2?, primary_v6?, helper_v4_p1?, token?, timeout_ms, cancel)`
  - `public_stun_servers[]` 默认内置多个高可用公共 RFC 5389 STUN 节点（如 `stun.qq.com:3478`、`stun.miwifi.com:3478`、`stun.cloudflare.com:3478`），支持用户覆盖。
- **输出**：`NatReport`（`status: complete_full | complete_single_vps | public_stun_only | unreachable`、`public_ipv4?`、`public_ipv6?`、`ipv6_reachable`、`mapping: endpoint_independent | address_or_port_dependent_symmetric | unknown`、`filtering: full_cone_nat1 | address_restricted_nat2 | port_restricted_nat3 | cone_nat1_or_nat2 | not_tested_public_stun | unknown`、`estimated_difficulty`、`notes[]`）
- **分层探测逻辑与降级约束**：
  - `complete_full`：配置了自建主节点双端口 + 辅节点（或双 IP VPS），完整测定 `mapping` 与 `filtering`（精准区分 NAT1 / NAT2 / NAT3 / NAT4）。
  - `complete_single_vps`：配置了 1 台自建 VPS 双端口（`p1` + `p2`）+ 公共 STUN 第二 IP（或仅自建双端口），100% 测定 `mapping`（区分锥形 vs 对称型 NAT4），并通过 `reply_from_alt_port` 精准判定是否为 `port_restricted_nat3`（若换端口回包可达则收敛为 `cone_nat1_or_nat2`）。
  - `public_stun_only`：零 VPS 模式，仅使用 `>= 2` 个不同公网 IP 的公共 STUN 节点对比同一本地 Socket 的反射端口，测定 `mapping`（锥形 NAT1～3 vs 对称型 NAT4）与 `ipv6_reachable`，`filtering` 标记为 `not_tested_public_stun`。
  - `unreachable`：所有探测节点均不可达。
  - 无状态、自建自毁临时 Socket（同一轮测试严格复用同一个本地 UDP Socket），严禁在节点不可达时误判 NAT 类型。

### 4.5 `C5` 链路探测（`v1.3` 预留，仅 `C1` 调用）

- **输入**：`run_link_probe(targets[], attempts_per_target, timeout_ms, max_concurrency, cancel)`
  - `targets[]`：`label`、`host_ip`、`port`、`protocol: tcp | tls_sni(sni)`
  - `attempts_per_target`：`1..=20`
  - `max_concurrency`：`1..=5`
- **输出**：`LinkTargetReport[]`（每目标含 `reachable`、`loss_ratio`、`avg_rtt_ms?`、`min_rtt_ms?`、`max_rtt_ms?`、`samples[]`）
- **约束**：无状态，直连指定 `host_ip` 绕过系统 DNS，禁用 ICMP；单目标失败不中断其他目标。

### 4.6 `C6` 本地存储（`v1.0`/`v1.2`，仅 `C1` 调用）

- **输入**：`save_report` / `list_reports` / `load_report` / `delete_report`
- **输出**：对应报告、摘要列表或存储错误
- **约束**：仅操作本地目录；**写盘失败绝不吞掉已完成的探测报告**。

---

## 五、客户端状态机与核心调用时序

### 5.1 客户端主状态机（由 `C1` 独占）

```text
  [ Idle ] <--- 初始态
     |
     | start_* （Idle / Completed / Cancelled / Failed 均可直接重起；
     |           Running 状态重复发起直接拒绝并返回 task_already_running）
     v
  [ Running ] （按子阶段推进并推送进度/实时延迟点）
     ├── 探测完成（含“NAT 画像结果”或“真实打洞超时未通”的有效结果） ──> [ Completed (存历史、给结论) ]
     ├── 本端取消 / Android 切后台或切网(v1.2)     ──> [ Cancelled (<=500ms 释放资源) ]
     └── 前置条件失败（全部节点不可达/鉴权错/房满/等友超时）──> [ Failed (ProbeError) ]
```

> **判定边界**：`v1.0` 中只要有可用节点输出 NAT 画像（即 `status != unreachable`），或 `v1.1` 中双方进房后真实打洞未通（`punched = false, verdict = failed`），均属于有效体检结果，正常进入 **`Completed`** 并保存；只有体检未能正常启动（如全部节点不可达、鉴权失败、房间满、等待 `120s` 触发 `peer_timeout`）才进入 **`Failed`**。

### 5.2 `v1.0` 核心 NAT 画像调用时序与防泄露退路

1. **`probing_nat`（公共 STUN 映射探测 + 可选自建 `S2` 过滤细分）**
   - `C1` 调 `C4.run_nat_probe`：绑定临时本地 UDP Socket，先并发请求多个不同公网 IP 的公共 RFC 5389 STUN 节点判定 IPv4 `Mapping`（`cone` vs `symmetric_nat4`）及 IPv6 连通性；若配置了自建 `S2`，再使用同一 Socket 请求主节点双端口与 `reply_from_alt_port`（及可选辅节点 `reply_from_helper_ip`）细分 `Filtering`（精准判出 `NAT3` 或完整 `NAT1/NAT2/NAT3`）。
   - *异常退路*：全部节点超时返回 `unreachable` 进入 `Failed(server_unreachable)`；用户取消时 `<= 500ms` 销毁临时 Socket 进入 `Cancelled`。
2. **`saving_report` → `Completed`**
   - `C1` 调 `C6.save_report` 落盘 `NatReport`；写盘失败附加 `storage_warning`，仍正常进入 `Completed`。

### 5.3 `v1.1` 核心 P2P 探测 5 段调用时序与防泄露退路

1. **`preparing_candidates`**
   - `C1` 调 `C3.prepare_session` 绑定独立的 v4/v6 双 Socket，直采本机公网 IPv6 (`host_global_v6`) 并向 `S2`/公共 STUN 查反射地址。
   - *异常退路*：失败或取消时，`C3` 立即关闭全部 Socket，进入 `Failed(server_unreachable)` 或 `Cancelled`。
2. **`waiting_for_peer`**
   - `C1` 调 `C2.pair_in_room` 等待对端进房；期间 `C1` 每 `15s` 驱动 `C3.keepalive_session` 向回显节点发心跳保活，防止 CGNAT / 家用路由器 `30s` 老化回收映射端口。
   - *异常退路*：若房满、鉴权错、`120s` 超时（`peer_timeout`）、对方退房或本端取消 → `C2` 关闭 WS，**`C1` 必须立即调用 `C3.abort_session(句柄)` 释放阶段 1 的 v4/v6 双 Socket**，保证零句柄泄露。
3. **`punching`（双路联动，零横向依赖）**
   - `C1` 同时运行 `C3.execute_probe`（等待相对倒计时后多轮连发打洞，IPv6 优先）与 `C2` 的 `SignalGuard`（监听 `peer_left`）：
   - *打通*：`C3` 发出 `punch_succeeded` → **`C1` 立刻调用 `C2.close()` 断开信令**，进入阶段 4。
   - *对方中途取消*：`C2` 收到 `peer_left` → **`C1` 立刻触发 `C3` 取消信号**，中止打洞并关全部 Socket。
   - *本端主动取消*：`C1` 通知 `C2.send_leave_and_close()` 并同时触发 `C3` 取消信号。
   - *打洞超时未通*：`C3` 关 Socket 返回 `P2pReport(failed)`，直跳阶段 5 进入 `Completed`。
4. **`sampling`**
   - `C3` 跑满固定窗口小包测 RTT / 抖动 / 丢包并推送 `latency_point`。
   - *异常退路*：对端中途掉线 → 剩余包记 `rtt_ms = null`，跑满窗口正常收口不挂死；本端取消或切后台 → `<= 500ms` 关全部 Socket 进入 `Cancelled`。
5. **`saving_report` → `Completed`**
   - `C1` 调 `C6.save_report`。写盘失败时附加 `storage_warning`，仍正常进入 `Completed` 展示报告。
