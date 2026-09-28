# ZippedServing

A single Windows x64 executable serving a local directory through an embedded
web application. Browse folders, upload files, create folders, and download files
or entire directory trees with **on-the-fly parallel LZ4 compression**.
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

Set `$env:RUST_LOG = 'debug'` for detailed server logs.

## Web application and formats

Select a folder to browse it; use the breadcrumbs to navigate. Upload with the
file picker or drag and drop. Multiple files upload sequentially with progress
and cancellation. New folders can be created in the current directory.
Folder upload is not implemented; create folders and upload their files.

* **File:** `filename.lz4`, a standard LZ4 frame containing the original bytes.
* **Directory:** `directory.tar.lz4`, a standard LZ4 frame containing a tar
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

The workflow runs formatting, Clippy, unit/integration tests, an optimized build,
and a real HTTP smoke test. It publishes the executable, documentation, and
SHA-256 checksum as a 30-day artifact. Push to `main`, open a pull request, or
use **Run workflow** to build.

To fetch and verify the latest successful `main` build using an existing
`GH_TOKEN` environment variable (requires repository Actions read permission):

```powershell
.\scripts\download-artifact.ps1
.\scripts\smoke-test.ps1 -Executable .\artifacts\windows-x64\zipped-file-serving.exe
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
