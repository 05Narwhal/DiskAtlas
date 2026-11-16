#!/bin/bash
# Disk Atlas - A cross-platform filesystem explorer.
#
# Copyright (C) 2026 Julian A. Loy.
#
# This program is free software: you can redistribute it and/or modify
# it under the terms of the GNU General Public License as published by
# the Free Software Foundation, either version 3 of the License, or
# (at your option) any later version.
#
# This program is distributed in the hope that it will be useful,
# but WITHOUT ANY WARRANTY; without even the implied warranty of
# MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.
#
# See the LICENSE file for the full license text.
#
# Builds the test tree that linux-images.sh copies into each image.
#
#   tools/conformance/make-tree.sh <folder> [--links] [--small]
#
# The names avoid the characters FAT, exFAT, HFS+ and Windows reserve
# (: * ? " < > | \), so one tree fits every format. --links adds symlinks,
# for the formats that have them (ext, UDF, ISO 9660 with Rock Ridge).
# File times are fixed and even-numbered seconds after 1980, so FAT's
# two-second clock round-trips them. --small leaves out the files over 1 MiB,
# for FAT12 and other tiny volumes.

set -eu
# FAT and exFAT store local time with no zone; UTC keeps every format equal.
export TZ=UTC

DEST="${1:?usage: make-tree.sh <folder> [--links]}"
LINKS=0
SMALL=0
for flag in "${@:2}"; do
    case "$flag" in
        --links) LINKS=1 ;;
        --small) SMALL=1 ;;
        *) echo "unknown flag $flag" >&2; exit 1 ;;
    esac
done

rm -rf "$DEST"
mkdir -p "$DEST"
cd "$DEST"

# Deterministic data: a fixed seed, so a failure reproduces.
data() { # data <bytes> <seed> <file>
    openssl enc -aes-128-ctr -K 00112233445566778899aabbccddeeff \
        -iv "$(printf '%032x' "$2")" -nosalt < /dev/zero 2>/dev/null | head -c "$1" > "$3"
}

mkdir -p docs/deep/er/and/deeper/still/more/leaf empty-folder "folder with spaces" \
    ".hidden-folder" "Unicode folder"
: > empty.bin
printf 'x' > one-byte.bin
data 511 1 size-511.bin
data 512 2 size-512.bin
data 513 3 size-513.bin
data 4095 4 size-4095.bin
data 4096 5 size-4096.bin
data 4097 6 size-4097.bin
data 65536 7 size-64k.bin
if [ "$SMALL" = 0 ]; then
    data 1048576 8 size-1m.bin
    data 5242889 9 docs/size-5m-odd.bin
    data 33554432 10 big-32m.bin
    head -c 3145728 /dev/zero > zeros-3m.bin
fi
printf 'hello\n' > docs/readme.txt
printf 'hello\n' > "folder with spaces/file with spaces.txt"
printf 'dot\n' > .hidden-file
printf 'dot\n' > .hidden-folder/inside.txt
printf 'accent\n' > "Unicode folder/café.txt"
printf 'cjk\n' > "Unicode folder/日本語のファイル.txt"
printf 'cyrillic\n' > "Unicode folder/Привет.txt"
printf 'greek\n' > "Unicode folder/ελληνικά.txt"
printf 'percent\n' > "100% done.txt"
printf 'many\n' > "a, b; c & d (e) [f] {g} #h @i !j ~k ^l +m =n.txt"
printf 'case\n' > Case.txt
printf 'case\n' > docs/CASE.TXT
long=$(printf 'n%.0s' $(seq 1 200))
printf 'long\n' > "$long.txt"
data 1000 11 docs/deep/er/and/deeper/still/more/leaf/payload.bin
# A folder with enough entries to need several directory blocks and nodes.
mkdir -p many
for i in $(seq 1 600); do
    printf '%04d\n' "$i" > "many/entry-$(printf '%04d' "$i")-with-a-longer-name.txt"
done

if [ "$LINKS" = 1 ]; then
    ln -s docs/readme.txt link-to-file
    ln -s docs link-to-folder
    ln -s does-not-exist link-dangling
    ln -s "$long.txt" link-to-long
fi

# Fixed even-second times, a spread of years. Files first, folders last, so
# folder times are not disturbed by the files inside them.
n=0
find . -mindepth 1 \( -type f -o -type l \) -print0 | sort -z | while IFS= read -r -d '' p; do
    n=$((n + 1))
    year=$((1990 + n % 30))
    touch -h -d "$year-0$((1 + n % 9))-1$((n % 9)) 1$((n % 10)):2$((n % 10)):$(printf '%02d' $(((n * 2) % 60)))" "$p"
done
find . -mindepth 1 -type d -depth -print0 | sort -z | while IFS= read -r -d '' p; do
    touch -d "2001-02-03 04:05:06" "$p"
done
touch -d "2001-02-03 04:05:06" .
