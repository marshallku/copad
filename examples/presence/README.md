# Presence detector recipes

`nestctl presence away|active` is a thin pipe. Whether you are "away" is policy that depends on your environment (Wayland compositor, screensaver daemon, lockscreen behaviour, work patterns), so nestty itself ships zero idle-detection logic. The recipes here are starting points — pick one, edit it to taste, drop it into your dotfiles / systemd-user / Hyprland exec-once / LaunchAgent.

## Picking a recipe

| Recipe | When to use |
|---|---|
| `hypridle-listener.conf` | You use Hyprland and already run hypridle. Paste the listener block into your `~/.config/hypr/hypridle.conf` — no new daemon. |
| `swayidle.sh` | You use any wlroots compositor (Sway / Hyprland / river / wayfire). One daemon process handles both idle + resume cleanly. |
| `loginctl-poll.sh` | DE-agnostic / headless / "I don't want a new daemon." Polls `loginctl show-session $XDG_SESSION_ID --property=IdleHint` every 30 s. |
| `darwin-pmset.sh` | macOS. Reads `pmset -g` / `ioreg HIDIdleTime` once a minute. Crude but works without LaunchAgents-on-idle plumbing. |

Lock screen as a hard "away" signal — most lockers (hyprlock, swaylock, gtklock, loginctl lock-session) emit a `org.freedesktop.ScreenSaver` D-Bus signal or a logind `Lock` signal. The Hyprland recipe demonstrates wiring `loginctl lock-session` to also flip presence; the same idea works on any logind-managed session.

## Threshold (how long is "away")

Default suggestion in the recipes: **5 minutes** (`300` seconds). Tradeoff:

- shorter (60-180 s): Discord pings the moment you step away from the keyboard — even quick context switches like reading a doc on a second monitor.
- longer (600+ s): you find out about commit blocks 10 minutes after they happen.

Tune to your work pattern. The recipes are commented at the threshold knob.

## After installing

Verify the detector actually fires:

```
# in one terminal, watch the presence stream
nestctl event subscribe | grep -E 'presence|claude\.'

# in another, wait out the idle threshold, then check
nestctl presence status   # should print `away`
```

If you don't see a `presence.changed` event around your timeout, the detector is not reaching the daemon — usually a `PATH` problem (the detector running outside your shell env doesn't have `nestctl` on PATH; either use the absolute path `~/.local/bin/nestctl` in the recipe or add `PATH=$HOME/.local/bin:$PATH` to the unit/service that runs the detector).

## Adding your own backend

If you write a backend for an environment not covered here (KDE / X11 / GNOME / mobile geo-fence / motion sensor / whatever), the contract is just "call `nestctl presence away` when you decide the user is gone, `nestctl presence active` when they come back." No need to coordinate with nestty.

When a backend stabilises into something you have used for months and would recommend to others, that's the right moment to consider promoting it to a first-party plugin (`plugins/presence-<backend>/`). Until then, keep it in your dotfiles where you can iterate freely.
