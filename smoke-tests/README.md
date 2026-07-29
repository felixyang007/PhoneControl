# Smoke tests

P0 smoke flows (Maestro) + an orchestrator that drives phone-control's control
API. Design & rationale: [`../docs/smoke-test-integration.md`](../docs/smoke-test-integration.md).

Role split (decision #3): **phone-control** does device scheduling + install +
capture (mp4 + logcat); **Maestro** drives the UI. The orchestrator glues them.

## Files
- `settings-smoke.yaml` — runnable-anywhere demo flow (targets `com.android.settings`); use it to validate the pipeline without the product APK.
- `app-smoke.template.yaml` — P0 template for the product app; copy, fill TODOs, pass `--env APP_ID=...`.
- `smoke-run.sh` — orchestrator: acquire → [install] → capture/start → `maestro test` → capture/stop → report → release. Exit code is Maestro's.

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
```bash
sh 'smoke-tests/smoke-run.sh --flow smoke-tests/app-smoke.yaml --apk app-debug.apk --task ${BUILD_TAG} --output-dir ${WORKSPACE}/artifacts'
// artifacts/<task>-manifest.json lists the mp4 + logcat to archive / attach to Linear
```
