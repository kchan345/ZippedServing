param(
    [Parameter(Mandatory = $true)][string]$Executable,
    [int]$Port = 0
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$exe = (Resolve-Path $Executable).Path
$testRoot = Join-Path (Get-Location) ('local-test\smoke-' + [guid]::NewGuid().ToString('N'))
$served = Join-Path $testRoot 'served'
New-Item -ItemType Directory -Path $served -Force | Out-Null
$process = $null
$client = [System.Net.Http.HttpClient]::new()
$client.Timeout = [TimeSpan]::FromSeconds(30)
$success = $false

function Assert-True($Condition, [string]$Message) {
    if (-not $Condition) { throw $Message }
}

function Read-Lz4([byte[]]$Frame) {
    # Independent test decoder for this server's LZ4 frame profile, not production code.
    Assert-True ($Frame.Length -ge 11) 'LZ4 frame is truncated'
    Assert-True ([BitConverter]::ToUInt32($Frame, 0) -eq 0x184D2204) 'Incorrect LZ4 magic'
    Assert-True ($Frame[4] -eq 0x70 -and $Frame[5] -eq 0x70) 'Unexpected LZ4 frame descriptor'
    $position = 7
    $output = [System.IO.MemoryStream]::new()
    try {
        while ($true) {
            Assert-True ($position + 4 -le $Frame.Length) 'Missing LZ4 end marker'
            [uint32]$blockHeader = [BitConverter]::ToUInt32($Frame, $position)
            $position += 4
            if ($blockHeader -eq 0) { break }
            $length = [int]($blockHeader -band 0x7FFFFFFF)
            $end = $position + $length
            Assert-True ($end + 4 -le $Frame.Length) 'Truncated LZ4 block'
            if (($blockHeader -band [uint32]2147483648) -ne 0) {
                $output.Write($Frame, $position, $length)
                $position = $end
            } else {
                $block = [byte[]]::new(4 * 1024 * 1024)
                $written = 0
                while ($position -lt $end) {
                    $token = [int]$Frame[$position++]
                    $literalLength = $token -shr 4
                    if ($literalLength -eq 15) {
                        do {
                            $extension = [int]$Frame[$position++]
                            $literalLength += $extension
                        } while ($extension -eq 255)
                    }
                    Assert-True ($position + $literalLength -le $end) 'Invalid LZ4 literal run'
                    [Array]::Copy($Frame, $position, $block, $written, $literalLength)
                    $position += $literalLength
                    $written += $literalLength
                    if ($position -eq $end) { break }
                    $offset = [int][BitConverter]::ToUInt16($Frame, $position)
                    $position += 2
                    Assert-True ($offset -gt 0 -and $offset -le $written) 'Invalid LZ4 match offset'
                    $matchLength = ($token -band 15) + 4
                    if (($token -band 15) -eq 15) {
                        do {
                            $extension = [int]$Frame[$position++]
                            $matchLength += $extension
                        } while ($extension -eq 255)
                    }
                    Assert-True ($written + $matchLength -le $block.Length) 'LZ4 block exceeds maximum size'
                    for ($i = 0; $i -lt $matchLength; $i++) {
                        $block[$written] = $block[$written - $offset]
                        $written++
                    }
                }
                $output.Write($block, 0, $written)
            }
            $position += 4 # Checksums are verified by the independent liblz4 tests in CI.
        }
        Assert-True ($position -eq $Frame.Length) 'Unexpected bytes after LZ4 frame'
        return ,$output.ToArray()
    } finally { $output.Dispose() }
}

function Assert-Bytes([byte[]]$Actual, [byte[]]$Expected, [string]$Message) {
    Assert-True ([Convert]::ToBase64String($Actual) -ceq [Convert]::ToBase64String($Expected)) $Message
}

try {
    $payload = [Text.Encoding]::UTF8.GetBytes(('streaming file server 0123456789' * 2048))
    $random = [byte[]]::new(65536)
    [System.Security.Cryptography.RandomNumberGenerator]::Fill($random)
    [IO.File]::WriteAllBytes((Join-Path $served 'sample.txt'), $payload)
    [IO.File]::WriteAllBytes((Join-Path $served 'random.bin'), $random)
    [IO.File]::WriteAllBytes((Join-Path $served 'empty.bin'), [byte[]]::new(0))
    New-Item -ItemType Directory -Path (Join-Path $served 'empty-folder') | Out-Null
    $stdout = Join-Path $testRoot 'stdout.log'
    $stderr = Join-Path $testRoot 'stderr.log'
    $process = Start-Process -FilePath $exe -ArgumentList @("`"$served`"", '--bind', '127.0.0.1', '--port', "$Port") -PassThru -RedirectStandardOutput $stdout -RedirectStandardError $stderr
    $base = $null
    for ($attempt = 0; $attempt -lt 100; $attempt++) {
        if ($process.HasExited) { throw "Server exited: $(Get-Content $stderr -Raw)" }
        $log = Get-Content $stdout -Raw
        if ($log -match '127\.0\.0\.1:(\d+)') {
            $base = "http://127.0.0.1:$($Matches[1])"
            break
        }
        Start-Sleep -Milliseconds 100
    }
    Assert-True ($null -ne $base) 'Server did not report its listening address'
    $html = $client.GetStringAsync("$base/").GetAwaiter().GetResult()
    Assert-True ($html.Contains('Local file server')) 'Embedded UI is missing'
    foreach ($asset in @('app.js', 'style.css')) {
        Assert-True ($client.GetStringAsync("$base/$asset").GetAwaiter().GetResult().Length -gt 0) "Missing $asset"
    }
    foreach ($name in @('sample.txt', 'random.bin', 'empty.bin')) {
        $expected = [IO.File]::ReadAllBytes((Join-Path $served $name))
        $raw = $client.GetByteArrayAsync("$base/api/download?path=$name&format=raw").GetAwaiter().GetResult()
        Assert-Bytes $raw $expected "Raw download mismatch: $name"
        $compressed = $client.GetByteArrayAsync("$base/api/download?path=$name").GetAwaiter().GetResult()
        Assert-Bytes (Read-Lz4 $compressed) $expected "Compressed download mismatch: $name"
    }
    $request = [System.Net.Http.HttpRequestMessage]::new([System.Net.Http.HttpMethod]::Put, "$base/api/upload?path=uploaded.txt")
    $request.Headers.Add('X-Requested-With', 'zipped-file-serving')
    $request.Content = [System.Net.Http.ByteArrayContent]::new($payload)
    $response = $client.SendAsync($request).GetAwaiter().GetResult()
    Assert-True ([int]$response.StatusCode -eq 201) 'Upload failed'
    $response.Dispose()
    $request.Dispose()
    Assert-Bytes ([IO.File]::ReadAllBytes((Join-Path $served 'uploaded.txt'))) $payload 'Upload changed bytes'
    $response = $client.GetAsync("$base/api/list?path=..%2Foutside").GetAwaiter().GetResult()
    Assert-True ([int]$response.StatusCode -eq 400) 'Traversal was not rejected'
    $response.Dispose()
    $before = @(Get-ChildItem $served -Force -Recurse | ForEach-Object { $_.FullName })
    $archive = $client.GetByteArrayAsync("$base/api/download").GetAwaiter().GetResult()
    $tarPath = Join-Path $testRoot 'download.tar'
    [IO.File]::WriteAllBytes($tarPath, (Read-Lz4 $archive))
    $extracted = Join-Path $testRoot 'extracted'
    New-Item -ItemType Directory -Path $extracted | Out-Null
    & tar.exe -xf $tarPath -C $extracted
    Assert-True ($LASTEXITCODE -eq 0) 'Windows tar could not extract the directory download'
    Assert-Bytes ([IO.File]::ReadAllBytes((Join-Path $extracted 'served\sample.txt'))) $payload 'Archive changed file bytes'
    Assert-Bytes ([IO.File]::ReadAllBytes((Join-Path $extracted 'served\random.bin'))) $random 'Archive changed random bytes'
    Assert-True (Test-Path (Join-Path $extracted 'served\empty-folder') -PathType Container) 'Archive lost empty folder'
    $after = @(Get-ChildItem $served -Force -Recurse | ForEach-Object { $_.FullName })
    Assert-True (-not (Compare-Object $before $after)) 'Download created files in the served directory'
    Write-Output "PASS: Windows executable, embedded UI, uploads, raw/LZ4 downloads, tar extraction, and path rejection ($base)."
    $success = $true
} finally {
    $client.Dispose()
    if ($null -ne $process -and -not $process.HasExited) {
        Stop-Process -Id $process.Id
        $process.WaitForExit()
    }
    if ($success) { Remove-Item -LiteralPath $testRoot -Recurse -Force }
    else { Write-Warning "Smoke-test diagnostics retained at $testRoot" }
}
