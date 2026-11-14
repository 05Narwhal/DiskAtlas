# Provider protocol, version 1

The byte-level contract between Disk Atlas and a separate process: a filesystem extension (M8 tasks 11–13) or the elevated device helper (M8 task 14). `m8-formats-plugins.md` §7 says why it is processes and pipes; this file says exactly what crosses the pipe. Written by Fable on 2026-11-12. It becomes a public interface the day the first extension ships: move it to `docs/provider-protocol.md` at the repository root then, and from then on change it only by adding a version.

A number in this file is a bound to enforce, not a suggestion. Both sides treat the other as hostile: every length, count, tag and handle is checked before use, exactly as on-disk bytes are.

## 1. Transport

Two byte streams, one in each direction, carrying frames back to back with nothing between them.

| Peer | Streams |
|---|---|
| Filesystem extension | The child's standard input (host writes) and standard output (host reads). Standard error is free text: the host reads it by line and logs each line at debug level with the extension's name, at most 4096 bytes a line and 100 lines a second (the rest is dropped and counted). |
| Device helper | Windows: one duplex named pipe. Linux: the helper's standard input and output. macOS: one Unix domain socket. See §8. |

An extension must not write anything but frames to standard output. A byte that does not parse as a frame ends the session.

## 2. Frame

```
offset size
0      4    len     u32 LE  bytes that follow this field: 8 ..= MAX_FRAME
4      4    id      u32 LE  request id
8      2    kind    u16 LE  message kind (§5–§7)
10     2    flags   u16 LE  bit 0 REPLY, bit 1 ERROR, bit 2 NO_REPLY; all other bits 0
12     len−8        payload
```

`MAX_FRAME = 16 MiB + 64 = 16_777_280`. A `len` below 8 or above `MAX_FRAME`, an unknown flag bit, `ERROR` without `REPLY`, or `NO_REPLY` with `REPLY` is a protocol error (§4.3).

