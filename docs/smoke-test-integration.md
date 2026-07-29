# phone-control × 移动端自动化冒烟测试 — 集成设计

> 目标：提测/打包完成后，CI 自动调起冒烟测试，10 分钟内验证 App 核心功能是否可用。
> 本文定义 phone-control 在这条流水线里承担的职责、需要做的改造，以及分阶段落地计划。

## 0. 一个关键事实纠正

原方案把 phone-control 描述为 **Electron / Native**，并据此建议「用 Node.js/Golang 做 CLI 包装」。
**实际架构是 Tauri 2（Rust 后端 + WebView 前端）**，进程内已内建一个本地 server（[`ws.rs`](../src-tauri/src/ws.rs) 的 `127.0.0.1:32199` 视频帧 WebSocket）。

影响：
- **方式 A（内置 HTTP API）是顺路活** —— 在同一个 Tokio 运行时里再挂一个 axum 服务即可，无需另起进程/语言。✅ 采用。
- **方式 B（Node CLI 包装）是弯路**，不采用。真需要 CLI，做个瘦 Rust bin 调 HTTP API 即可。
- 现有 `:32199` 那个 WS 是纯二进制视频帧流，不是控制通道，不能改造成 REST，故新开 `:9090`。

## 1. 职责边界（本设计的核心决策）

phone-control 只做它握着 ADB 连接的**独家优势**部分，其余交给流水线的其他环节。

| 环节 | 归属 | 理由 |
|---|---|---|
| 设备调度 / 分配 / 健康检查 | **phone-control** | 它已持有 ADB fleet 连接与轮询 |
| 装包（APK 安装 / 权限预置） | **phone-control** | host-side `adb install`，已实现 Android |
| 采集（录屏 / logcat / 截图 / 产物打包） | **phone-control** | 旁路采集，独立于 UI 驱动 |
| **UI 驱动（跑 Maestro / Appium 用例）** | **Maestro / Appium（不在 App 内）** | 见决策 #3 |
| AI 归因 / Linear 建单 / 飞书推送 | **CI 编排层（Claude Code + MCP）** | 凭据管理与可维护性不该进 GUI |

### 决策 #3：phone-control 不碰 UI 驱动，UI 全交给 Maestro

**为什么**：phone-control 会占用每台设备的 scrcpy **video + control socket**（见 [`scrcpy_client.rs`](../src-tauri/src/adb/scrcpy_client.rs)、[`stream.rs`](../src-tauri/src/adb/stream.rs)）。Maestro/Appium 也要独占驱动同一台设备的 UI。**两者同时驱动一台设备必冲突**。

Maestro 本身是个 CLI，直接通过 adb 跟设备通信，根本不需要 phone-control 来跑它。把它塞进 GUI App 只会让 App 变巨石、难测。

**落地约束**：
- `acquire` 一台设备给冒烟任务时，phone-control **主动释放**该设备的 scrcpy 流与控制 socket（`stop_stream_loop(force=true)`），之后不再对它做任何 tap/swipe/text。
- Jenkins 拿到分配的 serial 后，**自己**跑 `maestro test --device <serial>`。
- 采集（`adb screenrecord` / `adb logcat`）走**独立 adb 通道**，与 Maestro 的输入注入互不干扰，可全程并行。

```
[Jenkins]
   │ 1. POST /devices/acquire   → 拿到空闲 serial（phone-control 已释放该设备控制权）
   │ 2. POST /install           → adb install -r 测试包
   │ 3. POST /capture/start     → phone-control 后台起 screenrecord + logcat（旁路）
   │ 4. maestro test --device <serial> ...   ← Jenkins 直接驱动 UI，phone-control 不参与
   │ 5. POST /capture/stop       → 收尾产物
   │ 6. GET  /smoke/report       → 取产物路径（JUnit/JSON + mp4 + log）
   │ 7. POST /devices/release    → 归还设备
   ▼
[失败用例] → CI 把 log/截图喂给 Claude 分析 → 调 Linear MCP 建单 → 飞书推送
```

