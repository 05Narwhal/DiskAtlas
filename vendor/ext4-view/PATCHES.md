# Patches to ext4-view 1.0.0

Copied from crates.io (MIT OR Apache-2.0, Google LLC). Changes, each marked
`DISK ATLAS PATCH` in the source:

1. `src/iters/file_blocks/block_map.rs`, `IndirectBlockIter::new`: a block
   index of zero is a hole. Upstream read block 0 of the volume as if it were
   an indirect block, so a sparse ext2/ext3 file with a hole spanning a whole
   indirect block failed with `Corrupt`. Found with a real `mke2fs` image whose
   3 MiB all-zero file `mke2fs -d` stored sparse.

Remove this folder and the `[patch.crates-io]` entry when a release has the fix.
