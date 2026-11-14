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
# Writes a manifest of a mounted volume for atlas_check to compare against.
#
#   tools/conformance/manifest.sh [--no-data] /Volumes/Name > name.manifest
#
# macOS only (find -x, stat -f, cksum, readlink). Needs bash 3.2 or newer.
# It reads every file to checksum it, so it is slow on a big volume; with
# --no-data the checksum columns are "-" and nothing is read. It stays on
# the one volume (find -x), follows no symlinks, and skips the folders in
# SKIP below, which exist only at the volume root.
#
# Format (tab separated, UTF-8):
#   line 1:  # disk-atlas manifest v1<TAB><volume name>
#   then one line per entry, 11 columns:
#     kind          d (folder), f (file) or l (symlink)
#     path          relative to the volume root, "/" separated
#     size          data fork bytes; "-" for folders
#     rsrc          resource fork bytes (0 if none); "-" for folders
#     mode          octal permission bits with setuid, setgid and sticky
#     uid, gid      numeric owner and group
#     mtime         content modification time, Unix seconds
#     cksum crc     CRC from cksum(1) of the data fork; "-" except for files
#     cksum length  byte count from cksum(1); "-" except for files
#     target        symlink target; "-" except for symlinks
#   In path and target, % tab LF and CR are written %25 %09 %0A %0D.

# Folders at the volume root that are left out.
SKIP=".Spotlight-V100 .fseventsd .Trashes .DocumentRevisions-V100 .TemporaryItems"

usage() {
    echo "usage: $0 [--no-data] /Volumes/Name > name.manifest" >&2
    exit 1
}

DATA=1
ROOT=""
for arg in "$@"; do
    case "$arg" in
        --no-data) DATA=0 ;;
        -h|--help) usage ;;
        -*) usage ;;
        *) [ -z "$ROOT" ] || usage; ROOT="$arg" ;;
    esac
done
[ -n "$ROOT" ] || usage
[ -d "$ROOT" ] || { echo "$0: $ROOT is not a folder" >&2; exit 1; }

# Percent-encodes the four characters the format reserves.
enc() {
    local s="$1"
    s=${s//%/%25}
    s=${s//$'\t'/%09}
    s=${s//$'\n'/%0A}
    s=${s//$'\r'/%0D}
    ENC="$s"
}

# The volume's own name, which can differ from the mount point's name.
NAME=$(diskutil info "$ROOT" 2>/dev/null | sed -n 's/^ *Volume Name: *//p' | head -n 1)
if [ -z "$NAME" ]; then
    NAME=$(basename "$ROOT")
fi
enc "$NAME"
printf '# disk-atlas manifest v1\t%s\n' "$ENC"

# Work from inside the volume so paths are relative and the prune patterns
# hold no characters from the mount point.
cd "$ROOT" || exit 1

PRUNE=()
for name in $SKIP; do
    if [ ${#PRUNE[@]} -gt 0 ]; then
        PRUNE+=(-o)
    fi
    PRUNE+=(-path "./$name")
done

find -x . \( "${PRUNE[@]}" \) -prune -o -print0 | while IFS= read -r -d '' p; do
    [ "$p" = "." ] && continue
    rel=${p#./}
    if [ -L "$p" ]; then
        kind=l
    elif [ -d "$p" ]; then
        kind=d
    elif [ -f "$p" ]; then
        kind=f
    else
        echo "skipping $rel (not a file, folder or symlink)" >&2
        continue
    fi
    # stat does not follow symlinks by default, so a link reports itself.
    if ! meta=$(stat -f $'%z\t%Lp\t%u\t%g\t%m' "$p" 2>/dev/null); then
        echo "cannot stat $rel" >&2
        continue
    fi
    IFS=$'\t' read -r size mode uid gid mtime <<EOF
$meta
EOF
    rsrc=0
    crc=-
    cklen=-
    target=-
    case "$kind" in
        d)
            size=-
            rsrc=-
            ;;
        f)
            rsrc=$(stat -f %z "$p/..namedfork/rsrc" 2>/dev/null || echo 0)
            if [ "$DATA" = 1 ]; then
                if sum=$(cksum < "$p" 2>/dev/null); then
                    crc=${sum%% *}
                    cklen=${sum##* }
                else
                    echo "cannot read $rel" >&2
                fi
            fi
            ;;
        l)
            # Keep a trailing newline in the target: add a character, then remove it.
            target=$(readlink "$p"; printf x)
            target=${target%x}
            target=${target%$'\n'}
            enc "$target"
            target="$ENC"
            ;;
    esac
    enc "$rel"
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
        "$kind" "$ENC" "$size" "$rsrc" "$mode" "$uid" "$gid" "$mtime" "$crc" "$cklen" "$target"
done
