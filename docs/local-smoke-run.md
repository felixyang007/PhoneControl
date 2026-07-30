# 本地跑冒烟测试（不经 Jenkins）

在你自己的 Mac 上把整条链路跑一遍：**清装 → 采集（录屏+logcat）→ Maestro 用例 → 产物 + JUnit**。
Jenkins 接入见 [`jenkins-setup.md`](jenkins-setup.md)；设计见 [`smoke-test-integration.md`](smoke-test-integration.md)。

---

## 1. 前置条件（一次性）

```bash
# 工具
brew install --cask android-platform-tools   # adb
brew install scrcpy jq
curl -Ls https://get.maestro.mobile.dev | bash   # maestro → ~/.maestro/bin

# 让 maestro 在 PATH 上（每个新终端都要，或写进 ~/.zshrc）
export PATH="$PATH:$HOME/.maestro/bin"

# 设备就绪：真机已授权，或模拟器已启动
adb devices          # 记下 serial，例如 emulator-5556
```

## 2. 启动 phone-control（控制 API 必须在跑）

smoke-run 通过 `127.0.0.1:9090` 驱动 phone-control，所以它得先起来：

```bash
npm run tauri dev            # 前台带窗口；或 PHONE_CONTROL_HEADLESS=1 无窗口
```

确认控制 API 通了：

```bash
curl -s localhost:9090/api/v1/health          # → {"status":"ok",...}
```

> token 自动生成在 `~/.phone_control/api_token`（**下划线**），脚本会自己读，你不用管。

## 3. 先跑「到处可跑」的 demo，确认管道通（无需 APK / 凭据）

```bash
./smoke-tests/smoke-run.sh \
  --serial emulator-5556 \
  --flow "$(pwd)/smoke-tests/settings-smoke.yaml" \
  --task local-demo \
  --output-dir /tmp/smoke --junit /tmp/smoke/junit.xml
```

看到 `[Passed] settings-smoke` + `▸ done · exit=0` 就说明 **acquire / capture / maestro / report** 整条通了。

## 4. 跑真机 Plaud 冒烟（清装 + 登录 + Ask Plaud）

APK 放好（默认 `smoke-tests/app-debug.apk`，已 gitignore）。**凭据用 `--maestro-env` 传，别写进 yaml**：

```bash
./smoke-tests/smoke-run.sh \
  --serial emulator-5556 \
  --uninstall ai.plaud.android.plaud \
  --apk "$(pwd)/smoke-tests/app-debug.apk" \
  --flow "$(pwd)/smoke-tests/plaud-smoke.yaml" \
  --maestro-env "LOGIN_USER=你的QA邮箱" \
  --maestro-env "LOGIN_PASS=你的QA密码" \
  --task local-plaud \
  --output-dir /tmp/smoke --junit /tmp/smoke/junit.xml
```

链路：`acquire → uninstall → install → capture/start → maestro → capture/stop → report → release`。

参数速查：

| 参数 | 作用 |
|---|---|
| `--serial` | 指定设备；不给则租任意空闲设备 |
| `--uninstall <pkg>` | 装前先卸（clean install，避免 debug 包签名冲突） |
| `--apk <绝对路径>` | 要装的包；**用绝对路径**（见坑 2） |
| `--flow <绝对路径>` | Maestro 用例 |
| `--maestro-env "K=V"` | 透传给 maestro 的 `--env`（凭据等），可重复 |
| `--output-dir <dir>` | 产物落盘目录 |
| `--junit <path>` | 写 JUnit XML |
| `--triage` | 失败时跑 AI 根因分析（需 `claude` CLI） |

## 5. 看产物

跑完 `--output-dir`（上例 `/tmp/smoke`）里：

```bash
ls -lh /tmp/smoke/
cat /tmp/smoke/local-plaud-manifest.json          # 产物清单 + exit_code + device_info
open /tmp/smoke/local-plaud-*.mp4                  # 录屏
tail -50 /tmp/smoke/local-plaud-*.logcat.txt       # 日志
ffprobe /tmp/smoke/local-plaud-*.mp4               # 校验 mp4（可选）
```

- `exit_code=0` = 通过；非 0 = 失败（脚本退出码 = Maestro 退出码）。
- 失败时 Maestro 的调试截图在 `~/.maestro/tests/<时间戳>/`。

## 6. 常见坑

| 症状 | 解法 |
|---|---|
| `no API token` / acquire 失败 | phone-control 没起；先 `curl localhost:9090/api/v1/health` |
| `maestro: command not found` | `export PATH="$PATH:$HOME/.maestro/bin"` |
| `APK not found` | `--apk` 用**绝对路径**（`$(pwd)/...`）——`/install` 在 phone-control 进程的 CWD 里解析路径，相对路径会找不到 |
| `DeviceServerDied` / `tcp closed` | Maestro 驱动/adb 端口抖动。脚本已**自动重启 adb 重试一次**；仍不行就 `adb kill-server && adb start-server`，或重启模拟器 |
| flow 卡很久才失败 | 某个 `tapOn` 找不到元素在反复重试。给等待类步骤加 `extendedWaitUntil ... timeout`，或用 `maestro studio` 找稳定 id |
| 401 / token 对不上 | token 在 `~/.phone_control/api_token`（**下划线**）；`PHONE_CONTROL_TOKEN` 环境变量优先级高于文件，别乱设 |

## 7. 只调 flow、不重装（迭代更快）

改 yaml 时不用每次清装 134M APK，直接单独跑 maestro：

```bash
export PATH="$PATH:$HOME/.maestro/bin"
maestro test --device emulator-5556 \
  --env LOGIN_USER=... --env LOGIN_PASS=... \
  smoke-tests/plaud-smoke.yaml
```

调好后再用第 4 步的完整命令跑一遍出产物。