## 2. 现状 vs 目标：差距表

（✅ 已具备 / 🟡 部分 / ❌ 缺失 —— 均对照当前代码核对）

| 能力 | 现状 | 需要做什么 |
|---|---|---|
| CI 触发（HTTP API） | ❌ Tauri command 仅 WebView IPC 可调 | **本分支已起骨架**：axum `:9090` |
| 设备检测 | 🟡 `poll_all_servers` 轮询 + 状态解析 | 已可用；补离线自动重连 |
| 设备分配 / 租约 | ❌ 无「空闲锁定」概念 | **本分支已起骨架**：内存 lease 表 |
| 安装 APK | ✅ `install_apk_devices`（并行 `-r`，信号量限流） | 已复用为共享 `adb::commands::install_apk` |
| 权限预置 | ❌ | 加 `install -g` / `pm grant` / appops |
| 清装（先卸后装） | ❌ | 加 `adb uninstall` 前置选项 |
| 录屏落盘 | 🟡 有 H.264 实时流，不存文件 | `adb screenrecord`（或 `scrcpy --record`）→ mp4 |
| 截图 | ❌ 旧 JPEG 路径已删 | `adb exec-out screencap` |
| Logcat | ❌ | `adb logcat` per device/task 落盘 |
| 性能指标 | ❌ | dumpsys / perfetto，较重，后置 |
| 产物打包（JUnit/JSON） | ❌ | 标准化 `output-dir` + report bundle |
| iOS（IPA/simctl/tidevice） | ❌ 纯 ADB | **独立大工程**，单独排期 |
| macOS headless + 录屏权限 | ❌ 仅 GUI 启动 | 支持自启开 API；配合 LaunchAgent + TCC 授权 |
| AI 归因 / Linear / 飞书 | ❌ | 放 CI 编排层，不进 App |

## 3. HTTP 控制 API 规格（`127.0.0.1:9090`）

实现见 [`control_api.rs`](../src-tauri/src/control_api.rs)。

| 方法 | 路径 | 说明 | 状态 |
|---|---|---|---|
| GET | `/api/v1/health` | 健康检查 | ✅ 骨架 |
| GET | `/api/v1/devices` | 在线设备 + 租约归属 | ✅ 骨架 |
| POST | `/api/v1/devices/acquire` | 租用单台设备（`any`/`serial` + TTL；并释放其 scrcpy 控制权） | ✅ |
| POST | `/api/v1/devices/release` | 按 `task_id` 归还租约 | ✅ 骨架 |
| POST | `/api/v1/install` | `adb install -r`（复用共享逻辑） | ✅ 骨架 |
| POST | `/api/v1/capture/start` | 起 screenrecord + logcat | ⏳ Phase 2（stub 501） |
| POST | `/api/v1/capture/stop` | 收尾产物 | ⏳ Phase 2（stub 501） |
| GET | `/api/v1/smoke/report` | JUnit/JSON 产物包 | ⏳ Phase 2（stub 501） |

**鉴权**：除 `/health` 外，所有端点要求 `Authorization: Bearer <token>`。token 来自 `PHONE_CONTROL_TOKEN` 环境变量，或落盘在 `~/.phone_control/api_token`（App 首次启动自动生成 UUID）。API 仅绑定 `127.0.0.1`，绝不绑 `0.0.0.0`。

示例：

```bash
TOKEN=$(cat ~/.phone_control/api_token)

# 1) 租一台设备。any（默认）：任意空闲 Android 设备；或精确 serial。
#    一次一台；并行 Job 各自调一次 /acquire（无多设备组绑定）。
curl -s localhost:9090/api/v1/devices/acquire \
  -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' \
  -d '{"task_id":"build-1234"}'                       # any
# -d '{"task_id":"build-1234","serial":"emulator-5554","ttl_secs":900}'  # 精确 + 自定义 TTL
# → {"task_id":"build-1234","device":{"serial":"emulator-5554",...,"expires_at":1735689600}}

# 2) 装包到该任务租用的设备
curl -s localhost:9090/api/v1/install \
  -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' \
  -d '{"task_id":"build-1234","apk_path":"/path/to/app-debug.apk"}'

# 3) Jenkins 自己驱动 UI（phone-control 已让出该设备）
maestro test --device emulator-5554 ./smoke-tests/

# 4) 归还（即使漏调，租约到期后也会被后台 sweeper 自动回收）
curl -s localhost:9090/api/v1/devices/release \
  -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' \
  -d '{"task_id":"build-1234"}'
```

