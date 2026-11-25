# NetEval UI 设计规范（`v1.2.0`）

> 对应架构阶段：`v1.2.0`（跨平台 GUI — `apps/gui`）  
> 核心原则：极简操作主视图、实时曲线优先、测试完成大浮层详览、桌面/移动共享同一套领域状态模型。

---

## 1. 技术选型与架构边界

### 1.1 技术栈定型
- **应用壳层**：**Tauri v2**（`apps/gui/src-tauri`），作为 Workspace 成员统一由 Cargo 管理。
- **前端栈**：**Vite + Vue 3 + TypeScript**。不引入重型 UI 组件库，延迟/丢包曲线使用轻量原生 **SVG/Canvas** 自绘（单轮样本点上限 `60s × 20Hz = 1200` 点），保证资源开销可控与高交互性。
- **字体排印**：界面中文优先系统无衬线字体；所有 IP、端口、房间号、RTT、P95、抖动数值及百分比**统一使用等宽字体**（`tabular-nums`）避免跳动。

### 1.2 资源与工具链预期
- **体积与内存预算**：Windows 安装包/便携 EXE 约 5～10 MB，Android APK 约 10～18 MB；运行时总内存（含 WebView2 / 系统 WebView）约 70～130 MB，静止时 CPU 接近 0%。
- **工具链验证点**：启动 `v1.2.0` 实现前，需先在本机 `x86_64-pc-windows-gnu` 工具链验证 `webview2-com-sys` 能否通过 `cargo check`；Android APK 由 GitHub Actions（`tauri android build`）云端构建。

### 1.3 架构与数据边界铁律
- `apps/gui/src-tauri` **只能**依赖 `client-core` 与 `protocol`，**严格经由 `Orchestrator` 执行业务操作**，禁止绕过 `C1` 直接调用 `C2`–`C6` 或操作任何 Socket。
- `Orchestrator` 独占全部任务状态（加载、空闲、进度、取消、完成、错误），前端通过订阅状态流纯被动渲染。
- 文案极简：所有可见文案必须绑定真实数据、控件标签或校验提示，不添加装饰性副标题、占位宣传语或无意义徽章。

---

## 2. 跨平台布局规范

### 2.1 全局导航
全应用统一承载三个主视图：
1. **NAT 体检**（对应 `start_nat_assessment`）
2. **P2P 打洞**（对应 `start_p2p_assessment`）
3. **历史记录**（对应 `list_history` / `get_report` / `delete_report`）

| 平台 | 导航形态 | 默认窗口 / 视口策略 |
|---|---|---|
| **Windows** | 顶部标签栏（或极窄左侧 48px 图标栏） | 启动默认 **16:9 固定比例大窗**（如 `1120×630`），允许自由缩放；禁止通过点击按钮动态改变 OS 窗口外框宽高或坐标 |
| **Android** | 底部固定三栏导航（`NAT` / `P2P` / `历史`） | 竖屏自适应安全区（避开状态栏与手势条），软键盘弹出时输入区保持可见 |

---

### 2.2 P2P 打洞视图布局

#### A. Windows：窗口内三栏可折叠布局（类 IDE 工作区）

```text
+------------------------------------------------------------------------------+
| [NAT 体检] [P2P 打洞*] [历史记录]                                   [⚙ 高级设置] |
+-----------------+----------------------------------------+-------------------+
| 左侧栏 (默认收起) |              中央主工作区              | 右侧栏 (默认收起)   |
| (240px, 可折叠)  |                                        | (320px, ⚙ 开关)   |
|                 |  [常用节点 ▼] [room_code 随机/复制]     |                   |
| 当前对端信息     |  [ 口令(可选) ] [ 昵称 ]  [ 开始 / 停止 ]|  Advanced Settings|
| - peer_label    |----------------------------------------|  - 信令地址与预设   |
| - ip_family     |  阶段进度条 (5阶段) + 当前 RTT 摘要       |  - S2 回显 v4/v6   |
| - active_addr   |                                        |  - 公共 STUN 列表  |
| - punch_elapsed |                                        |  - 超时与采样参数   |
|-----------------|           实时延迟 / 丢包折线图          |  - 打洞策略(只读)  |
| 近期 P2P 历史    |         (占据主工作区 ~75% 面积)        |                   |
| (预留在线列表位) |                                        |                   |
+-----------------+----------------------------------------+-------------------+
```

- **中央主工作区**：默认铺满整个窗口宽度。顶部是紧凑单行控制条（已保存节点下拉、房间号输入 + 随机生成/一键复制、单按钮双向切换 `开始测试` / `停止`），下方 `~75%` 面积留给 5 阶段进度条与实时折线图。
- **右侧栏（Advanced Settings）**：默认收起；点击右上角 `⚙` 在窗口内部平滑展开，再点收起，主工作区自适应收缩。
- **左侧栏（对端与会话信息）**：默认收起；开始 P2P 测试进入配对/打洞时自动展开（支持手动开关）。上半区展示当前对端详情，下半区放最近几条 P2P 历史快捷回放（同时为后续“小圈子在线成员列表”预留结构空间）。

