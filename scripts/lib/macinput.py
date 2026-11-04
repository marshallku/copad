#!/usr/bin/env python3
"""Synthesized input for the macOS e2e, posted as HID events through Quartz.

System Events (`osascript … keystroke`) needs Automation consent on top of Accessibility and is
refused (error 1002) from processes that are trusted for Accessibility alone; it also types `/ .
- 0-9` with keypad codes and adds Option under a Korean input source. Posting CGEvents needs only
Accessibility.

    macinput.py trusted            exit 0 when this process may post events
    macinput.py front PID          bring that app to the front
    macinput.py type TEXT          type TEXT (unicode, key by key)
    macinput.py key CODE [ctrl]    press a virtual key code, optionally with Control
"""
import ctypes
import sys
import time

import AppKit
import Quartz


def trusted():
    lib = ctypes.cdll.LoadLibrary("/System/Library/Frameworks/ApplicationServices.framework/ApplicationServices")
    lib.AXIsProcessTrusted.restype = ctypes.c_bool
    return lib.AXIsProcessTrusted()


def post(code, down, flags=0, text=None):
    src = Quartz.CGEventSourceCreate(Quartz.kCGEventSourceStateHIDSystemState)
    e = Quartz.CGEventCreateKeyboardEvent(src, code, down)
    Quartz.CGEventSetFlags(e, flags)
    if text is not None:
        Quartz.CGEventKeyboardSetUnicodeString(e, len(text), text)
    Quartz.CGEventPost(Quartz.kCGHIDEventTap, e)
    time.sleep(0.02)


def main(argv):
    cmd = argv[1] if len(argv) > 1 else ""
    if cmd == "trusted":
        return 0 if trusted() else 1
    if cmd == "front":
        app = AppKit.NSRunningApplication.runningApplicationWithProcessIdentifier_(int(argv[2]))
        if app is None:
            return 1
        app.activateWithOptions_(AppKit.NSApplicationActivateIgnoringOtherApps)
        time.sleep(0.4)
        return 0
    if cmd == "type":
        for ch in argv[2]:
            post(0, True, text=ch)
            post(0, False, text=ch)
        return 0
    if cmd == "key":
        flags = Quartz.kCGEventFlagMaskControl if "ctrl" in argv[3:] else 0
        post(int(argv[2]), True, flags)
        post(int(argv[2]), False, flags)
        return 0
    print(__doc__, file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv))