## 4. 部署拓扑（Jenkins Agent on Mac）

- Mac mini / Mac Studio 装 phone-control，作为 Jenkins **Dedicated Agent**。
- Agent 必须以 **GUI 用户自动登录（Auto-login）+ LaunchAgent** 启动，**不能**用 LaunchDaemon 后台服务 —— 否则拿不到 macOS GUI 上下文、屏幕录制（TCC）权限、USB/ADB 硬件访问。
- phone-control 支持 **Tray-Only 启动**（窗口隐藏 + 系统托盘常驻，控制 API + 轮询照常跑），由 LaunchAgent 拉起。托盘「Open GUI」可随时拉出窗口调试。

### headless 启动

两种等效触发方式（二选一）：

```bash
# 1) CLI flag（bundled .app 里的可执行文件）
/Applications/phone-control.app/Contents/MacOS/phone-control --headless

# 2) 环境变量（LaunchAgent plist 里更自然）
PHONE_CONTROL_HEADLESS=1 /Applications/phone-control.app/Contents/MacOS/phone-control
```

LaunchAgent 示例 `~/Library/LaunchAgents/com.mac.phone-control.plist`（**用户级 Agent，非 Daemon**，保证 GUI/TCC 上下文）：

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>com.mac.phone-control</string>
  <key>ProgramArguments</key>
  <array>
    <string>/Applications/phone-control.app/Contents/MacOS/phone-control</string>
    <string>--headless</string>
  </array>
  <key>EnvironmentVariables</key>
  <dict>
    <!-- 若 adb/scrcpy 不在默认 PATH，可显式指定 -->
    <key>ADB_PATH</key><string>/opt/homebrew/bin/adb</string>
    <key>SCRCPY_PATH</key><string>/opt/homebrew/bin/scrcpy</string>
  </dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