#### B. Android：单侧边抽屉 + 顶部动态折叠对端区

- **唯一侧边栏**：仅保留**右侧抽屉**用于承载 `Advanced Settings`；顶栏右上角固定提供 `⚙` 按钮触发打开（手势辅助，避免与 Android 全面屏边缘右滑/左滑返回系统手势冲突）。
- **主屏自上而下排布**：
  1. **顶部对端实时信息区**：`Idle` 状态与准备阶段保持**折叠/极薄状态**，不占用宝贵首屏空间；一旦进入 `waiting_for_peer` / `punching` 并获得对端或等待状态信息，动态展开为 `2～3` 行紧凑卡片，向下方结果区微让空间。
  2. **输入与控制区**：节点选择、房间号（带随机/复制）、可选口令、主按钮 `开始` / `停止`。
  3. **实时结果区**：阶段指示 + 实时 RTT 数值 + 轻量实时延迟曲线（不在此处硬塞完整参数表）。

---

### 2.3 完整结果展示：应用内居中大浮层（In-App Result Modal）

- **触发时机**：当 P2P（或 NAT）测试达到 `AssessmentState::Completed(AssessmentReport)` 后自动弹出；在「历史记录」点击某条记录、或测试完成后在主区点击结果摘要栏也可再次打开。
- **浮层形态**（**明确采用应用内浮层，不使用 Android 系统级悬浮窗 `SYSTEM_ALERT_WINDOW`**）：
  - **Windows**：窗口内居中遮罩大浮层（占主窗约 `85%` 面积，背景轻量压暗），支持 `Esc` 或右上角关闭。
  - **Android**：应用内高占比浮层 / `90%` 高度底部面板（Sheet），支持下拉或点击关闭。
- **浮层内容结构**：
  1. **主结论头**：`verdict` 结论状态块 + `peer_label` + `failure_reason` / `interrupted_reason`（如有）。
  2. **核心指标网格**：`avg_rtt_ms`、`p95_rtt_ms`、`jitter_ms`、`loss_ratio`（百分比）+ `sent / received`。
  3. **通道详情行**：`ip_family`（`IPv6` / `IPv4`）、`active_peer_addr`、`punch_elapsed_ms`。
  4. **宽幅交互式延迟曲线**：基于 `series`（`Vec<LatencyPoint>`）绘制，支持鼠标悬停 / 手指点按查看单包 `(seq, elapsed_ms, rtt_ms)`；丢包点（`rtt_ms == null`）在曲线上保留**断开间隙 + 底部红色丢包标记**。

---

## 3. 字段级界面映射契约

以下字段名、类型、范围与 `crates/protocol/src/lib.rs` 严格一致。

### 3.1 P2P 输入表单（`P2pTaskInput`）

| 字段名 | 类型 | 归属区域 | 校验 / 默认值 | UI 控件 |
|---|---|---|---|---|
| `signaling_ws_url` | `String` | 主视图（节点下拉）+ 高级设置 | 必填，非空；当前仅支持 `ws://`，填 `wss://` 时即时报错提示 | 下拉预设选择器 + 地址输入框 |
| `room_code` | `String` | 主视图 | 必填，`4..=16` 位 ASCII 字母或数字 | 等宽文本框 + 随机生成 + 复制邀请 |
| `peer_label` | `String` | 主视图 / 节点预设 | 必填，去空白后非空 | 文本输入框（可记住上次本机昵称） |
| `token` | `Option<String>` | 主视图（可选）或节点预设内 | 可选，`None` | 密码框（带显示/隐藏切换） |
| `echo_udp_v4` | `Option<String>` | 右侧高级设置 | 可选，`host:port` | 文本框（支持从 NAT 页复用同 VPS 地址） |
| `echo_udp_v6` | `Option<String>` | 右侧高级设置 | 可选，`host:port` | 文本框 |
| `public_stun_servers` | `Vec<String>` | 右侧高级设置 | 默认 `["stun.qq.com:3478", "stun.miwifi.com:3478", "stun.cloudflare.com:3478"]` | 可增删列表或逗号分隔编辑 |
| `candidate_timeout_ms` | `u64` | 右侧高级设置 | 默认 `2500` ms | 数字输入框 |
| `pair_timeout_secs` | `u64` | 右侧高级设置 | 默认 `120` s（建议 UI 限制 `1..=120`） | 数字输入框 |
| `punch_timeout_ms` | `u32` | 右侧高级设置 | 默认 `6000` ms，范围 `3000..=15000` | 滑块 / 数字输入框 |
| `window_secs` | `u32` | 右侧高级设置 | 默认 `20` s，范围 `5..=60` | 滑块 / 数字输入框 |
| `packet_rate_hz` | `u32` | 右侧高级设置 | 默认 `10` Hz，范围 `1..=20` | 滑块 / 数字输入框 |
| `strategy` | `PunchStrategy` | 右侧高级设置（只读） | 默认且当前唯一值 `standard_udp_v6_first` | 只读标签（`IPv6 优先回退 IPv4`） |

