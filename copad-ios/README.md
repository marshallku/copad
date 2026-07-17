# copad-ios

A **thin native iOS shell** around copad's `web-bridge` PWA. The whole UI —
terminal attach (xterm.js), presence, live events, pilot cockpit — comes from
web-bridge loaded in a `WKWebView`. The native layer adds only:

- an app shell + home-screen presence (so it can eventually receive APNs
  background push — the one thing a PWA is weak at on iOS);
- a validated **server-URL** setting (https, or http for loopback only);
- a **local-notification permission** scaffold + status.

**Auth is the PWA's job.** There is deliberately no native token field — the
web-bridge page reads `sessionStorage["copad.token"]` and shows its own token
prompt; a native duplicate would double-entry a high-authority secret and store
it insecurely. See [docs/mobile-access.md](../docs/mobile-access.md).

## Status

v1 (Simulator-verified 2026-07-17): app builds for the iOS Simulator, loads the
web-bridge PWA over a configured URL, restricts navigation to that origin, shows
a failure/retry UI, and surfaces the local-notification permission state.

**Not yet built** (needs an Apple developer account + a real device):
- `registerForRemoteNotifications` + APNs — real background push. Pairs with
  web-bridge **WU2b** (APNs device-token registration + send). v1 intentionally
  does NOT call it (no `aps-environment` entitlement → would only fail).
- Native keyboard accessory (the PWA ships its own on-screen `.kbd-bar`).
- Keychain token seeding, biometric gate, TestFlight distribution.

## Build & run (Simulator)

Requires Xcode + [`xcodegen`](https://github.com/yonaskolb/XcodeGen)
(`brew install xcodegen`). The `.xcodeproj` is generated from `project.yml`
(the reviewable source of truth) and is gitignored.

```bash
cd copad-ios
xcodegen generate
open copad-ios.xcodeproj      # then run on a simulator from Xcode
```

Headless build + launch + screenshot (deterministic from a clean state — picks
the newest iOS runtime + an iPhone, boots it, preflights the web-bridge, uninstalls
any prior copy, resets notification privacy):

```bash
# 1. start a local web-bridge (see docs/mobile-access.md) on 127.0.0.1:7575
# 2. then:
./scripts/verify-sim.sh                       # defaults to http://127.0.0.1:7575
./scripts/verify-sim.sh https://host.ts.net   # or a real Tailscale URL
```

The Simulator shares the host's `localhost`, so `http://127.0.0.1:7575` reaches a
web-bridge running on your Mac (allowed by the scoped `NSAllowsLocalNetworking`
ATS exception; the real `https://*.ts.net` path needs no exception).

## Layout

```
project.yml          # xcodegen spec (bundle id, iOS 17 target, ATS) — the SoT
Sources/
  CopadApp.swift     # @main App
  ContentView.swift  # web screen ↔ settings; failure/retry banner
  WebView.swift      # WKWebView: load-on-URL-change, nav policy, failure UI
  AppModel.swift     # URL validation/persistence, notification status
scripts/verify-sim.sh
```
