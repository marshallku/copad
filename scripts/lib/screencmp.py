#!/usr/bin/env python3
"""Compare what an outer terminal SHOWS with what a comux pane HOLDS.

    screencmp.py <pane.txt> <outer.txt> <x> <y> <cols> <rows> [--label L]

`pane.txt`  = `comux capture-pane` of the pane (its rows, top-down).
`outer.txt` = the outer terminal's screen as text (tmux capture-pane -p / coctl terminal read).
<x> <y>     = the pane's origin on the outer screen; <cols> <rows> its size.

Every pane row must appear at exactly that position on the outer screen. Rows are walked by
display column, so a CJK glyph counts two and combining marks count none, and trailing blanks
do not matter. A tab in the pane text is a blank: what it left on screen is blank cells.
Exits 1 when a row differs, naming the row and both texts.
"""
import sys
import unicodedata


def is_zero_width(ch):
    return (
        unicodedata.category(ch) in ("Mn", "Me", "Cf")
        or 0x1160 <= ord(ch) <= 0x11FF  # Hangul Jamo vowels/finals combine into the initial
        or 0xFE00 <= ord(ch) <= 0xFE0F
    )


def cells(line):
    """One entry per display column; the trailing half of a wide glyph is ''."""
    out = []
    for ch in line.replace("\t", " "):
        if is_zero_width(ch):
            if out:
                out[-1] += ch
            continue
        out.append(ch)
        if unicodedata.east_asian_width(ch) in ("W", "F"):
            out.append("")
    return out


def norm(cs):
    # Compare composed glyphs, so an NFD filename matches however an emulator stored it. A VS16
    # does not count: comux renders the base glyph on purpose (decisions, snapshot_grid).
    return unicodedata.normalize("NFC", "".join(cs)).replace("\ufe0f", "").rstrip(" ")


def main():
    argv = sys.argv[1:]
    label = ""
    if "--label" in argv:
        i = argv.index("--label")
        label = argv[i + 1] + ": "
        del argv[i : i + 2]
    pane_path, outer_path, x, y, cols, rows = argv
    x, y, cols, rows = int(x), int(y), int(cols), int(rows)
    pane = open(pane_path, encoding="utf-8", errors="replace").read().removesuffix("\n").split("\n")
    outer = open(outer_path, encoding="utf-8", errors="replace").read().split("\n")
    # capture-pane joins a soft-wrapped line; put it back on the rows it occupies.
    wrapped = []
    for line in pane:
        cs = cells(line)
        wrapped.extend(cs[i : i + cols] for i in range(0, max(len(cs), 1), cols))
    wrapped = wrapped[-rows:]
    bad = 0
    for r in range(rows):
        want = wrapped[r] if r < len(wrapped) else []
        got_row = cells(outer[y + r] if y + r < len(outer) else "")
        got = got_row[x : x + cols]
        if norm(want) != norm(got):
            bad += 1
            if bad <= 6:
                print(f"{label}row {r}\n  pane : {norm(want)!r}\n  outer: {norm(got)!r}")
    if bad:
        print(f"{label}{bad} row(s) differ")
        sys.exit(1)
    print(f"{label}ok ({rows} rows)")


main()
