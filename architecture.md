# Architecture and tradeoffs

## Shape of the executables

Rust/Axum/Tokio handles HTTP and asynchronous uploads. HTML, CSS, and JavaScript
are compiled in using `include_str!`; no asset directory, Node runtime, database,
or service installation is needed. Clap handles the directory argument and
resource limits. A shared Rayon pool does native liblz4 block compression.
The separate Rust client uses blocking reqwest/rustls with streaming responses,
native codec decoders, and staged output. Both MSVC Windows x64 builds statically
link the C runtime via `.cargo/config.toml`.
GitHub's Windows Server 2025 hosted image supplies MSVC, rather than assuming
Linux-built binaries will run on Windows. Generic x64 code is used, not
`target-cpu=native`, so runner-specific CPU instructions are not required.

Files:

| File | Responsibility |
| --- | --- |
| `src\main.rs` | CLI, listener, logging, graceful shutdown |
| `src\lib.rs` | Routing, limits, listings, uploads, error responses |
| `src\paths.rs` | Relative path validation and reparse-point checks |
| `src\compression.rs` | Ordered, bounded, parallel LZ4 frame writer |
| `src\download.rs` | File/tar production, HTTP backpressure, attachment names |
| `src\web\` | Embedded browser application |
| `src\profiles.rs` | Durable, revision-controlled command profile CRUD |
| `src\transfer.rs` | Shared codec, chunk envelope, negotiation types, incremental XXH3-128 |
| `src\chunk_api.rs` | Manifest, independent chunk requests, staged upload sessions |
| `src\client.rs` | Streaming client, extraction, chunk verification and publication |
| `src\bin\zipped-file-client.rs` | Standalone client CLI |
| `tests\http.rs` | API and filesystem integration tests |
| `scripts\smoke-test.ps1` | Black-box Windows executable test without a compiler |

## Server-persisted web command profiles

The embedded web application can generate a PowerShell client download command
for the current root/directory or a child directory row. A shared profile stores
only its UUID, display name, client executable path, and general destination
folder. Up to **99 profiles** are allowed, with case-insensitive unique names.
Paths are client-side absolute Windows drive/UNC strings, not server filesystem
operations. Profiles cannot supply arbitrary shell arguments or a command
template. The selected remote directory, endpoint URL, and new output container
are derived separately.

The UI invokes the client using PowerShell's `&` operator with literal quoted
arguments. ASCII and curly single quotes are doubled; control characters are
rejected. Directory URL parameters are URL-encoded, and all display values use
text/value properties rather than HTML. The default output container is
`<directory>-download` (or `root-download`), because the client refuses existing
output directories. The container name can be changed for repeat downloads,
but cannot contain traversal or path separators. The browser origin supplies
the server address; neither a bind address nor an untrusted forwarded host
header is used. The operator must choose an origin reachable from the client.

Profile storage defaults to `%LOCALAPPDATA%\ZippedServing\profiles.json` for the
server account (`XDG_CONFIG_HOME`, then `HOME\.config` on non-Windows systems).
`--profiles-file` overrides it. Keeping it outside the served tree avoids
including workstation paths in downloads or permitting the upload API to
replace configuration. The profiles endpoint creates the parent on first use
and rejects storage within the canonical served root or linked/reparse-point
data/lock files. The local configuration directory must remain trusted against
hostile local filesystem mutations.

Every read or mutation takes an OS file lock on an adjacent stable `.lock` file,
loads and validates a versioned JSON document, and releases the lock on return.
There is no stale per-process cache. Mutations enforce the 99 limit and a global
revision under the same lock, so concurrent browsers/processes cannot exceed
the cap or overwrite unseen edits. A busy lock returns 503; stale revisions and
duplicate names return 409. A new revision is returned only after writing and
syncing a temporary file and atomically replacing the JSON file. Failed writes
are not reflected as successful in-memory state. The schema is bounded to
1 MiB, individual paths to 4096 UTF-8 bytes, names to 100 bytes, and HTTP JSON
mutation bodies to 32 KiB. Bad existing storage is an explicit error, never an
automatic reset. JSON was chosen over a database for this small bounded set.
The directory entry itself is not fsynced, so this is not a power-loss-proof
transaction log; back up the JSON if the profiles matter.

These settings are shared, not private user accounts. No credentials are
stored, and the server still has no authentication: all reachable clients can
read or mutate profiles with the existing custom write-header safeguard.
Generated commands are previews, never executed by the web server. An operator
must inspect shared executable paths before running them. Profile CRUD deletes
configuration entries only; it does not delete served files or client files.
The chosen profile is page-local, whereas profile contents survive browser and
server restarts. Clipboard support uses the secure-context API when available,
then selection-based copying; failure leaves the command selected with explicit
manual-copy instructions for HTTP LAN browsers.

## Compression choice

| Option | Strength | Cost / reason not chosen |
| --- | --- | --- |
| LZ4 fast | Very low compression/decompression CPU cost; standard frames; independent blocks parallelize naturally | Larger output than zstd, especially across block boundaries |
| Snappy | Low CPU cost; useful within existing Snappy ecosystems | Less convenient end-user archive tooling; no clear advantage here over LZ4 |
| zstd low/negative level | Better size/speed tradeoff for bandwidth-constrained networks; standard multithreaded encoder | More CPU and working memory; more tuning needed for storage-speed workloads |

The default is **LZ4 FAST(1)**. The native codec is reused; only the small standard
frame wrapper is implemented here to parallelize compression without a
whole-file buffer. An existing serial frame encoder would be simpler but can
make one CPU core the storage-throughput bottleneck. The native client can also select zstd level 1, favoring a smaller transfer
at higher CPU cost. The browser/API retain LZ4 defaults. Zstd encoding is
serial in this implementation; the LZ4 worker setting does not parallelize it.

Frames use magic `0x184D2204`, version 1, independent 4 MiB blocks, block
checksums, and no known content size. Each compressed block uses the standard
LZ4 block codec. If compression is not smaller, the block is stored raw.
XXH32 protects the descriptor and individual encoded blocks. There is no
whole-frame content checksum; checksums detect accidental corruption, not
malicious alteration. The frame ends only after successful production.
Tests decode frames through liblz4, including empty input, multiple blocks,
incompressible blocks, and out-of-order worker completion potential. A separate
PowerShell smoke decoder exercises the released executable's format.

References: [LZ4 frame specification](https://github.com/lz4/lz4/blob/dev/doc/lz4_Frame_format.md),
[LZ4 project](https://github.com/lz4/lz4),
[zstd project](https://github.com/facebook/zstd),
[Snappy project](https://github.com/google/snappy).
Upstream benchmark numbers are not measurements of this server.

## Conventional archive pipeline and backpressure

```text
regular file -----------+
                        +--> 4 MiB independent blocks --> shared LZ4 workers
