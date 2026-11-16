# Patches to xattr 0.2.3

Copied from crates.io (MIT OR Apache-2.0). It is used by gpui's `zed-async-tar`,
which asks for `xattr = "0.2"`, so a newer xattr is not an option.

1. `src/sys/mod.rs`: `ENOATTR` is `libc::ENODATA` on Linux and Android.
   libc 0.2.191 does not export `ENOATTR` there, which broke the Ubuntu CI job
   (`cannot find value ENOATTR in crate ::libc`).

Remove this folder and the `[patch.crates-io]` entry when gpui moves to xattr 1.x.