</dict>
</plist>
```

> 首次运行仍需在「系统设置 → 隐私与安全性 → 屏幕录制」里授权 phone-control（TCC 无法脚本静默授权），之后 LaunchAgent 常驻即可。CI 用 `curl localhost:9090/api/v1/health` 探活。

## 5. 分阶段落地

### Phase 1 — API 化 + 基础链路（本分支起步）
- [x] 抽 `install_apk` 共享逻辑（Tauri command 与控制 API 共用）
- [x] axum 控制 API 骨架：health / devices / acquire / release / install
- [x] `acquire` 释放设备 scrcpy 控制权（决策 #3）
- [x] `acquire` 单设备粒度 + `any`/`serial` 过滤 + `expires_at` TTL（默认 15min）+ 后台 sweeper 自动回收（决策 #1）
- [x] Bearer-token 鉴权（env `PHONE_CONTROL_TOKEN` 或 `~/.phone_control/api_token`），仅绑 `127.0.0.1`（决策 #3-auth）
- [x] headless 启动（`--headless` / `PHONE_CONTROL_HEADLESS` env）+ Tray-Only 常驻（决策 #4）
- [ ] 3~5 条最核心 Android Maestro 用例（安装/启动/登录/首页）
- [ ] Mac mini 配 Jenkins Agent，跑通「打包 → acquire → install → maestro → 结果」

> ⚠️ 现实修正：原方案把「API 化 + iOS 冒烟」都压进 1~2 周。**iOS 是独立大头**（另一套 tidevice/simctl 工具链），Phase 1 只做 Android，iOS 单独排期。

### Phase 2 — 采集与产物聚合
- [ ] **录屏（决策 #2）：scrcpy 纯视频流（video-only）→ Pure Rust muxer 落盘 `.mp4`**
  - 租约模式下 phone-control 断开 control socket、以 `control=false` 重连拿视频流
  - 侧读 H.264 NAL → **Pure Rust muxer（`muxide` 或 `mp4e`）** 写盘
  - **muxer 选型：坚决用 Pure Rust，放弃 FFmpeg**
    - ✅ Pure Rust：零外部依赖（编进单文件）、`cargo build` 一键出包、增量 <1MB、无 IPC 开销（在 scrcpy 接收 Loop 里顺手落盘）、内存安全无 C-FFI 崩溃/僵尸进程
    - ❌ FFmpeg FFI（`ffmpeg-next`/`ac-ffmpeg`）：Universal Binary 交叉编译易断裂，C crash 直接砸崩 Tauri
    - ❌ FFmpeg CLI sidecar：+70MB 打包、Pipe 二次拷贝、强杀遗留僵尸进程；仅作开发期调试备选
  - scrcpy 流天然适配 Pure Rust muxer：首帧 header 带原始分辨率、包自带微秒 PTS、IDR 前固定带 SPS(0x67)/PPS(0x68)；muxer 负责 Annex B→AVCC、构 `avcC`/`moov`、`finish()` 时写 fast-start（CI/飞书/Linear 免下载在线预览）
  - ⚠️ **屏幕旋转坑**：竖→横切换时 scrcpy 重发新分辨率 + 新 SPS/PPS；MP4 不重编码无法中途改 track 分辨率。解法：监听分辨率变化 → `finish()` 当前段 → 新分辨率开 `part2.mp4`；报告按时序挂多段（需单文件再 CI 侧 ffmpeg 拼），比 Rust 端实时重编码高效得多
  - ❌ 不用 `adb screenrecord`（3min 限制 + 占存 + 多一次 pull I/O）
- [ ] `adb logcat` per task 落盘到 `output-dir`；错误时 `adb exec-out screencap` 截图
- [ ] `smoke/report`：产物打包为 JUnit XML + JSON 供 Jenkins 解析
- [ ] 飞书/钉钉机器人推送（编排层）

### Phase 3 — AI 诊断与 Linear 联动（编排层，不进 App）
- [ ] 失败时把 logcat + 截图喂给 Claude 做根因分类（崩溃 / 环境 / 用例失效）
- [ ] 经 Linear MCP 自动建单，挂载日志/录屏/AI 分析

## 6. 决策记录（原开放问题已定）

1. **设备分配粒度** ✅ 单设备粒度，一次一台，**不做多设备组绑定**。并行 Job 各自调一次 `/acquire`。过滤：`any`（默认，任意空闲 Android）/ `serial`（精确机型，兼容性复现）。
2. **录屏方案** ✅ **scrcpy 纯视频流写文件**（非 `adb screenrecord`）。理由见 Phase 2。
3. **API 鉴权** ✅ `127.0.0.1` 绑定 + 动态 Bearer Token（env 或 `~/.phone_control/api_token`）。防本机杂散进程误触发 ADB。
4. **headless 形态** ✅ 非纯 Daemon，而是 **Tray-Only（隐藏窗口 + 系统托盘）**。macOS TCC（屏幕录制/辅助功能/USB）需要 LaunchAgent 提供的 Aqua GUI 上下文，纯 LaunchDaemon 会导致 ADB 受阻或录屏黑屏。托盘「Open GUI」可随时拉出窗口调试。

### 租约状态机（决策 #1 + #3）
```
Idle ──/acquire──▶ Leased(task_id, expires_at)
                      │  ├─ /release            ──▶ Idle
                      │  ├─ expires_at 到期(sweeper)──▶ Idle   ← 防僵尸锁
                      │  └─ acquire 时同步 stop_stream_loop(force) 释放 UI 控制权（决策 #3）
```
