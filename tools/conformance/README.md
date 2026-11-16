# Conformance pass

Disk Atlas reads HFS+ and APFS volumes and disk images from bytes alone. The unit tests
use volumes and images that our own code wrote, so they cannot catch a
misreading shared by writer and reader. This folder holds what is needed to
check the engine against real macOS: a script that lists a mounted volume as
macOS sees it, and a checker program that lists the same volume as Disk Atlas
sees it and compares the two.

Nothing here writes to a disk or an image. The checker opens everything
read-only. The script only reads.

## 1. On the Mac

### A manifest per volume

Mount the volume read-only, so macOS does not replay a journal or touch the
disk, and write its manifest:

```
hdiutil attach -readonly -nobrowse image.dmg      # for an image; skip for a physical disk
tools/conformance/manifest.sh /Volumes/Name > name.manifest
```

For a physical disk that is already mounted, unmount it and mount it again
read-only: `diskutil unmount /Volumes/Name` and
`diskutil mount readOnly /dev/diskNsM`.

The script needs bash 3.2 or newer (the one macOS ships) and macOS's own
`find`, `stat`, `cksum` and `readlink`. It stays on the one volume, follows no
symlinks, and leaves out `/.Spotlight-V100`, `/.fseventsd`, `/.Trashes`,
`/.DocumentRevisions-V100` and `/.TemporaryItems` (the list is at the top of
the script).

**It reads every file** to checksum it, so it is slow on a big volume. With
`--no-data` it skips the checksums and reads nothing:
`tools/conformance/manifest.sh --no-data /Volumes/Name > name.manifest`.
The comparison then covers names, kinds, sizes, owners, modes, dates and link
targets, but not file content.

The manifest is a text file. The first line names the volume, then one line
per entry with 11 tab-separated columns (kind, path, data size, resource fork
size, mode, uid, gid, mtime, cksum CRC, cksum length, link target); the format
is written out in the script's header. Paths and targets use `%25`, `%09`,
`%0A` and `%0D` for `%`, tab, line feed and carriage return.

### Test images

Make images from a folder of real content (a copy of a project, some photos,
a file with a resource fork, a hard link, a symlink, a few names with accents
or emoji). Add `-fs HFS+` (or `-fs "Journaled HFS+"`, or `-fs HFSX`): without
it recent macOS makes APFS images (also worth checking, see below). Add
`-layout NONE`, `-layout SPUD` (Apple Partition Map) or `-layout GPTSPUD`
(GPT) to choose the partition map; the default is GPT.

```
SRC=~/Desktop/sample-folder

hdiutil create -srcfolder "$SRC" -fs "Journaled HFS+" -volname Sample -format UDZO sample-udzo.dmg   # zlib
hdiutil create -srcfolder "$SRC" -fs "Journaled HFS+" -volname Sample -format UDBZ sample-udbz.dmg   # bzip2
hdiutil create -srcfolder "$SRC" -fs "Journaled HFS+" -volname Sample -format ULFO sample-ulfo.dmg   # lzfse
hdiutil create -srcfolder "$SRC" -fs "Journaled HFS+" -volname Sample -format ULMO sample-ulmo.dmg   # lzma
hdiutil create -srcfolder "$SRC" -fs "Journaled HFS+" -volname Sample -format UDRO sample-udro.dmg   # read-only, uncompressed
hdiutil create -srcfolder "$SRC" -fs "Journaled HFS+" -volname Sample -type SPARSE sample.sparseimage
hdiutil create -srcfolder "$SRC" -fs "Journaled HFS+" -volname Sample -type SPARSEBUNDLE sample.sparsebundle
```

(`-format UDSP` and `-format UDSB` are the same sparse formats as
`-type SPARSE` and `-type SPARSEBUNDLE`.) Other ways to get the same content
in a different container:

```
hdiutil convert sample-udzo.dmg -format UDBZ -o sample-converted-udbz.dmg
hdiutil convert sample.sparseimage -format UDRO -o sample-from-sparse.dmg
hdiutil convert sample-udro.dmg -format UDSB -o sample-converted.sparsebundle
```

Then take a manifest of the content once, from any one of them (they hold the
same files):

```
hdiutil attach -readonly -nobrowse sample-udzo.dmg
tools/conformance/manifest.sh /Volumes/Sample > sample.manifest
hdiutil detach /Volumes/Sample
```

