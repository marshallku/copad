#!/usr/bin/env bash
# Color / attribute decoding + Unicode edge cases. Mostly a correctness
# check (look at the output) rather than a perf stress.
#
# What to watch:
#   - 16/256-color and truecolor grids show the expected gradient.
#   - Attribute samples (bold/italic/underline/inverse/strike/dim)
#     render distinctly.
#   - ZWJ emoji collapse into a single grapheme cell (not 4 separate
#     emoji), CJK occupies 2 cells, RTL flows correctly, combining
#     marks stack onto their base char.
#   - Cursor over any glyph remains readable (inverse-glyph re-render).
#
# Usage: ./scripts/stress/colors-unicode.sh

set -u

echo "=== 16 ANSI colors (fg • / bg block, normal + bright) ==="
for i in 0 1 2 3 4 5 6 7; do printf "\e[3${i}m●\e[0m \e[4${i}m  \e[0m  "; done; echo
for i in 0 1 2 3 4 5 6 7; do printf "\e[9${i}m●\e[0m \e[10${i}m  \e[0m  "; done; echo
echo

echo "=== 256-color cube + grayscale ==="
for i in $(seq 16 231); do
    printf "\e[48;5;%dm  \e[0m" "$i"
    [ $(( (i - 15) % 36 )) -eq 0 ] && echo
done
for i in $(seq 232 255); do printf "\e[48;5;%dm  \e[0m" "$i"; done; echo
echo

echo "=== Truecolor gradient (32 rows × 32 cols) ==="
for r in $(seq 0 8 248); do
    for b in $(seq 248 -8 0); do
        printf "\e[48;2;%d;0;%dm \e[0m" "$r" "$b"
    done
    echo
done
echo

echo "=== Attribute samples ==="
printf "\e[1mbold\e[0m  \e[2mdim\e[0m  \e[3mitalic\e[0m  \e[4munderline\e[0m  "
printf "\e[5mblink\e[0m  \e[7minverse\e[0m  \e[9mstrike\e[0m\n"
printf "\e[1;4;31mbold+underline+red\e[0m  "
printf "\e[3;38;5;208mitalic+orange256\e[0m  "
printf "\e[48;2;30;30;46;38;2;243;139;168mtruecolor fg/bg\e[0m\n"
echo

echo "=== Unicode edge cases ==="
echo "ZWJ family:    👨‍👩‍👧‍👦 (4 codepoints + 3 ZWJ → 1 grapheme)"
echo "Skin tone:     👋🏻 👋🏼 👋🏽 👋🏾 👋🏿 (base + Fitzpatrick modifier)"
echo "Profession:    👨🏻‍💻 👩🏿‍🚀 🧑🏽‍🎤 (base + tone + ZWJ + role)"
echo "Regional flag: 🇰🇷 🇯🇵 🇺🇸 🇩🇪 (regional indicator pairs)"
printf 'Combining:     e\xcc\x81 e\xcc\x82 e\xcc\x83 (e + acute/circumflex/tilde)\n'
echo "CJK mix:       안녕하세요 こんにちは 你好 (Hangul / Hiragana / Hanzi)"
echo "RTL mix:       Latin شَلوم العَرَبية עִברית (Arabic/Hebrew — BiDi if supported)"
# Zero-width space and joiner — surrounding text should look unbroken
printf 'Zero-width:    a\xe2\x80\x8bb\xe2\x80\x8cc (should look like "abc", no gap)\n'
printf 'Wide+combo:    ㅎ\xcc\x81 ㅏ\xcc\x82 (combining mark on Korean jamo)\n'
echo "VT100 box:"
printf "\e(0lqqqqqqqqkx        xtqqqqqqqquxx        xmqqqqqqqqj\e(B\n"
echo
echo "Visual check complete."
