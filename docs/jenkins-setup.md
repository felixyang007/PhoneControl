# Jenkins 接入指南 — Mac 设备节点跑冒烟

把一台 Mac（mini / Studio）配成 Jenkins 节点，跑 [`smoke-tests/Jenkinsfile`](../smoke-tests/Jenkinsfile)。
流水线本身见 [`smoke-test-integration.md`](smoke-test-integration.md)；本文只讲**怎么配 Jenkins**。

> ⚠️ 最关键的坑：Mac 节点**必须以「自动登录 GUI 用户 + LaunchAgent」运行，绝不能用 LaunchDaemon / SSH 后台**。
> 否则没有 Aqua 图形会话 → 屏幕录制（TCC）拿不到、USB/ADB 也可能不可见。这是整个方案能否跑通的前提。

---

## 0. 节点机器一次性准备

```bash
# 工具链（Homebrew）
brew install --cask android-platform-tools   # adb
brew install scrcpy jq
brew install --cask temurin                   # JDK（Jenkins agent 需要）
curl -Ls https://get.maestro.mobile.dev | bash   # maestro → ~/.maestro/bin

# 设备可见
adb devices          # 真机需已授权；模拟器需已启动
```

- **自动登录**：系统设置 → 用户与群组 → 自动以该用户登录。
- **TCC 授权**（系统设置 → 隐私与安全性），给**运行 Jenkins agent 的那个 java 进程**（首次运行会弹，或手动添加）：
  - **屏幕录制** ← scrcpy 投屏/录屏必需
  - **辅助功能** ← 部分自动化必需
- 关闭「节能自动睡眠」，否则 CI 空闲后设备/会话会掉。

## 1. phone-control 常驻（控制 API）

phone-control 需一直在跑（控制 API `127.0.0.1:9090`）。用 LaunchAgent 开机自启——plist 模板见 [`smoke-test-integration.md` §4](smoke-test-integration.md#4-部署拓扑jenkins-agent-on-mac)。
- token 落在 `~/.phone_control/api_token`，`smoke-run.sh` 默认读它。
- 探活：`curl -sf http://127.0.0.1:9090/api/v1/health`。
- 你也可以就让它**前台开着**（见 README 常驻说明）——控制 API 与窗口解耦，前台一样能被 CI 调。

## 2. 在 Jenkins 里建节点（Node/Agent）

Manage Jenkins → Nodes → **New Node** → Permanent Agent：
- **Labels**: `mac-device-lab`（Jenkinsfile 里 `agent { label 'mac-device-lab' }` 就是它）
- **Remote root directory**: 例如 `/Users/<gui-user>/jenkins-agent`
- **Launch method**: **Launch agent by connecting it to the controller**（inbound / JNLP）
- **# of executors**: `1`（一台设备一次一个冒烟，配合 Jenkinsfile 的 `disableConcurrentBuilds`）

保存后 Jenkins 会给出该节点的 `secret` 和 `agent.jar` 下载地址。

## 3. 让 agent 以 LaunchAgent 启动（拿到 GUI 上下文）

`~/Library/LaunchAgents/com.plaud.jenkins-agent.plist`（**LaunchAgent，非 Daemon**）：

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>com.plaud.jenkins-agent</string>
  <key>ProgramArguments</key>
  <array>
    <string>/opt/homebrew/opt/temurin/bin/java</string>
    <string>-jar</string><string>/Users/GUI_USER/jenkins-agent/agent.jar</string>
    <string>-url</string><string>https://jenkins.your.co/</string>
    <string>-secret</string><string>PASTE_NODE_SECRET</string>
    <string>-name</string><string>mac-device-lab</string>
    <string>-workDir</string><string>/Users/GUI_USER/jenkins-agent</string>
  </array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key><string>/opt/homebrew/bin:/Users/GUI_USER/.maestro/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
  </dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>/tmp/jenkins-agent.out</string>
  <key>StandardErrorPath</key><string>/tmp/jenkins-agent.err</string>
</dict>
</plist>
```

```bash
launchctl load ~/Library/LaunchAgents/com.plaud.jenkins-agent.plist
```

节点在 Jenkins 里变绿即连上。

## 4. 建流水线 Job

New Item → **Pipeline** → Pipeline → **Pipeline script from SCM**：
- SCM: Git，指向本仓库；Script Path: `smoke-tests/Jenkinsfile`。
- 参数（Jenkinsfile 已声明）：`FLOW` / `APK_PATH` / `DEVICE_SERIAL` / `FILE_LINEAR`。

**由打包上游触发**：在 APK 构建 Job 末尾用 `build job: 'smoke', parameters: [string(name:'APK_PATH', value: "${WORKSPACE}/app-debug.apk")]`，或用 Copy Artifact 插件把 APK 拉到本 Job 的 workspace 再传 `--apk`。

## 5. 结果与产物

- **测试结果**：`junit artifacts/junit.xml` → Jenkins 的 Test Result 页显示每条 Maestro flow 的通过/失败。
- **产物**：`archiveArtifacts artifacts/**` → 每次构建挂 mp4 + logcat + `<task>-manifest.json`（失败时经 trap 也一定生成）。
- **失败自动归因**：勾 `FILE_LINEAR` → 只对 [A] 崩溃建 Linear 单（需节点上配好 Linear MCP，见 [`smoke-tests/README.md` Phase 3 生产接入](../smoke-tests/README.md)）。

## 6. 常见坑

| 症状 | 原因 / 解法 |
|---|---|
| 录屏黑屏 / 0 帧 | agent 不是 GUI 上下文（用了 Daemon / SSH）或没给「屏幕录制」权限 |
| `phone-control not up on :9090` | phone-control 没起，或 token 不对；先手动 `curl .../health` |
| `adb: command not found` | 非登录 shell 没继承 PATH；已在 plist `EnvironmentVariables.PATH` 里加 Homebrew |
| `maestro: command not found` | 同上，PATH 里加 `~/.maestro/bin` |
| 构建被 abort 后设备没释放 | Jenkins abort 发 SIGTERM，`smoke-run.sh` 的 trap 会 release；`kill -9` 才会漏，靠 15min TTL sweeper 兜底 |
| 多 Job 抢同一台设备 | `disableConcurrentBuilds` + `/devices/acquire` 租约（`leased_by` 可查）已防抢 |
