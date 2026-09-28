# ZippedServing

A standalone Windows x64 server and a separate standalone Windows x64 client.
Browse folders, upload files, create folders, and download files or entire
directory trees with **streaming LZ4 or zstd compression**.
No compressed download or intermediate tar archive is written on the server.
There are no runtime packages, frontend assets, or Rust tools to install.

## Run

Download `zipped-file-serving-windows-x64` from a successful
[Windows executable workflow](https://github.com/kchan345/ZippedServing/actions/workflows/windows.yml),
unzip it, and run in PowerShell:

```powershell
.\zipped-file-serving.exe 'D:\Files'
```

The directory must already exist. By default the server listens on
**0.0.0.0:8081**. Open `http://localhost:8081` locally or
`http://<server-ip>:8081` from another machine. `0.0.0.0` is the bind address,
not the browser address. Windows Firewall may require permission for inbound
TCP 8081 on your trusted/private network; the utility does not change firewall
rules. Stop with Ctrl+C (active transfers are allowed to finish).

**Trusted networks only.** There is no authentication or TLS. Every reachable
client can read the served tree and create files/directories. Do not expose
this directly to the internet or point it at sensitive directories. Bind to
loopback for local-only use, or put it behind an authenticated TLS reverse proxy.
Existing files cannot be overwritten, and deletion is not exposed.

```powershell
.\zipped-file-serving.exe 'D:\Files' --bind 127.0.0.1 --port 9000
.\zipped-file-serving.exe 'D:\Files' --compression-threads 8 --max-downloads 2
.\zipped-file-serving.exe --help
```

| Option | Default | Meaning |
| --- | --- | --- |
| `directory` | Required | Existing root; includes subdirectories |
| `--bind` | `0.0.0.0` | Listening IPv4 or IPv6 address |
| `--port` | `8081` | TCP port; `0` selects a free port |
| `--compression-threads` | Logical CPUs, capped at 8 | Shared compression workers, 1-64 |
| `--max-downloads` | `2` | Active downloads, including raw; 1-256 |
| `--max-uploads` | `4` | Active uploads; 1-256 |
| `--max-upload-bytes` | `107374182400` | Per-file upload limit (100 GiB) |
| `--max-chunk-mib` | `256` | Maximum negotiated uncompressed chunk size, 1-1024 MiB |
| `--profiles-file` | `%LOCALAPPDATA%\ZippedServing\profiles.json` | Shared command profiles; must be outside the served tree |

Set `$env:RUST_LOG = 'debug'` for detailed server logs.

## Web application and formats

Select a folder to browse it; use the breadcrumbs to navigate. Upload with the
file picker or drag and drop. Multiple files upload sequentially with progress
and cancellation. New folders can be created in the current directory.
Folder upload is not implemented; create folders and upload their files.

* **File:** `filename.lz4` or `filename.zstd`, a standard frame containing the original bytes.
* **Directory:** `directory.tar.lz4` or `directory.tar.zstd`, a standard frame containing a tar
  archive with one top-level directory, nested files, and empty directories.
* **Raw:** uncompressed file download for clients without a decoder or for
  already-compressed data. Directories require compression.

These are **not ZIP files**, and browsers do not automatically extract them.
Use an LZ4-capable decoder on the receiving machine. For example, where the
optional `lz4` command is available:

```powershell
lz4 -d .\report.txt.lz4 .\report.txt
lz4 -d .\photos.tar.lz4 .\photos.tar
tar -xf .\photos.tar
```

Alternatively, the included native client extracts both formats directly from
HTTP without needing an external decoder or saving the compressed archive.

## Copy a download command from the web application

Click **Client command** beside a directory, or in the toolbar for the currently
open directory (including the root). The command panel generates a **PowerShell**
command for the standalone client. It does not execute commands in the browser
or on the server.

Create a named profile containing one **client executable path** (for example
`C:\Tools\zipped-file-client.exe`) and one **general download folder** (for example
`D:\Downloads`). These are absolute Windows drive or UNC paths on the **client
machine**; the server does not check whether they exist there. Save the profile,
select it from the dropdown, review the command, and click **Copy command**.
Names must be unique (case-insensitive). You can edit or delete profiles.
There is a hard limit of **99 profiles**, enforced on both server and UI;
editing and deleting still work at the limit.

The generated command uses verified chunk downloads with the client's default
codec and split size. For directory `photos`, it defaults to:

```powershell
& 'C:\Tools\zipped-file-client.exe' download 'http://server:8081/api/download?path=photos' 'D:\Downloads\photos-download'
```

The base folder `D:\Downloads` must already exist. The new output container
`photos-download` must **not** exist, and the downloaded tree will be under
`D:\Downloads\photos-download\photos`. Change **New output folder name** for
repeat downloads. For the server root, the suggested container is `root-download`.
The command URL uses the browser's current server origin: use the server's
LAN hostname/IP in the browser if the command will run on another machine
(`localhost` on that client would point to itself).

Paths, URLs, spaces, apostrophes, and PowerShell metacharacters are quoted as
literal arguments. Copy uses the clipboard API on HTTPS/localhost, with a
selection-based fallback for ordinary HTTP LAN pages. If browser permissions
prevent copying, the command is selected and the UI tells you to press Ctrl+C.
Unsaved edits must be saved before a command is generated for that profile.

**Profiles are stored on the server, not in browser local storage.** They survive
browser and server restarts and are available to all browsers. Selection is
per-page; on a fresh page the first saved profile is selected. Profiles are
shared by server instances using the same profile file, even with different
served roots. To isolate profiles or choose a different storage location:

```powershell
.\zipped-file-serving.exe 'D:\Files' --profiles-file 'C:\ServerState\profiles.json'
```

The parent directory is created when profiles are first used. The file and its
adjacent `.lock` file must stay outside the served directory, in a location
writable by the server account. The lock serializes access across processes;
updates use a flushed temporary file and atomic replacement. Browser edits
carry a revision: if another browser changes profiles, reload before saving
instead of silently overwriting its changes. Invalid, unreadable, or unwritable
storage is reported explicitly; a damaged store is never silently reset.
File serving remains available if profile storage fails.

**No authentication is added:** everyone who can reach the server can read,
edit, or delete shared profiles. Do not store secrets or sensitive paths, and
review shared executable paths before running generated commands.

## Standalone client: verified chunk transfers

`zipped-file-client.exe` is a single executable, independent of the server
executable. No Rust, codec, or other runtime installation is required.

```powershell
# Directory or file download to a NEW local directory:
.\zipped-file-client.exe download `
  'http://server:8081/api/download?path=photos' 'D:\Received'

# Request zstd with 64 MiB uncompressed chunks:
.\zipped-file-client.exe download `
  'http://server:8081/api/download?path=photos' 'D:\ReceivedZstd' `
  --codec zstd --split-mib 64

# Stream-extract a conventional tar archive from this server:
.\zipped-file-client.exe download `
  'http://server:8081/api/download?path=photos&format=zstd' 'D:\Extracted' --archive

# Ordinary archive URLs select archive mode automatically:
.\zipped-file-client.exe download `
  'https://example.test/photos.tar.lz4' 'D:\ExtractedLz4'

# Upload one file, or an entire tree including empty directories:
.\zipped-file-client.exe upload 'D:\report.bin' `
  'http://server:8081/api/upload?path=report.bin'
.\zipped-file-client.exe upload 'D:\Photos' `
  'http://server:8081/api/upload?path=incoming-photos' --codec zstd
```

For downloads, the output directory must **not exist**, and its parent must
exist. `photos` appears under `D:\Received\photos`; a single file similarly
appears under the new output directory. Downloads first write decompressed
files into an adjacent temporary directory and publish it only after every
chunk/frame verifies. Errors clean up that staging directory. No compressed
archive or whole-chunk temporary file is created.

For `/api/download` URLs the default is the **chunk protocol**, not tar. A
manifest preserves the same file tree, and each regular file is transferred
in separate HTTP requests. `--codec` selects the negotiated codec in this
mode; the URL's legacy `format` parameter is used only with `--archive`.
`--archive` streams a single ordinary tar frame and does **not** provide
chunk retries or XXH3-128 verification; it validates codec checksums instead.
Both `.tar.zst` and `.tar.zstd` work; detection uses frame magic, not extension.
There is no silent fallback from a failed negotiation to an unverified download.

The client proposes an uncompressed split size (default **256 MiB**), and the
server replies with the smaller of that and `--max-chunk-mib`. Sizes from 1 MiB
to 1 GiB are accepted. Each nonempty file is split independently; the last
chunk may be shorter. Empty files need no chunk request. Chunk boundaries are
independent of LZ4's internal 4 MiB codec blocks or the 256 KiB I/O buffers.
An entire 256 MiB chunk is **never buffered in client memory**.

Every chunk carries its offset, raw length, codec, and an **XXH3-128** hash of
the uncompressed bytes. Hashing occurs incrementally in the compression/
decompression copy loop, without a second disk pass. Download chunks are
retried up to three total attempts. Uploads advance only after the server
decodes and verifies the chunk; bad chunks are truncated back to the previous
verified offset, and the client checks the returned hash and next offset.
Only a completed upload is published without overwriting.

XXH3-128 was selected for high throughput and low CPU cost, not cryptographic
security. It detects accidental corruption; a malicious peer can forge it.
Use HTTPS through a trusted reverse proxy when authentication or protection
against active modification is required. SHA-256 artifact checksums are
separate from the fast transfer hash.

| Client option | Default | Meaning |
| --- | --- | --- |
| `--split-mib` | `256` | Requested uncompressed chunk size, 1-1024 MiB |
| `--codec` | `lz4` | `lz4` or `zstd`, for chunk transfers |
| `--max-bytes` | `107374182400` | Aggregate input/output limit; archive mode counts tar headers/padding too |
| `--timeout-seconds` | `3600` | Maximum time per HTTP request |
| `--compression-threads` | `2` | Parallel LZ4 upload workers; decoding is serial |

The current client processes chunks and files sequentially. It does not persist
resume state across runs. Uploads do not automatically retry ambiguous network
failures; the client attempts to cancel that file's session and reports failure.
Inactive upload sessions expire after 15 minutes and are reaped by subsequent
session operations. Abrupt server termination can leave staging files.
Directory uploads publish individual files, not a transactional whole tree:
completed files/directories remain if a later file fails. A fresh destination
is required; existing files/directories are never intentionally merged.
Do not change source files while transferring: metadata checks detect ordinary
changes but are not filesystem snapshots.

The receiver may save/extract archives; the no-intermediate-disk constraint
applies to the server's compression pipeline. Do not pipe binary archive data
through older PowerShell versions that convert native output to text.

Symbolic links, junctions, other Windows reparse points, and special files are
blocked for direct access and omitted from directory archives. Active upload
files (`.zfs-upload-*`) are hidden and omitted. Unsupported Windows filenames
are blocked; a directory containing an unsupported filename can fail during
archive streaming. Files with Windows alternate data streams are not supported;
only the default file stream is archived. Hard links are treated as regular files.
See [architecture.md](architecture.md) for consistency and security boundaries.

Uploads stream into a temporary file **in the destination directory**, then
publish without overwriting only after the whole upload is received and flushed.
These are upload staging files, not intermediate compression results. Failed or
cancelled uploads are removed on normal cleanup; abrupt process termination
can leave `.zfs-upload-*.part` files. Remove confirmed stale files manually
while the server is stopped.

## HTTP API

Paths are relative to the configured root, URL-encoded, with `/` between
components. This URL separator is independent of Windows filesystem syntax.

| Method | Route | Result |
| --- | --- | --- |
| GET | `/api/list?path=` | JSON entries, types, sizes, modification times, upload limit |
| GET | `/api/download?path=report.txt` | Streaming `.lz4` |
| GET | `/api/download?path=photos` | Streaming `.tar.lz4` |
| GET | `/api/download?path=report.txt&format=raw` | Original file |
| PUT | `/api/upload?path=new.txt` | Raw request body saved as a new file |
| POST | `/api/mkdir?path=new-folder` | Create one folder; parent must exist |
| GET | `/api/transfer/manifest?path=photos&chunk_size=268435456&codec=lz4` | Negotiate chunks and list tree |
| GET | `/api/transfer/chunk?path=...&stamp=...&offset=0&chunk_size=268435456&codec=lz4` | One compressed, verified file chunk |
| POST | `/api/transfer/uploads` | JSON `{path,size,chunk_size,codec}`; create upload session |
| PUT | `/api/transfer/uploads/{id}?offset=0` | Stream one chunk; return verified hash/next offset |
| POST | `/api/transfer/uploads/{id}/complete` | Publish a fully received file |
| DELETE | `/api/transfer/uploads/{id}` | Cancel and remove staged upload |
| GET | `/api/profiles` | JSON `{version,revision,profiles}` |
| POST | `/api/profiles` | Create `{revision,name,client_path,download_folder}` |
| PUT | `/api/profiles/{id}` | Update the same fields using the latest revision |
| DELETE | `/api/profiles/{id}?revision=...` | Delete a shared profile using the latest revision |

Writes require `X-Requested-With: zipped-file-serving`. This is a browser
cross-origin write safeguard, **not authentication**. No CORS access is enabled.

```powershell
curl.exe --fail-with-body -H 'X-Requested-With: zipped-file-serving' `
  --upload-file '.\report.txt' 'http://localhost:8081/api/upload?path=report.txt'
curl.exe --fail --output '.\files.tar.lz4' 'http://localhost:8081/api/download?path='
```

Successful writes return 201. Common failures: 400 invalid path, 403 forbidden
path/header, 404 missing source, 409 existing destination, 413 upload too large,
503 active transfer limit. Filesystem errors are logged; a failure after download
headers have been sent aborts the stream, so discard incomplete downloads.
Compressed sizes are not known up front; download responses do not advertise
Content-Length or support ranges/resume.

## Build and download without local build tools

All compilation happens on GitHub's **`windows-2025` x64 hosted image**, targeting
`x86_64-pc-windows-msvc`. This produces an executable compatible with the local
Windows 11 x64 environment. Rust, native liblz4, and the C runtime are statically
linked; Windows system DLLs are still required. No Docker or local compiler is
needed. The executable is unsigned, so Windows SmartScreen may warn.

The workflow runs formatting/lockfile checks, Clippy, unit/integration tests,
optimized builds, and real HTTP smoke tests for both binaries. A 256 MiB + 17
byte fixture crosses the default chunk boundary for both codecs and directions;
the test requires peak client working set below 128 MiB. It publishes both executables, documentation, and
SHA-256 checksum as a 30-day artifact. Push to `main`, open a pull request, or
use **Run workflow** to build.

Profile API tests cover persistence, the exact 99-profile limit, conflicts,
validation, and storage failures. Hosted Node tests parse generated commands
with PowerShell, and Playwright tests run in the hosted image's Microsoft Edge
to exercise the UI, clipboard behavior, server restarts, and execution of a
generated command with the real client. These test-only packages are installed
on GitHub, not required by the executable or installed locally.

To fetch and verify the latest successful `main` build using an existing
`GH_TOKEN` environment variable (requires repository Actions read permission):

```powershell
.\scripts\download-artifact.ps1
.\scripts\smoke-test.ps1 -Executable .\artifacts\windows-x64\zipped-file-serving.exe
.\scripts\client-smoke-test.ps1 `
  -Server .\artifacts\windows-x64\zipped-file-serving.exe `
  -Client .\artifacts\windows-x64\zipped-file-client.exe -Large
.\artifacts\windows-x64\zipped-file-serving.exe 'D:\Files'
```

Scripts use PowerShell 7 and Windows' existing `tar.exe`, not local build tools.
Specify `-RunId <id>` to download a particular successful run. The smoke test
uses a disposable directory, a loopback listener on an automatically selected
port, independently decodes LZ4 data, and stops its own process afterward.
It keeps diagnostics on failure and removes its test directory on success.

## Performance

LZ4 fast mode with parallel 4 MiB blocks prioritizes low CPU use and throughput;
zstd generally produces smaller output. Worker count and transfer concurrency
are bounded; increasing them increases memory and CPU use. Use raw downloads
for data that cannot benefit from compression.

**NVMe saturation is a goal, not a measured guarantee.** Storage, input
compressibility, CPU, client decode speed, network bandwidth, and small-file
metadata overhead all matter. A 1 Gb/s or 10 Gb/s link is usually slower than
an NVMe device. No benchmark result is claimed for this machine; the automated
tests check correctness, not an NVMe throughput threshold.
