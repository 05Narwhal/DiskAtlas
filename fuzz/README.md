# Fuzz targets

cargo-fuzz targets for the engine's raw-byte parsers. Needs a nightly
toolchain and `cargo install cargo-fuzz`; this crate is not in the main
workspace.

```
# seed inputs built from the HFS+ test volume builder
DISK_ATLAS_FUZZ_SEEDS=$PWD/fuzz/seeds \
    cargo test -p disk-atlas-lib --lib write_fuzz_seeds -- --ignored

mkdir -p fuzz/corpus/hfsplus_mount
cargo +nightly fuzz run hfsplus_mount fuzz/corpus/hfsplus_mount fuzz/seeds/hfsplus_mount \
    -- -max_total_time=60 -max_len=1048576
```

Targets: `probe`, `partition_map`, `hfsplus_volume_header`, `hfsplus_mount`.
Pass `-max_len` above the 4096 default or the HFS+ seeds are truncated.
