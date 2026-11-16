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
# Builds real filesystem images with the standard Linux tools, fills each
# with a test tree, and checks Disk Atlas's reading of it against a manifest
# of that tree (atlas_check --manifest --read-data). The images are made by
# other people's code, so a misreading shared by our writer and our reader
# cannot hide here.
#
#   sudo tools/conformance/linux-images.sh <outdir> <atlas_check> [formats...]
#
# <atlas_check> is the built example: a Linux binary, or a Windows .exe when
# run from WSL (paths are converted with wslpath). Needs root for the loop
# mounts (exFAT, UDF). Tools: mke2fs, mkfs.vfat, mtools, mkfs.exfat and
# exfat-fuse, genisoimage, mkudffs, mkfs.hfsplus, openssl, GNU coreutils.
# No Mac is needed; HFS+ and classic HFS are only checked on an empty volume
# here, because no Linux tool in WSL can fill one (hpmount rejects mkfs
# output and the kernel module is absent). APFS and classic HFS need a Mac.
#
# Formats: ext2 ext3 ext4 fat12 fat16 fat32 exfat iso udf hfsplus

set -u
export TZ=UTC

OUT="${1:?usage: linux-images.sh <outdir> <atlas_check> [formats...]}"
CHECK="${2:?usage: linux-images.sh <outdir> <atlas_check> [formats...]}"
shift 2
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ALL="ext2 ext3 ext4 fat12 fat16 fat32 exfat iso udf hfsplus"
FORMATS="${*:-$ALL}"

mkdir -p "$OUT"
OUT="$(cd "$OUT" && pwd)"

TREE="$OUT/tree"
TREE_LINKS="$OUT/tree-links"
TREE_SMALL="$OUT/tree-small"
[ -d "$TREE" ] || "$HERE/make-tree.sh" "$TREE"
[ -d "$TREE_LINKS" ] || "$HERE/make-tree.sh" "$TREE_LINKS" --links
[ -d "$TREE_SMALL" ] || "$HERE/make-tree.sh" "$TREE_SMALL" --small

# The checker may be a Windows program reached through WSL interop.
case "$CHECK" in
    *.exe) winpath() { wslpath -w "$1"; } ;;
    *) winpath() { echo "$1"; } ;;
esac

blank() { dd if=/dev/zero of="$1" bs=1M count="$2" status=none; }

# make_<fmt> <image>: creates and fills the image; sets TREE_USED and BLANK.
make_ext() { # make_ext <image> <type> <mb> <extra mke2fs options>
    local img="$1" type="$2" mb="$3"
    shift 3
    TREE_USED="$TREE_LINKS"
    BLANK=""
    blank "$img" "$mb"
    mke2fs -q -t "$type" -L "atlas-$type" -d "$TREE_USED" "$@" "$img"
}
make_ext2() { make_ext "$1" ext2 96 -b 1024; }
make_ext3() { make_ext "$1" ext3 96 -b 2048; }
make_ext4() { make_ext "$1" ext4 96 -b 4096; }

make_fat() { # make_fat <image> <bits> <mb> <tree>
    local img="$1" bits="$2" mb="$3"
    TREE_USED="$4"
    BLANK="mode,uid,gid"
    blank "$img" "$mb"
    mkfs.vfat -F "$bits" -n ATLASFAT "$img" >/dev/null
    (cd "$TREE_USED" && mcopy -s -p -m -Q -i "$img" ./* ./.hidden-file ./.hidden-folder :: )
}
make_fat12() { make_fat "$1" 12 16 "$TREE_SMALL"; }
make_fat16() { make_fat "$1" 16 96 "$TREE"; }
make_fat32() { make_fat "$1" 32 96 "$TREE"; }

mount_fill() { # mount_fill <image> <mount options...>: copy the tree in, unmount
    local img="$1" mnt
    shift
    mnt="$(mktemp -d)"
    if mount "$@" -o loop "$img" "$mnt"; then
        cp -a "$TREE_USED/." "$mnt/"
        sync
        umount "$mnt"
    else
        echo "mount failed for $img" >&2
        rmdir "$mnt"
        return 1
    fi
    rmdir "$mnt" 2>/dev/null
}
make_exfat() {
    local img="$1" mnt
    TREE_USED="$TREE"
    BLANK="mode,uid,gid"
    blank "$img" 96
    mkfs.exfat -L ATLASEXFAT "$img" >/dev/null || return 1
    mnt="$(mktemp -d)"
    # No kernel exFAT in WSL: the FUSE driver, on the loop device.
    local loop
    loop="$(losetup -f --show "$img")" || return 1
    if mount.exfat-fuse "$loop" "$mnt"; then
        cp -a --no-preserve=ownership "$TREE_USED/." "$mnt/"
        sync
        umount "$mnt"
    else
        echo "exfat-fuse mount failed" >&2
    fi
    losetup -d "$loop"
    rmdir "$mnt" 2>/dev/null
}
make_udf() {
    local img="$1"
    TREE_USED="$TREE_LINKS"
    BLANK=""
    blank "$img" 96
    mkudffs --label=ATLASUDF "$img" >/dev/null || return 1
    mount_fill "$img" -t udf
}
make_iso() {
    local img="$1"
    TREE_USED="$TREE_LINKS"
    # Rock Ridge keeps owners, modes and links; Joliet gives the long Unicode names.
    BLANK=""
    genisoimage -quiet -R -J -joliet-long -V ATLASISO -o "$img" "$TREE_USED"
}
make_hfsplus() {
    local img="$1"
    TREE_USED="$OUT/tree-empty"
    mkdir -p "$TREE_USED"
    BLANK=""
    blank "$img" 64
    mkfs.hfsplus -v ATLASHFS "$img" >/dev/null
}

SUMMARY="$OUT/summary.txt"
: > "$SUMMARY"
for fmt in $FORMATS; do
    img="$OUT/$fmt.img"
    log="$OUT/$fmt.log"
    manifest="$OUT/$fmt.manifest"
    rm -f "$img"
    echo "== $fmt"
    if ! "make_$fmt" "$img"; then
        echo "$fmt: IMAGE BUILD FAILED" | tee -a "$SUMMARY"
        continue
    fi
    if [ -n "$BLANK" ]; then
        "$HERE/manifest-linux.sh" --blank "$BLANK" "$TREE_USED" > "$manifest"
    else
        "$HERE/manifest-linux.sh" "$TREE_USED" > "$manifest"
    fi
    "$CHECK" "$(winpath "$img")" --manifest "$(winpath "$manifest")" --read-data > "$log" 2>&1
    rc=$?
    echo "$fmt: exit $rc ($(wc -l < "$manifest") manifest lines) log $log" | tee -a "$SUMMARY"
done
echo
cat "$SUMMARY"