- **Request:** `flags = 0`. The receiver answers with exactly one frame carrying the same `id` and `kind` and `REPLY` (or `REPLY | ERROR`).
- **Notification:** `flags = NO_REPLY`. Never answered. Only `Log` is one.
- **Ids.** Each side numbers the requests *it* sends, starting at 1 and counting up; the two directions are separate number spaces (a frame's `REPLY` bit says which space its `id` is in). An id is never reused in a session. A side that reaches `u32::MAX` closes the session. `id = 0` is used only by `Hello`.
- Replies may come in any order. A reply whose id matches no waiting request is discarded if that request timed out earlier in this session, and is a protocol error otherwise. (Implementation: keep the highest id sent; a reply at or below it with no waiter was a timeout.)

## 3. Encoding

Integers are little-endian. There is no padding and no alignment.

| Type | Bytes |
|---|---|
| `u8`, `u16`, `u32`, `u64`, `i64` | as is, little-endian |
| `bool` | `u8`, 0 or 1; anything else is malformed |
| `string(max)` | `u32` byte length ≤ `max`, then that many bytes of valid UTF-8 |
| `bytes(max)` | `u32` length ≤ `max`, then the bytes |
| `option<T>` | `u8` 0 (absent) or 1 followed by `T` |
| `list<T>(max)` | `u32` count ≤ `max`, then the items |
| `[u8; N]` | N bytes |

A payload must be consumed exactly: bytes left over after the last field are malformed. Where a limit is not stated beside a field, strings are at most 4096 bytes.

### Error payload (a frame with `REPLY | ERROR`)

`u16 code`, `string(4096) message`.

| Code | Meaning | Engine `Error` on the host |
|---|---|---|
| 1 | I/O failure | `Io` with the message |
| 2 | Access denied | `AccessDenied` |
| 3 | Read outside the source or stream | `OutOfBounds` (offset, length and size from the request; the message is dropped) |
| 4 | Damaged structure | `Corrupt` with structure "extension volume" and `CorruptKind::Foreign(message)` |
| 5 | Not supported | `Unsupported(message)` |
| 6 | No such node | `NotFound { node }` from the request |
| 7 | Not a folder | `NotADirectory { node }` |
| 8 | Is a folder | `IsADirectory { node }` |
| 9 | No such stream | `NoSuchStream { node }` |
| 10 | Malformed request, unknown handle | `Io`, "the <name> extension rejected a request" (a host bug or version skew; logged at warn) |
| 11 | Unknown message kind | `Unsupported("<name> does not implement this")` |
| 12 | Too many open handles | `Io` with the message |

An unknown code is treated as code 1. Codes are stable; new ones are added at the end.

## 4. Session

### 4.1 Hello

The first frame each side sends, `id = 0`, `kind = 0x0001`, `flags = 0`. It is a request in form but is never answered; each side's `Hello` is the other's answer.

```
[u8; 4]      magic   "DAPP"  (44 41 50 50)
u16          version 1
u8           role    1 host, 2 filesystem extension, 3 device helper
string(256)  name    for logs ("Disk Atlas", the extension's manifest name)
string(64)   build   the sender's own version text
```

The host sends role 1 and expects 2 from an extension and 3 from the helper. Wrong magic, a version the reader does not speak, or an unexpected role closes the session. Each side sends its `Hello` without waiting for the other's, and waits at most 5 s for it.

### 4.2 Limits and timeouts

| Constant | Value | Applies to |
|---|---|---|
| `MAX_FRAME` | 16 MiB + 64 | one frame |
| `MAX_READ` | 16 MiB | bytes asked for in one `Read`, `ReadFile`, `ReadDevice`, `ReadXattr` |
| `MAX_IN_FLIGHT` | 64 | unanswered requests one side may have outstanding; the sender waits |
| `MAX_HANDLES` | 4096 | open handles of each kind (volume, folder, file, scan, device) per session |
| `REQUEST_TIMEOUT` | 30 s | from sending a request to its reply |
| `HELLO_TIMEOUT` | 5 s | the handshake |
| `MAX_RESTARTS` | 3 | times the host restarts a dead extension in one app session |

A request that times out fails on the host with `Io` ("the <name> extension did not answer"); the session goes on. Three timeouts in a row end it.

### 4.3 Protocol errors and the end of a session

A protocol error is any frame or payload that breaks this file: a bad length, bad flags, a malformed payload, a count or string over its limit, a reply to an id never sent, a handle the receiver never issued (in a *reply*; in a request it is error code 10), a request kind sent in the wrong direction.

On a protocol error, on end-of-file, or on a write failure, the host: marks the session dead; fails every waiting and every later call with `Io` ("the <name> extension stopped"); kills the child process and reaps it; and marks every source mounted through it `Failed`. It then restarts the extension lazily, on the next probe, up to `MAX_RESTARTS`. The helper and an extension, on a protocol error from the host, exit.

Closing: the host closes its write end. The child sees end-of-file on its input and exits. The host waits 2 s, then kills it.

### 4.4 Threading on the host (`crates/lib/src/ipc/`)

The engine has no executor; this is plain threads.

- One **reader thread** per session parses frames. A reply is handed to its waiter through a `Mutex<HashMap<u32, SyncSender<Reply>>>`. An incoming request (`Read`) is handed to a pool of 4 worker threads over a bounded channel of `MAX_IN_FLIGHT`. **The reader thread never writes to the pipe and never blocks on anything but the pipe and that channel**; that is what makes the protocol deadlock-free (a full pipe in one direction cannot stop replies in the other).
- One `Mutex` around the write end. A frame is written whole under it.
- A caller sends its request, then blocks on its own channel with `recv_timeout(REQUEST_TIMEOUT)`.
- `RemoteFs`, `RemoteDir`, `RemoteFile` and `RemoteDevice` hold an `Arc<Session>` and a handle; `Drop` sends the matching close as a best-effort request without waiting for the reply.

## 5. Host → filesystem extension

Handles are `u32` values chosen by the extension, unique among its live handles of the same kind. `node` is the engine's `NodeId` as a `u64`. `source` is a `u64` chosen by the host, meaningful only in `Read` (§6).

| Kind | Name | Request payload | Reply payload |
|---|---|---|---|
| `0x0010` | Probe | `u64 source`, `u64 len`, `u32 sector_size`, `bytes(65536) head` | `option<string(32)>` format name |
| `0x0011` | Mount | `u64 source`, `u64 len`, `u32 sector_size` | `u32 volume`, `VolumeInfo` |
| `0x0012` | Unmount | `u32 volume` | empty |
| `0x0020` | Metadata | `u32 volume`, `u64 node` | `Metadata` |
| `0x0021` | Lookup | `u32 volume`, `u64 dir`, `string name` | `option<u64>` node |
| `0x0022` | OpenDir | `u32 volume`, `u64 dir` | `u32 dir` |
| `0x0023` | NextBatch | `u32 dir`, `u32 max` (1 ..= 16384) | `Batch` |
| `0x0024` | CloseDir | `u32 dir` | empty |
| `0x0030` | Open | `u32 volume`, `u64 node`, `u8 stream` (0 data, 1 resource) | `u32 file`, `u64 len` |
| `0x0031` | ReadFile | `u32 file`, `u64 offset`, `u32 len` (≤ `MAX_READ`) | exactly `len` bytes, raw, with no length prefix |
| `0x0032` | CloseFile | `u32 file` | empty |
| `0x0040` | Extents | `u32 volume`, `u64 node`, `u8 stream` | `list<(u64 offset, u64 len)>(1_000_000)`, every `len` non-zero |
| `0x0041` | AllocationMap | `u32 volume`, `u32 buckets` (1 ..= 4096) | `u64 start`, `u64 len`, `bytes(4096) density` |
| `0x0042` | Xattrs | `u32 volume`, `u64 node` | `list<(string(1024) name, u64 size)>(65536)` |
| `0x0043` | ReadXattr | `u32 volume`, `u64 node`, `string(1024) name`, `u32 max` (≤ `MAX_READ`) | `option<bytes(max)>` |
| `0x0044` | Identity | `u32 volume` | `option<([u8; 16] id, [u8; 16] change)>` |
| `0x0050` | ScanOpen | `u32 volume` | `u32 scan` |
| `0x0051` | ScanNext | `u32 scan`, `u32 max` (1 ..= 16384) | `ScanBatch` |
| `0x0052` | ScanClose | `u32 scan` | empty |

Rules:

- **Probe** runs only on content every built-in probe calls `Unknown`, after them, and the extension may send `Read` for `source` until it replies. The format name must be one the extension's manifest lists; otherwise the reply is treated as `None`.
- **Mount.** The extension may `Read` from `source` until it answers `Unmount` or the session ends. `Unmount` closes every folder, file and scan handle of the volume.
- **A method the extension does not have** is answered with error code 5 or 11, and the host then behaves as the engine's trait default does: `Extents` and `AllocationMap` → `Unsupported`; `Xattrs` → empty; `ReadXattr` → `None`; `Identity` → `None`; `Open` → `Unsupported`; `ScanOpen` → the host walks the volume through `OpenDir` (`default_scan`). `Metadata`, `Lookup`, `OpenDir`, `NextBatch` are required.
- **NextBatch and ScanNext are pulled**, like `DirStream::next_batch`: the host asks, the extension answers with at most `max` entries. There is no pushed stream and so no cancel message: the host stops asking and closes the handle. An extension whose scan is a callback runs it on its own thread behind a bounded queue.
- **ReadFile** fills the whole range or fails (code 3 past the end): no short reads, as `BlockSource::read_at`.
- Snapshots and locked volumes are not in version 1. An extension volume has none.

### Structures

`VolumeInfo`:

```
string(1024)        name
option<string(32)>  format        must be a name from the extension's manifest
option<u64>         total_bytes
option<u64>         free_bytes
bool                resource_forks
bool                local_times
u8                  journal       0 none, 1 clean, 2 replayed, 3 not replayed
  when 2:  u32 transactions, bool stopped_early
  when 3:  string(1024) reason
```

`VolumeInfo::format` is a `&'static str` in the engine. The host interns each manifest format name once when the manifest is loaded (at most 16 names of at most 32 bytes per extension, leaked deliberately) and uses that.

`Metadata`:

```
u64                 node
u8                  kind          0 folder, 1 file, 2 symbolic link, 3 other
option<u64>         size
option<u64>         resource_size
option<i64>         created       nanoseconds since the Unix epoch
option<i64>         modified
option<i64>         accessed
u16                 flags         EntryFlags bits: 1 hidden, 2 links to a folder, 4 compressed
option<string>      link_target
option<[u8; 8]>     finder_codes  type then creator
option<(u32, u32, u16)> owner     user, group, mode
option<u32>         attributes
option<[u8; 32]>    finder_info
```

A flag bit the host does not know is cleared, not an error (a newer extension may set bits a version-1 host predates).

`Entry` (one folder entry, 32 bytes plus the name):

```
u64           node
u8            kind
u8            present     bit 0 size, bit 1 modified, bit 2 resource size; other bits 0
u16           flags
u64           size            0 when not present
u64           resource_size   0 when not present
i64           modified        0 when not present
string(1024)  name            not empty; not "." or ".."
```

`Batch` (the reply to `NextBatch`):

```
u8            status      0 more, 1 done, 2 failed
list<Entry>(max)
when status = 2:  u16 code, string(4096) message
```

Status 2 is the `DirStream` contract over the wire: the entries in the reply are valid and are kept, the listing is over, and the host reports the error. The frame is an ordinary reply, not an `ERROR` frame. An `ERROR` reply to `NextBatch` means the same with no entries.

`ScanBatch`:

```
u8            status      0 more, 1 done, 2 failed
list<(u64 parent, Entry)>(max)
when status = 1:  u64 skipped      structures the scan could not read
when status = 2:  u16 code, string(4096) message
```

The host counts entries itself. The contract of `FileSystem::scan` holds: children may come before parents, the root is never reported, a node may repeat under different parents.

## 6. Filesystem extension → host

| Kind | Name | Request payload | Reply payload |
|---|---|---|---|
| `0x0100` | Read | `u64 source`, `u64 offset`, `u32 len` (≤ `MAX_READ`), `bool bulk` | exactly `len` bytes, raw |
| `0x0101` | Log | `u8 level` (0 error, 1 warn, 2 info, 3 debug, 4 trace), `string(4096) text` | none (`NO_REPLY`) |

`Read` is the only way an extension reaches a disk. The host answers only for a `source` it has offered this session in a `Probe` not yet answered or a `Mount` not yet unmounted; any other source is error code 10. The range is checked against the source's length (code 3). `bulk = true` goes through `BlockSource::read_bulk` (large sequential reads: use it for scans and file content), `false` through `read_at` (the chunk cache).

**An extension never opens the device or image itself.** That is the whole security argument: an extension cannot write to a disk and needs no rights.

## 7. Host → device helper

| Kind | Name | Request payload | Reply payload |
|---|---|---|---|
| `0x0200` | ListDevices | empty | `list<Device>(256)` |
| `0x0201` | OpenDevice | `string(1024) path` | `u32 device`, `u64 size`, `u32 sector_size` |
| `0x0202` | ReadDevice | `u32 device`, `u64 offset`, `u32 len` (≤ `MAX_READ`) | exactly `len` bytes, raw |
| `0x0203` | CloseDevice | `u32 device` | empty |

`Device`: `string(1024) path`, `option<string(256)> model`, `u64 size`, `u32 sector_size`, `bool removable` (the fields of `io::device::DeviceInfo`).

The helper answers nothing else (code 11). `OpenDevice` accepts only a path equal, byte for byte, to one in the helper's own most recent `ListDevices` reply (else code 10): the app cannot make the helper open an arbitrary file. Devices are opened read-only, through the same `io::device::open` the app uses. The helper never sends a request; a `Log` notification from it is allowed.

## 8. Starting the peers

### 8.1 Extensions

- An extension is a folder `<config dir>/extensions/<name>/` holding a `manifest` file and the program. Nothing is loaded from anywhere else; the app downloads nothing.
- `manifest` is `key = value` lines, `#` comments, as the settings file:

  | Key | Value |
  |---|---|
  | `name` | shown in Settings; 1–64 characters |
  | `version` | free text, ≤ 64 |
  | `protocol` | `1` |
  | `command` | the program, as a path relative to the extension's folder. Rejected when absolute, when any component is `..`, or when the resolved path (after `canonicalize`) is not inside the folder |
  | `args` | optional; split on spaces, no quoting, ≤ 16 arguments |
  | `formats` | comma-separated format names, 1–16 of them, each 1–32 bytes of printable ASCII |

- The child is started with `std::process::Command`: working directory the extension's folder; standard input, output and error piped; the environment cleared except `PATH`, and on Windows also `SystemRoot`, `TEMP` and `TMP` (most runtimes will not start without them), elsewhere `HOME` and `LANG`.
- It is started on the first probe that reaches it, not at app start, and only when the user has turned the extension on.

### 8.2 The device helper

The helper is the app's own executable started as `disk-atlas --device-helper …` with administrator rights. This is the part that **needs a real test with UAC and with `pkexec` before it is final** (task 14, Julian's go-ahead required: it is privileged code taking requests). What is decided, and what the test must confirm:

