# Privacy Policy

*copad for iPhone and iPad. Last updated: 29 October 2026.*

copad connects your iPhone or iPad to computers you run yourself. It has no account, no
analytics and no advertising, and the developer never sees your terminals.

## What stays on your device

- **Hosts** — names, addresses, ports, user names, the command that starts comux, and the host
  keys you chose to trust. Stored in the app on this device only.
- **Your device key** — new installations create a P-256 key inside the device's Secure Enclave;
  it can be used but never read out. Installations made before that keep an Ed25519 key in this
  device's Keychain, available on this device only. Either way the private key never leaves the
  device; you copy the *public* key to the hosts you want to reach.

## What goes to your own computers

Everything you type and everything your terminals show travels over SSH directly between the app
and the host you connect to. The developer runs no server in that path.

The app gives each comux host you connect to this device's notification token and a key that only
this device's copad can open notifications with, so the host can tell you when an agent finishes or
needs you. It does so whether or not you allow notifications (iOS decides whether one is shown;
turning notifications off in Settings stops them appearing), unless the host is set to send none.

## Notifications through the push relay

A comux host sends its notifications through a relay run by the developer
(`aide-push.marshallku.dev`), which passes them to Apple's push service. Every notification is
**end-to-end encrypted** on your host: the relay and Apple see the device token, a fixed
placeholder text and ciphertext, never what an agent said. The relay stores no notifications. It
keeps a short-lived, in-memory record of recent device tokens to limit how often a device can be
notified, and when Apple rejects a token its logs keep a 4-byte hash of that token, which does not
identify you or your device to anyone else. It sees the network address of the host sending a
notification (not of your device) and uses it in memory only, to limit how often one host can send. A host can be set to use its own Apple key instead,
or to send no notifications (`[push]` in `mux.toml`).

## What is not collected

No contact details, location, usage data, diagnostics, identifiers for advertising or tracking.
Nothing is sold or shared with third parties.

## Contact

Questions or requests: open an issue at <https://github.com/marshallku/copad/issues>.

---

# 개인정보 처리방침

*iPhone·iPad용 copad. 최종 수정: 2026년 10월 29일.*

copad는 iPhone이나 iPad를 사용자가 직접 운영하는 컴퓨터에 연결하는 앱입니다. 계정, 분석 도구,
광고가 없고 개발자는 사용자의 터미널을 볼 수 없습니다.

## 기기에만 저장되는 정보

- **호스트** — 이름, 주소, 포트, 사용자 이름, comux 실행 명령, 신뢰하기로 한 호스트 키. 이 기기의
  앱 안에만 저장됩니다.
- **기기 키** — 새로 설치하면 기기의 Secure Enclave 안에 P-256 키를 만듭니다. 사용만 할 수 있고
  밖으로 꺼낼 수 없습니다. 그 이전에 설치한 앱은 이 기기의 키체인에 Ed25519 키를 보관합니다. 어느
  쪽이든 개인 키는 기기를 떠나지 않으며, 접속할 호스트에는 *공개* 키만 복사합니다.

## 사용자 컴퓨터로 가는 정보

입력하는 내용과 터미널에 표시되는 내용은 모두 SSH로 앱과 접속한 호스트 사이를 직접 오갑니다. 그
경로에 개발자가 운영하는 서버는 없습니다.

앱은 접속하는 각 comux 호스트에 이 기기의 알림 토큰과, 이 기기의 copad만 열 수 있는 알림용 키를
전달합니다. 호스트는 에이전트가 작업을 마치거나 응답을 기다릴 때 알리기 위해 이를 보관합니다. 알림
허용 여부와 관계없이 전달되며(알림 표시 여부는 iOS가 정하고, 설정에서 알림을 끄면 표시되지 않습니다),
호스트가 알림을 보내지 않도록 설정돼 있으면 보관하지 않습니다.

## 푸시 릴레이를 거치는 알림

comux 호스트는 개발자가 운영하는 릴레이(`aide-push.marshallku.dev`)를 통해 Apple 푸시 서비스로
알림을 보냅니다. 모든 알림은 호스트에서 **종단 간 암호화**되므로 릴레이와 Apple은 기기 토큰, 고정된
안내 문구, 암호문만 보고 에이전트가 한 말은 볼 수 없습니다. 릴레이는 알림을 저장하지 않습니다. 한
기기로 가는 알림 빈도를 제한하려고 최근 기기 토큰을 메모리에 잠시 기록하며, Apple이 토큰을 거부하면
그 토큰의 4바이트 해시를 로그에 남깁니다. 이 해시로는 누구도 사용자나 기기를 알아낼 수 없습니다.
알림을 보내는 호스트(사용자 기기가 아님)의 네트워크 주소는 한 호스트의 전송 빈도를 제한하는 데만
메모리에서 사용합니다.
호스트는 자체 Apple 키를 쓰거나 알림을 보내지 않도록 설정할 수 있습니다(`mux.toml`의 `[push]`).

## 수집하지 않는 정보

연락처, 위치, 사용 기록, 진단 정보, 광고·추적용 식별자를 수집하지 않습니다. 어떤 정보도 판매하거나
제3자와 공유하지 않습니다.

## 문의

문의나 요청은 <https://github.com/marshallku/copad/issues>에 이슈로 남겨 주세요.
