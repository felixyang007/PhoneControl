# Smoke tests

P0 smoke flows (Maestro) + an orchestrator that drives phone-control's control
API. Design & rationale: [`../docs/smoke-test-integration.md`](../docs/smoke-test-integration.md).

Role split (decision #3): **phone-control** does device scheduling + install +
capture (mp4 + logcat); **Maestro** drives the UI. The orchestrator glues them.

## Files
- `settings-smoke.yaml` — runnable-anywhere demo flow (targets `com.android.settings`); use it to validate the pipeline without the product APK or creds.
- `plaud-smoke.yaml` — the real Plaud P0 flow (login → Ask Plaud). **Creds are not hardcoded** — pass them at run time (see below), so no secret is committed.
- `app-smoke.template.yaml` — P0 template for the product app; copy, fill TODOs, pass `--env APP_ID=...`.

### Credentials (never commit them)
`plaud-smoke.yaml` uses `${LOGIN_USER}` / `${LOGIN_PASS}`. Inject at run time:
```bash
smoke-tests/smoke-run.sh --flow smoke-tests/plaud-smoke.yaml \
  --uninstall ai.plaud.android.plaud --apk ./app-debug.apk \
  --maestro-env "LOGIN_USER=…" --maestro-env "LOGIN_PASS=…" \
  --serial emulator-5556 --output-dir /tmp/art --junit /tmp/art/junit.xml
```
In Jenkins these come from a `usernamePassword` credential (`plaud-qa-login`) — see `Jenkinsfile`.
- `smoke-run.sh` — orchestrator: acquire → [install] → capture/start → `maestro test` → capture/stop → report → release. Exit code is Maestro's. `--triage` runs AI root-cause on failure; `--triage-create` also files a Linear bug.
- `smoke-triage.sh` + `triage-prompt.md` — Phase 3: on a failed run, extract crash/network signals from the logcat and ask Claude to classify (A crash / B env / C flaky-UI); with `--create`, file a Linear bug (class [A] only) via the Linear MCP. Runs in CI, not in the app.

## Prerequisites
- phone-control running (control API on `127.0.0.1:9090`); token at `~/.phone_control/api_token`.
- A connected device/emulator (`adb devices`).
- [Maestro](https://maestro.mobile.dev): `curl -Ls https://get.maestro.mobile.dev | bash`
  (the orchestrator still runs acquire/capture/report if Maestro is absent — it just skips the UI flow.)

## Run
```bash
# demo flow, any idle device
./smoke-run.sh --flow settings-smoke.yaml

# product flow on a specific device, installing a build first
./smoke-run.sh --flow app-smoke.yaml --serial emulator-5556 --apk ./build/app-debug.apk --task build-1234
```

## Jenkins
Ready-to-use pipeline: [`Jenkinsfile`](Jenkinsfile). Full node/agent setup (the
GUI-context/LaunchAgent requirement, TCC, credentials, job creation):
[`../docs/jenkins-setup.md`](../docs/jenkins-setup.md). 本地跑（不经 Jenkins）见 [`../docs/local-smoke-run.md`](../docs/local-smoke-run.md).

Minimal shell if you'd rather script it yourself:
```bash
sh 'smoke-tests/smoke-run.sh --flow smoke-tests/app-smoke.yaml --apk app-debug.apk \
      --task ${BUILD_TAG} --output-dir ${WORKSPACE}/artifacts --junit ${WORKSPACE}/artifacts/junit.xml --triage'
// junit artifacts/junit.xml → Test Result; artifacts/<task>-manifest.json → mp4+logcat+AI triage
```

`smoke-run.sh` is exception-safe: a Maestro failure, a CI SIGTERM, or Ctrl-C all
still run capture/stop + report + release (via a `trap`), so artifacts are always
finalized and the device returned — not left for the 15-min TTL sweeper. The
script exits with Maestro's code, so CI passes/fails on the flow.

### `<task>-manifest.json` — the bridge to the orchestration/AI layer
The single JSON the AI-triage step reads; every path it needs is in here.
```json
{
  "task_id": "BUILD-1024",
  "exit_code": 1,                       // Maestro's result, injected by smoke-run.sh
  "artifacts": [                        // array: one entry per device (parallel-safe)
    {
      "serial": "emulator-5556",
      "mp4_path": "/…/BUILD-1024-emulator-5556-<ts>.mp4",
      "logcat_path": "/…/BUILD-1024-emulator-5556-<ts>.logcat.txt",
      "frame_count": 55,
      "duration_ms": 28787,
      "device_info": { "model": "Pixel 6", "os_version": "13" }
    }
  ]
}
```
Logcat is closed with SIGTERM (not SIGKILL), so the last flushed lines — often the
crash stacktrace — are preserved.

## Phase 3 — 生产接入（AI 归因 → Linear）

`--triage`（只分析、dry-run）在 CI 上只需 `claude` CLI。`--triage-create`（自动建 Linear 单）额外需要在 **CI runner** 上配好：

1. **Linear MCP**：`claude mcp add --transport sse linear https://mcp.linear.app/sse`，并**预先完成 OAuth 授权**（headless `claude -p` 不会弹交互授权）。无法交互授权的环境改用带 `LINEAR_API_KEY` 的 Linear MCP。
2. **headless 权限**：`claude -p` 拿不到人工批准，需为该运行放行 Linear MCP 工具（权限模式 / 工具 allowlist）。仅放行建单所需工具，别全放开。
3. **目标 team / label**：按你们 Linear 规范定死（改 `triage-prompt.md` 的 ACTION，或给 `smoke-triage.sh` 传 team/label）。当前演示用的是 `For self-testing` team、无 label——生产务必改。

安全边界：只有 **[A] 崩溃** 才建单（[B]/[C] 只出分析），避免环境抖动/用例失效刷单。`smoke-triage.sh` 默认 dry-run，`--create` 才写 Linear。
