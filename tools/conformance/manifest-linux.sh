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
# The Linux twin of manifest.sh: writes a disk-atlas manifest of a folder
# (the source tree an image was built from) for atlas_check to compare.
#
#   tools/conformance/manifest-linux.sh [--blank mode,uid,gid,mtime] <folder> > x.manifest
#
# --blank writes "-" in those columns, for formats that do not store them
# (FAT has no owners or modes, ISO 9660 and UDF may not). GNU find and stat.
# The format is the one in manifest.sh.

set -eu

BLANK=""
if [ "${1:-}" = "--blank" ]; then
    BLANK=",${2},"
    shift 2
fi
ROOT="${1:?usage: manifest-linux.sh [--blank cols] <folder>}"
[ -d "$ROOT" ] || { echo "$0: $ROOT is not a folder" >&2; exit 1; }

enc() {
    local s="$1"
    s=${s//%/%25}
    s=${s//$'\t'/%09}
    s=${s//$'\n'/%0A}
    s=${s//$'\r'/%0D}
    ENC="$s"
}
col() { # col <name> <value>
    case "$BLANK" in *",$1,"*) echo "-" ;; *) echo "$2" ;; esac
}

enc "$(basename "$ROOT")"
printf '# disk-atlas manifest v1\t%s\n' "$ENC"

cd "$ROOT"
find . -mindepth 1 -print0 | sort -z | while IFS= read -r -d '' p; do
    rel=${p#./}
    if [ -L "$p" ]; then kind=l
    elif [ -d "$p" ]; then kind=d
    elif [ -f "$p" ]; then kind=f
    else echo "skipping $rel" >&2; continue; fi
    IFS=$'\t' read -r size mode uid gid mtime <<EOF
$(stat -c $'%s\t%a\t%u\t%g\t%Y' "$p")
EOF
    rsrc=0 crc=- cklen=- target=-
    case "$kind" in
        d) size=-; rsrc=- ;;
        f)
            sum=$(cksum < "$p")
            crc=${sum%% *}
            cklen=${sum##* }
            ;;
        l)
            target=$(readlink "$p"; printf x)
            target=${target%x}
            target=${target%$'\n'}
            enc "$target"
            target="$ENC"
            ;;
    esac
    enc "$rel"
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
        "$kind" "$ENC" "$size" "$rsrc" \
        "$(col mode "$mode")" "$(col uid "$uid")" "$(col gid "$gid")" "$(col mtime "$mtime")" \
        "$crc" "$cklen" "$target"
done