directory --> tar stream+                                  |
                                                           v
HTTP client <-- bounded channel <-- ordered frame writer <-+
```

Blocking filesystem/tar work runs outside Tokio's async executor. Each download
has at most `compression_threads` outstanding block results. Workers return
results independently, but the producer writes them in source order. A full
work window waits for the oldest block before submitting another. A slow first
block can delay later results: ordered framing is favored over custom indexing.

The frame writer emits 256 KiB chunks into an eight-slot bounded Tokio channel
(2 MiB queued HTTP payload). Backpressure propagates through the frame writer
to block submission and ultimately filesystem reads. Memory scales with
configured concurrency and worker count, not file/archive length. Budget
roughly `2 * workers * 4 MiB` per active compressed download for block work,
plus current input, output, copy buffers, codec allocations, and transport
overhead. This is an allocation estimate, not an enforced process RSS cap.
The shared pool bounds actual active compression threads across downloads;
per-download windows bound queued work. Defaults cap workers at eight and
active downloads at two. Directory listing JSON still scales with the number
of entries in one directory; it is not paginated.

Directory traversal retains one directory iterator per nesting level rather
than collecting the whole tree. Tar preserves directory layout, empty files,
and empty directories. It does not preserve Windows ACLs, alternate streams,
all NTFS metadata, or hard-link identity. Unsupported filenames cause explicit
stream failure; links/reparse points and upload staging files are intentionally
omitted. Tar avoids ZIP's central-directory bookkeeping and its commonly
expected DEFLATE encoding, but `.tar.lz4` is less familiar to Windows users.

No download code creates a file on the server. The tar bytes and compressed
blocks exist only in memory before transmission. OS filesystem cache and
virtual-memory paging are outside that application-level guarantee.
There is no compressed Content-Length, range support, resumability, cache,
or precomputed archive. Raw downloads use the same bounded response path.
OS sendfile-style zero-copy is not used: compression needs user-space bytes,
and one consistent backpressure path simplifies raw fallback.

Disconnects close the channel; producers stop on their next write or traversal
check and release the download slot. A bounded number of already-submitted
compression blocks may finish. A read failure after HTTP 200 has started is
logged and sent as a body error, terminating the response rather than silently
returning a valid archive. Before streaming starts, failures use HTTP error
statuses. Ctrl+C stops accepting new connections and waits for active requests;
there is no forced shutdown timeout.

## Browser uploads and consistency

The browser sends a raw `PUT` body for each file, not a buffered multipart
form. The server enforces size both from Content-Length when present and
from actual streamed bytes. A semaphore limits concurrent uploads separately
from downloads; excess transfers fail with 503 rather than queue indefinitely.
There is no global disk quota or per-client rate limit. A slow client can
occupy a transfer slot, so use a reverse proxy with timeouts for wider exposure.

Each upload writes an exclusively-created `.zfs-upload-*.part` staging file
alongside its destination. Successful receipt flushes and synchronizes the file
before `persist_noclobber` publishes it without overwriting an existing path.
This costs an fsync and temporarily requires the full uploaded file's disk
space, but does not require a second content copy or cross-volume rename.
Normal error/cancellation cleanup removes staging files; process crashes can
leave them for explicit operator cleanup. Upload staging is deliberately
separate from the prohibition on intermediate compression output.

Archives are live views, not snapshots. Files can change, disappear, or be
uploaded while traversal is running; this may produce a mixed-time view or
abort the download. For coherent backups, quiesce writers or serve an externally
created filesystem snapshot. The utility does not create VSS snapshots.

## Negotiated chunk protocol (native client)

Native transfers use a manifest and independent HTTP file-chunk requests
instead of splitting an opaque tar archive. This keeps source chunks seekable
and retryable without replaying tar from the beginning, caching compressed data
on disk, or retaining a large in-memory archive. A directory manifest describes
the tree (including empty directories) and paths inside one top-level directory.
The result on disk is the same layout as extracting the conventional archive.
The tradeoff is one manifest and more HTTP requests; many tiny files are less
efficient than one tar stream. The browser's existing transfers remain compatible.

The client proposes the codec and raw `chunk_size`; the server replies with
`min(requested, server maximum)`, normally **268435456 bytes (256 MiB)**.
Supported sizes are 1 MiB through 1 GiB. Negotiation declares protocol
`zfs-chunks-v1` and hash `xxh3-128`; the client rejects mismatches. Each file has
offsets 0, split, 2*split, etc. with a short last chunk. Empty files need no
data requests. Manifests are capped at 100000 entries and 16 MiB of serialized
JSON; very large trees must be transferred in subdirectories.

A download manifest records source size and modification time. Each chunk
checks them before opening its range and after encoding. This catches normal
concurrent changes, but a same-size change with a deliberately preserved
timestamp can escape detection. This is not a snapshot or cryptographic source
identity. Source directories should be quiescent during a transfer.

Each HTTP chunk has the following binary envelope, independent of HTTP's own
transfer encoding:

| Field | Encoding |
| --- | --- |
| Magic | Four ASCII bytes `ZFC1` |
| Codec | One byte: 1 = LZ4, 2 = zstd |
| File offset | u64 little endian |
| Uncompressed length | u64 little endian |
| Compressed records | Repeated u32 little-endian size followed by 1-262144 bytes |
| End of compressed records | Zero u32 |
| Integrity footer | XXH3-128 of raw bytes, 16 bytes big endian |

Concatenating the record payloads yields exactly one independently compressed
frame. Record framing lets the receiver find the hash footer without knowing
the compressed length beforehand or relying on poorly supported HTTP trailers.
The decompressor receives an EOF at the record terminator, not the integrity
footer. Header values, decoded length, footer, and end of HTTP body are checked.
Encoded data has a conservative size budget to reject pathological inputs.
Decoders consume 256 KiB copy buffers plus codec state; chunks never become
256 MiB allocations. LZ4 uses up to 4 MiB codec blocks and zstd windows are
limited to 8 MiB. Zstd frames requiring a larger window are explicitly rejected.
LZ4 uploads default to two compression workers on the client; an eight-thread
server pool is still shared across active downloads.

The client streams verified output into a new adjacent staging directory.
It can write a chunk before the footer arrives, but never publishes that
directory until all chunks verify. Retry truncates the staged file back to its
last verified offset. There are at most three attempts per download chunk.
After completion, files are flushed/synchronized and the directory is renamed
to the requested, previously nonexistent destination. This protects existing
output from corrupt/partial transfers without duplicating decompressed file
contents. Crashes can leave staging directories for operator cleanup.

Chunk uploads create an opaque UUID session with an exclusive upload staging
file and one upload semaphore permit. Each PUT must match the next offset;
decoding and XXH3-128 validation happen in a blocking worker via a streaming
async-to-sync bridge. A bad chunk truncates the file back to the previous
verified offset and returns 422. A good chunk returns its hash and next offset;
the client checks both. Finalization requires the full declared size, syncs
the file, and publishes with no-clobber semantics. Competing operations on one
session return 503 instead of racing. Sessions count against `--max-uploads`
until completion/cancellation; abandoned idle sessions are reaped after 15
minutes when subsequent session operations run. There is no persisted resume
journal or automatic replay of uploads after an ambiguous network failure.
Directory uploads are file-transactional, not tree-transactional.

### Integrity algorithm choice

XXH3-128 provides a fast, incremental, non-cryptographic digest with a much wider
collision space than CRC32 or XXH32. It is computed over the same raw copy
buffers already passing through compression/decompression; no reread or
whole-file prehash is required. Verified offset, raw length, file metadata, and
per-chunk digest together detect ordinary data corruption, missing chunks,
wrong ordering, and incomplete files. No separate full-file hash is advertised.

CRC32C can be extremely cheap on hardware with matching instructions but has
only 32 bits and throughput varies by implementation. XXH3-64 is similarly fast
but has less collision headroom. BLAKE3 is a good choice for cryptographic
integrity with SIMD/parallelism, but requires more work than a non-cryptographic
transfer checksum. SHA-256 is broadly interoperable but not the preferred
minimal-CPU hot-path option here. XXH3-128 prioritizes throughput and accidental
corruption detection; it does not authenticate data and is not a substitute
for TLS or a signature. Codec block/frame checksums are retained for
interoperability, so their small additional memory pass remains.

No universally lowest CPU hash or non-bottleneck claim is made without a
hardware/data-specific benchmark. The chosen algorithm is intended to keep
hashing cheaper than the codecs; the CI checks correctness and bounded memory,
not a measured hashing-versus-codec throughput threshold. See the
[xxHash project](https://github.com/Cyan4973/xxHash) for algorithm details.

### Direct archive compatibility

The same client can stream conventional `tar.lz4`, `tar.zst`, and `tar.zstd`
URLs. Codec detection uses frame magic. It decodes directly into a tar parser
and staged files without saving compressed input or a tar intermediate.
It drains past tar's end marker so late decoder/checksum failures cannot be
mistaken for a successful extraction. Output paths are checked against Windows
path traversal and reserved-name rules; symbolic/hard links, special entries,
duplicate files, and nonzero data after tar end are rejected. A decompressed
byte budget and entry-count cap limit extraction. A streaming physical-header
guard caps tar metadata extension records at 1 MiB before the tar parser can
buffer them; regular file content remains streamed. The local staging parent must
be trusted against hostile local filesystem mutation, just as the server root.
HTTP redirects are disabled; use the final target URL directly.

Direct archives are compatibility mode, not the chunk protocol: they have
no XXH3 footer, independent retries, or negotiated splitting. Frame checksums
are checked when present. A failed archive transfer is restarted from the
beginning rather than transparently downgrading verification.

## Filesystem and network trust boundary

The root is canonicalized at startup. Request paths reject absolute paths,
parent/dot components, backslashes, alternate-stream colons, invalid Windows
characters, DOS devices, trailing dots/spaces, and reserved upload names.
Each existing path component is checked for links/reparse points. Listings
mark inaccessible kinds as blocked, and recursive archives do not follow links.
RFC 5987 attachment filenames preserve Unicode with an ASCII fallback.
Browser filenames are inserted as text, not HTML.

These path checks defend against HTTP path traversal, **not a hostile local
filesystem writer**. There is a check/open race if a local user can replace
checked directories with junctions, and ordinary hard links can expose data
already linked into the root. Keep the serving tree under trusted local control
and run with a least-privilege OS account. Fully race-resistant confinement
would require handle-relative Windows opens and final-path verification for
every operation, which is outside this trusted-folder utility's scope.

There is intentionally no login, TLS, CORS, or internet-facing access policy.
The required custom write header blocks simple cross-origin browser writes,
but is not authentication and does not defend against DNS rebinding or native
clients. CSP, nosniff, no-store, and attachment responses reduce browser content
risks. Loopback binding or a trusted firewall is necessary when unrestricted
LAN access is not desired. A production internet deployment needs an
authenticated TLS reverse proxy, host validation, timeouts, and access controls.

## Throughput and measurement

Application throughput is bounded by the slowest of storage reads, tar/metadata
work, block compression, serial checksumming/framing, memory copies, network,
and the receiver. Typical NVMe sequential bandwidth is not a portable fixed
threshold. Parallel LZ4 removes a common single-core limitation, but neither
this architecture nor any codec can guarantee NVMe saturation for all input.
Small-file trees are often metadata-bound; encrypted/media data gains little
from compression. On slower networks, zstd can win overall despite more CPU.

To benchmark, choose a source larger than the filesystem cache, test both
compressible and incompressible data and small-file trees, and distinguish
cold reads from warm-cache runs. Use a receiver/network fast enough not to cap
the measurement. Compare raw to LZ4 at worker counts 1, 2, 4, and 8; record
uncompressed source bytes / wall time, wire bytes, CPU, peak RSS, and disk
throughput. Use curl to write the response to `NUL` when excluding client disk,
then separately verify decoded content and archive entries. Do not call a tiny
loopback correctness test an NVMe benchmark. No throughput result is claimed
without these measurements.

## CI and reproducibility

GitHub Actions on `windows-2025` runs rustfmt and lockfile consistency checks,
Clippy with denied warnings, locked dependency tests, and optimized release
builds for both binaries. Generated formatting/lockfile corrections are
published as a patch artifact on failure, allowing fixes without local tools.
Integration tests
cover uploads, no-overwrite, paths, limits, aborted bodies, nested archives,
and Windows junctions, plus both chunk codecs, hash corruption, offset
validation, truncation, negotiation, upload rollback, and unsafe archive paths.
Release smoke tests exercise real HTTP requests, the actual native client,
and extraction with Windows tar. A 256 MiB + 17 byte fixture crosses the
default chunk boundary in both transfer directions with both codecs; the
client's measured peak working set must stay below 128 MiB. This threshold
guards against whole-chunk buffering, not all possible workloads or memory
allocators. Artifacts include both executables and SHA-256 checksums and
have a 30-day retention period. Local testing downloads that artifact using
GH_TOKEN without installing Rust, MSVC, Node, Docker, or a codec.

Cargo.lock fixes dependency resolution; the stable Rust channel and hosted
image evolve. This is repeatable CI, not a bit-for-bit reproducible or signed
supply-chain build. Pin a Rust toolchain, action commit SHAs, and a controlled
image if stronger reproducibility is needed. The supplied checksum catches
download corruption but is not independent publisher authentication.
