# mac_encodings

Generates `crates/lib/src/fs/hfs/encoding/tables.rs`, the tables behind the single-byte Mac OS encodings (`disk_atlas_lib::fs::hfs::encoding`): Roman, Central European, Cyrillic, Greek, Turkish, Croatian, Icelandic and Romanian. The output is checked in. This crate is std-only and is not part of the repository workspace.

## Inputs

The eight mapping files are checked in beside this README, unmodified, with their digests in `SHA256SUMS` (`sha256sum` format). The generator refuses any file whose SHA-256 differs from `SHA256SUMS`.

| File | Encoding | SHA-256 |
|---|---|---|
| `ROMAN.TXT` | Mac OS Roman | `18e571645be895e9553ed5c842ea8f65f9c5d3c9ccb43e66e0c33a132ed0d721` |
| `CENTEURO.TXT` | Mac OS Central European | `5f2e262a66a7d08317555835dcc8445f4666928f13f84a19951b19b42fe0b623` |
| `CYRILLIC.TXT` | Mac OS Cyrillic | `092ca7d0fe584b3fde176af4f2a175ceeba07b4c1a1c0b472c2016a6c92eafcc` |
| `GREEK.TXT` | Mac OS Greek | `d57b3e0644a54f33396d53f3ca63a8483e96526de0d62be1dcd6eb40399a398e` |
| `TURKISH.TXT` | Mac OS Turkish | `afce027def8db108de0607a76f1aae00787a4b2c7a44f68a359d9053c50a30bf` |
| `CROATIAN.TXT` | Mac OS Croatian | `cd3b6a79271664df7e4a935766906961bcfc6bf85cc8b9c877381a22044cf84e` |
| `ICELAND.TXT` | Mac OS Icelandic | `39b8d242bd0995a3b9f205273f098c1ff9bab4505a213c4e0313a700e47f26ce` |
| `ROMANIAN.TXT` | Mac OS Romanian | `a05436e00507e757e8badb5d582985b3c9d765b07609c2aeae9033dcfa02f43a` |

Source: https://www.unicode.org/Public/MAPPINGS/VENDORS/APPLE/ (downloaded 2026-11-15). The files are Apple's published mapping tables, hosted by Unicode.org, and may be used freely under the Unicode Terms of Use: https://www.unicode.org/copyright.html (the data files licence: https://www.unicode.org/license.txt). Their headers carry Apple's own notice and disclaimer; keep them with the files.

To refresh, download the files again, run `sha256sum *.TXT > SHA256SUMS`, review the diff of the generated tables, and update the digests above.

## Regenerate

From the repository root:

```
cargo run --manifest-path tools/mac_encodings/Cargo.toml
```

To verify the checked-in tables without writing anything:

```
cargo run --manifest-path tools/mac_encodings/Cargo.toml -- --check
```

Exit status: 0 up to date, 1 different, 2 error (including a digest mismatch). `--dir PATH` and `--out PATH` override the defaults (this directory, and the checked-in `tables.rs`).

## What it generates

Per encoding, `<NAME>_DECODE: [char; 128]` for bytes 0x80-0xFF and `<NAME>_ENCODE: &[(char, u8)]`, sorted by character for binary search, holding the same 128 mappings minus any U+FFFD. Bytes 0x00-0x7F are ASCII in all eight and are not stored.

## What it checks

- Every file maps 0x20-0x7E and 0x80-0xFF. The files leave out the control bytes 0x00-0x1F and 0x7F (the standard control characters); the generator treats them as identity. Any of 0x00-0x7F mapped to something other than itself fails the run.
- No two bytes of one encoding decode to the same character, so the encoder is a true inverse.
- A byte the file leaves undefined (or lists without a value) becomes U+FFFD, which never encodes.
- A mapping given as a sequence (`0xAAAA+0xBBBB`) must be in the generator's `SEQUENCES` list, which names the precomposed character to use. The eight files contain none, so the list is empty. Direction hints (`<LR>`, `<RL>`) fail the run.
- The Apple logo (U+F8FF, private use) is kept as the files give it.

## Notes on the data

- `TURKISH.TXT` assigns 0xF5 to U+F8A0 with the comment `undefined1`: Apple's placeholder for the one code point Mac OS Turkish leaves undefined. The generator lists it in `UNDEFINED_PLACEHOLDERS` and maps 0xF5 to U+FFFD.
- `GREEK.TXT` maps 0xFF to U+00AD (soft hyphen); before Mac OS 9.2.2 it was undefined.
- `ROMAN.TXT` maps 0xDB to U+20AC (euro), the Mac OS 8.5 and later mapping, not the older currency sign. `CYRILLIC.TXT` maps 0xFF to U+20AC.
- Roman, Turkish, Croatian, Icelandic and Romanian map 0xBD to U+03A9 (Greek capital omega), not U+2126 (ohm).