### 3.2 运行态与阶段映射（`AssessmentState`）

| 状态变体 | 字段 | UI 行为 |
|---|---|---|
| `idle` | — | 允许启动测试；主按钮显示「开始测试」 |
| `running` | `task_id`, `stage`, `progress_pct` (`0..=100`), `live_point` (`Option<LatencyPoint>`) | 主按钮切为「停止」；按 `progress_pct` 驱动进度条；收到非空 `live_point` 时追加至实时曲线 |
| `completed` | `AssessmentReport` | 解除运行锁；自动弹出居中结果大浮层；若 `storage_warning` 非空则显示非致命黄色提示条 |
| `cancelled` | `reason: Option<String>` | 解除运行锁；恢复空闲并在状态条提示取消原因 |
| `failed` | `ProbeError` | 解除运行锁；展示对应错误文案，不写入历史记录 |

**P2P `stage` 枚举值与展示文案：**
1. `preparing_candidates` — 正在采集本机网络与公网候选地址
2. `waiting_for_peer` — 已进入房间，等待对端加入（主区显示房间号倒计时）
3. `punching` — 双端同步打洞中
4. `sampling` — 直连通道已建立，正在固定窗口采样（唯一产生 `live_point` 的阶段）
5. `saving_report` — 正在保存评估报告

**`P2pVerdict` 配色与语义映射：**
- `excellent`（绿）：直连极佳
- `good`（蓝/青）：直连可用
- `unstable`（琥珀黄）：存在明显抖动或丢包
- `failed`（红）：打洞超时或连接失败（注：已配对后的 `punch_timeout` 同样进入 `Completed`，`punched == false`、`verdict == failed`）

**高频 `ProbeError` 提示映射：**
- `unauthorized` → 访问口令错误或未填写
- `room_full` → 该房间人数已满或服务端达到 64 房间上限
- `peer_timeout` → 等待对端加入超时
- `peer_left` → 对端已主动退出测试
- `server_unreachable` → 信令/探测服务器不可达或连接断开（附带细节）
- `invalid_input` → 参数格式/范围有误，或已有任务正在运行（`task_already_running`）

### 3.3 NAT 体检与历史记录字段

- **NAT 输入（`NatTaskInput`）**：
  - 主区可选切换「零 VPS 公共 STUN 模式」或选择自建 VPS 节点预设（填充 `primary_v4_p1`、`primary_v4_p2`、`primary_v6`、`helper_v4_p1`、`token`）；`public_stun_servers` 与 `timeout_ms`（默认 `2500` ms）收进高级设置。
- **NAT 结果（`NatReport`）**：
  - 核心展示：`status`（`complete_full` / `complete_single_vps` / `public_stun_only` / `unreachable`）、`mapping`、`filtering`、`estimated_difficulty`、`ipv6_reachable`、`public_ipv4`、`public_ipv6`、`notes[]`。
- **历史列表（`ReportSummary`）**：
  - 每行展示 `report_id`、格式化时间（`created_unix_ms`）、类型与结论标签（`has_nat` + `nat_status`；`has_p2p` + `p2p_verdict`），提供「查看详情（打开结果浮层）」与「删除」操作。

---

## 4. Tauri v2 桥接接口契约

### 4.1 Tauri Commands（前端 → Rust 壳 → `Orchestrator`）
- `start_nat_assessment(input: NatTaskInput) -> Result<String, ProbeError>`
- `start_p2p_assessment(input: P2pTaskInput) -> Result<String, ProbeError>`
- `cancel_current(reason: Option<String>) -> Result<(), ProbeError>`
- `list_history() -> Result<Vec<ReportSummary>, ProbeError>`
- `get_report(id: String) -> Result<AssessmentReport, ProbeError>`
- `delete_report(id: String) -> Result<(), ProbeError>`
- UI 本地偏好持久化（节点预设、最近房间参数、默认昵称）由前端 `localStorage` 或 Tauri `app_data_dir` 小型 JSON 独立管理，不混入 `C6` 的评估报告目录。

### 4.2 Tauri Events & 移动端生命周期
- **状态事件推送**：Rust 壳在初始化 `Orchestrator` 后启动后台转发任务，监听 `orchestrator.subscribe()`（`watch::Receiver<AssessmentState>`）并持续向前端 emit 事件 `assessment://state`（payload 为 `AssessmentState` JSON）。
- **Android 前后台 / 切网中断保护**：Android 原生生命周期进入后台或网络发生切换时，Rust 壳层调用 `orchestrator.cancel_current(Some("android_background" | "network_changed"))`，保证立即释放本地 UDP Socket 并向信令房间发送 `LeaveRoom`。