| Platform | Channel | How it is started |
|---|---|---|
| Linux | The helper's standard input and output | `Command::new("pkexec").arg(<own exe>).arg("--device-helper")` with piped stdio. `pkexec` passes stdio through and cleans the environment. No `unsafe`. No polkit agent or a refusal: the app keeps today's hint text. |
| Windows | A duplex named pipe `\\.\pipe\disk-atlas-<32 hex digits from the OS random source>` | The app creates the pipe first (`CreateNamedPipeW`: `PIPE_ACCESS_DUPLEX \| FILE_FLAG_FIRST_PIPE_INSTANCE`, `PIPE_TYPE_BYTE \| PIPE_REJECT_REMOTE_CLIENTS`, one instance, a security descriptor granting access to the current user's SID and to `BUILTIN\Administrators` only), then starts the helper through the existing `ShellExecuteExW` `runas` path with `--device-helper --pipe <name> --parent <pid>`. The helper opens the pipe with `std::fs::OpenOptions` (no FFI on its side for the open). A process started through `runas` cannot inherit handles or stdio, which is why this is a named pipe. |
| macOS | A Unix domain socket in a fresh directory of mode 0700 under the user's temporary folder | `osascript -e 'do shell script "<exe> --device-helper --socket <path> >/dev/null 2>&1 &" with administrator privileges'`. `do shell script` does not connect stdio, hence the socket. |

Checks, all platforms:

- The helper exits when the channel closes, and on Windows and macOS also when it cannot connect within 10 s.
- **Windows, helper side:** `GetNamedPipeServerProcessId` must equal `--parent`, and that process's image path (`QueryFullProcessImageNameW`) must equal the helper's own executable path. **App side:** `GetNamedPipeClientProcessId`'s image path must equal the app's own. These are two FFI calls each, in O's hands with `// SAFETY:` comments.
- **macOS, helper side:** the socket's parent directory is owned by the uid in `--uid` and has mode 0700 (`std::os::unix::fs::MetadataExt`, no FFI).

What this does and does not protect. The UAC or polkit prompt is the user's consent to "Disk Atlas may read disks". After consent, the helper gives raw read access to whoever holds the channel. The checks above stop another program from borrowing the consent by starting the helper itself with its own pipe. They do not stop a program that already runs as the same user from injecting code into the running app; nothing at this level can, and Windows does not treat same-user elevation as a security boundary. The helper's narrowness (three requests, listed devices only, read-only) is what limits the damage: it can disclose disk contents, never change them. Say this plainly in the README's security section when the helper ships.

To confirm with a real prompt before building: (1) the elevated helper can open a pipe created by the unelevated app under that security descriptor, both when the user is an administrator and when a different administrator's credentials are typed into the prompt; (2) `ShellExecuteExW` reports a declined prompt the way `elevation.rs` already expects; (3) `pkexec` really leaves stdin and stdout connected under GNOME and KDE agents; (4) the macOS backgrounded command survives `osascript` returning.

When the helper exists, "Restart as administrator" becomes "Allow reading disks", the main process never runs elevated, and `RemoteDevice` sits under the usual `CachedSource`.

## 9. Conformance tests (task 11 builds these with the host side)

- Codec round trips for every message and structure; every decoder against truncation at each byte, lengths over the limit, bad `bool` and `option` tags, trailing bytes, invalid UTF-8.
- A fuzz target `ipc_frames` that feeds arbitrary bytes to the host's reader as if from an extension, with a scripted set of waiting requests: no panic, no hang, memory bounded by `MAX_FRAME`.
- An in-process loopback (two sessions joined by `std::io::pipe` or a pair of in-memory queues) serving the HFS+ test volume through `RemoteFs`: listing, lookup, metadata, file reads and scan equal the direct results. This is also the reference extension's core.
- Misbehaving peers, each as a scripted fake: never answers (timeout, then the session survives); answers twice; answers with a wrong kind; dies mid-frame; sends `Read` for a source it was never offered; floods `Log`.
- The example extension (`plugins/example-romfs/`, task 12) is built and run against a tiny image in CI.

## 10. Out of scope for version 1

Snapshots and password unlock through an extension; an extension that writes; shared memory or file-descriptor passing for reads (the 16 MiB frame is enough: 1 GiB/s needs 64 round trips a second); compression of frames; a network transport.