If a volume has been used, not just built, so much the better: a startup
disk, an old Time Machine drive or a Mac OS Extended USB stick exercise
journals, hard links, compression and long names that a new image does not.

## 2. On the Windows machine

Build once, then run the checker on each image, folder or disk:

```
cargo run --release -p disk-atlas-lib --example atlas_check -- <path> [--manifest FILE] [--max-entries N] [--read-data]
```

`<path>` is a disk image file, a sparse bundle folder, or a device
(`\\.\PhysicalDrive1` on Windows from an Administrator shell; `/dev/rdisk3`
and the like elsewhere). Everything is opened read-only. For each image the
checker prints the image format, the disk size, the partition map and each
partition with its probed content, and for every HFS+ volume the mount
result, the volume name, the journal state, a walk of the whole tree with
counts and errors, and timings. For an APFS container it prints every volume
with its role and state (ready, locked or failed), then for each ready volume
its snapshots (each one opened and its root listed, with timings), a timed
`scan`, and the same walk.

- `--read-data` reads the data of every file: it adds a throughput line and
  is needed for checksum comparison.
- `--manifest FILE` compares the walk with a manifest, for the volume whose
  name matches the manifest's first line (or the only volume).
- `--max-entries N` stops each volume's walk after N entries, for a first
  look at a very large disk.

For example:

```
atlas_check sample-udzo.dmg --manifest sample.manifest --read-data > report-udzo.txt
atlas_check sample.sparsebundle --manifest sample.manifest --read-data > report-sparsebundle.txt
atlas_check \\.\PhysicalDrive2 --manifest usbstick.manifest --read-data > report-usb.txt
```

The exit code is 0 when the run found nothing to report, 2 when there were
walk errors or differences, and 1 when the checker could not start or open
the path.

### What the comparison reports

- **Only in the manifest / only in Disk Atlas**: entries one side lists and
  the other does not. Both lists are failures, except for the cases below.
- **Mismatches**, one list per field: kind, size, resource fork size, mode
  (permission bits with setuid, setgid and sticky), uid, gid, mtime, link
  target, and with `--read-data` the data checksum. The first 200 of each list
  are printed, with the total.
- **Expected hidden**: Disk Atlas does not list the two folders HFS+ keeps for
  hard links (`<NUL><NUL><NUL><NUL>HFS+ Private Data` and
  `.HFS+ Private Directory Data<CR>`), or anything inside them, and macOS
  shows them. Manifest entries under those names, and `.journal` and
  `.journal_info_block` at the root (macOS hides them), are listed separately
  as expected and are not failures.
- **Expected extra**: entries Disk Atlas lists that the script skipped on
  purpose (the five folders above), and the journal files and `.hotfiles.btree`
  at the root.

### Names

macOS lists HFS+ names as stored, in decomposed form (`e` followed by a
combining accent, not the single character). Disk Atlas shows names composed.
The checker compares each path component after running both sides through the
engine's own decomposition (the one it uses for lookups: Unicode 3.2
canonical decomposition with Apple's excluded ranges), so a composed name and
its decomposed form match. Two names that differ in some other way, for
example one the engine stored in an older decomposition (the notes in
`.claude/docs/filesystems.md` explain this), still show up as one entry only
in the manifest and one only in Disk Atlas. That is a finding to report, not
noise to ignore.

### Known gaps in the comparison

- Case-insensitive volumes: paths are compared exactly as written, so if macOS
  and Disk Atlas disagree about capitalisation, both entries are reported.
- The script skips the five folders at the root by name, at the root only.
- The size of a compressed file's resource fork: Disk Atlas reports 0 for a
  transparently compressed file (its fork holds the compressed content, not a
  user fork). `stat` on the Mac normally hides the compression fork as well; if
  it does not, every compressed file shows as a resource fork size mismatch,
  which is worth reporting once rather than per file.
- Extended attributes and Finder flags are not in the manifest.

## 3. What to send back

- The checker's full output for each run (the `report-*.txt` files), as they
  are.
- The manifest for any run that reported differences, and the `hdiutil` or
  `diskutil` command that made the image.
- For a disk, the output of `diskutil list` and `diskutil info /dev/diskN`
  for it, and the macOS version that last wrote to it.
- Anything that crashed, hung, or took much longer than the disk's size
  suggests, with the command line.
