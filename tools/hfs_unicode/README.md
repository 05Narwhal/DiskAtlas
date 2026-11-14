# hfs_unicode

Generates `crates/lib/src/fs/hfsplus/unicode/tables.rs`, the tables behind HFS+ name comparison, decomposition and composition. The output is checked in. This crate is std-only and is not part of the repository workspace.

## Inputs

| Input | Source | SHA-256 |
|---|---|---|
| `UnicodeData-3.2.0.txt` | https://www.unicode.org/Public/3.2-Update/UnicodeData-3.2.0.txt | `5e444028b6e76d96f9dc509609c5e3222bf609056f35e5fcde7e6fb8a58cd446` |
| `CompositionExclusions-3.2.0.txt` | https://www.unicode.org/Public/3.2-Update/CompositionExclusions-3.2.0.txt | `1d3a450d0f39902710df4972ac4a60ec31fbcb54ffd4d53cd812fc1200c732cb` |
| `tn1150_lower_case.txt` (checked in) | TN1150 `gLowerCaseTable`, see below | values: `01d87d06619efc3ec97f28daa8d93ef63b6335d9a131ff372a8881e1f7a4ff6c` |

The two Unicode files are not checked in. The generator refuses either one unless its SHA-256 matches. For the lower case table it hashes the 2816 parsed values (as big-endian `u16`s), so comments and line endings in the text file may change but the values may not.

### Where the lower case table comes from

TN1150, "HFS Plus Volume Format" (https://developer.apple.com/library/archive/technotes/tn/tn1150.html), prints `FastUnicodeCompare` but omits the table from the listing ("Full data tables omitted for brevity") and points to its Downloadables section. That links one file:

| File | SHA-256 |
|---|---|
| https://developer.apple.com/library/archive/technotes/tn/downloads/tn1150.1.hqx | `c1ed4db42b31458221e6d77cea8237a151111e3ca625231348e76e75105f3929` |
| `FastUnicodeCompare.c` inside it (31537 bytes, CR line endings) | `fa6c4908fe8b3cc4d1bf3ca9b1e9e5543d52ef61dde5c8eff3ae1231f3744432` |
| `tn1150.html` as fetched (for reference) | `fd1ca22de7ec38c044bcc726b5bc14b5b0ee63f1b242039f8bc4f3728e247cb5` |

`tn1150.1.hqx` is BinHex 4.0 around a StuffIt 5 archive, `FastUnicodeCompare.c.sit`, whose single entry is Arsenic-compressed. The BinHex header and data fork CRCs and the Arsenic stream's CRC-32 all matched on extraction. `tn1150_lower_case.txt` is the body of `UInt16 gLowerCaseTable[] = { ... };` from that file, copied verbatim with CR converted to LF. To re-verify, unpack the `.hqx` with any StuffIt-capable tool (for example `unar tn1150.1.hqx`) and compare the table.

## Regenerate

Download the two Unicode files into a directory of your choice, then from the repository root:

```
curl -fLO https://www.unicode.org/Public/3.2-Update/UnicodeData-3.2.0.txt
curl -fLO https://www.unicode.org/Public/3.2-Update/CompositionExclusions-3.2.0.txt
cargo run --manifest-path tools/hfs_unicode/Cargo.toml -- UnicodeData-3.2.0.txt CompositionExclusions-3.2.0.txt
```

To verify the checked-in tables without writing anything:

```
cargo run --manifest-path tools/hfs_unicode/Cargo.toml -- --check UnicodeData-3.2.0.txt CompositionExclusions-3.2.0.txt
```

Exit status: 0 up to date, 1 different, 2 error (including a digest mismatch). `--lower-case PATH` and `--out PATH` override the defaults (`tn1150_lower_case.txt` here, and the checked-in `tables.rs`).

## What it generates

- `LOWER_CASE`: `gLowerCaseTable` as is (two-level: a 256-entry high-byte index, then ten 256-entry pages).
- `DECOMP_KEYS`, `DECOMP_INDEX`, `DECOMP_DATA`: the full canonical decomposition of every code point that has one in Unicode 3.2 (Hangul excepted, it is algorithmic), canonically ordered, as UTF-16. The engine applies Apple's exclusions (U+2000-U+2FFF, U+F900-U+FAFF, and everything outside the BMP) at lookup time, for both lookup and display, and never composes into an excluded code point; the generator fails if any other decomposition passes through an excluded code point, which would make that shortcut wrong.
- `COMBINING_CLASS`: non-zero canonical combining classes as ranges.
- `COMPOSE`: primary composites for NFC, excluding the explicit composition exclusions, singletons and non-starter decompositions.
