param(
    [Parameter(Mandatory = $true)][string]$Server,
    [Parameter(Mandatory = $true)][string]$Client,
    [switch]$Large
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$serverExe = (Resolve-Path $Server).Path
$clientExe = (Resolve-Path $Client).Path
$root = Join-Path (Get-Location) ('local-test\client-' + [guid]::NewGuid().ToString('N'))
$served = Join-Path $root 'served'
$source = Join-Path $served 'source'
New-Item -ItemType Directory -Path (Join-Path $source 'nested\empty') -Force | Out-Null
$process = $null
$success = $false
$script:invocation = 0

function Assert-True($Condition, [string]$Message) {
    if (-not $Condition) { throw $Message }
}

function Assert-File([string]$Expected, [string]$Actual) {
    Assert-True ((Get-FileHash $Expected).Hash -eq (Get-FileHash $Actual).Hash) "File mismatch: $Actual"
}

function Run-Client([string[]]$Arguments, [switch]$Fail, [switch]$BoundMemory) {
    $script:invocation++
    $stdout = Join-Path $root "client-$script:invocation.stdout.log"
    $stderr = Join-Path $root "client-$script:invocation.stderr.log"
    $quoted = @($Arguments | ForEach-Object { '"' + $_ + '"' })
    $child = Start-Process -FilePath $clientExe -ArgumentList $quoted -PassThru -RedirectStandardOutput $stdout -RedirectStandardError $stderr
    # Keep the process handle open so the OS retains peak working-set accounting.
    $null = $child.Handle
    $peak = 0L
    try {
        $deadline = [DateTime]::UtcNow.AddMinutes(5)
        while (-not $child.WaitForExit(25)) {
            $child.Refresh()
            $peak = [Math]::Max($peak, $child.PeakWorkingSet64)
            if ([DateTime]::UtcNow -gt $deadline) { throw 'Client exceeded smoke-test timeout' }
        }
        $child.WaitForExit()
        $peak = [Math]::Max($peak, $child.PeakWorkingSet64)
        $log = Get-Content $stderr -Raw
        if ($Fail) {
            Assert-True ($child.ExitCode -ne 0) 'Client unexpectedly accepted invalid transfer'
        } else {
            Assert-True ($child.ExitCode -eq 0) "Client failed: $log"
        }
        if ($BoundMemory) {
            Assert-True ($peak -gt 0 -and $peak -lt 128MB) "Client peak memory $peak is not below 128 MiB for a 256 MiB chunk"
        }
        Write-Output "Client $($Arguments[0]): exit=$($child.ExitCode), peak=$([Math]::Round($peak / 1MB, 2)) MiB"
    } finally {
        if (-not $child.HasExited) { Stop-Process -Id $child.Id; $child.WaitForExit() }
        $child.Dispose()
    }
}

try {
    $data = [byte[]]::new(3MB + 17)
    [Security.Cryptography.RandomNumberGenerator]::Fill($data)
    [IO.File]::WriteAllBytes((Join-Path $source 'nested\random.bin'), $data)
    [IO.File]::WriteAllText((Join-Path $source 'text.txt'), ('streamed content' * 8192))
    [IO.File]::WriteAllBytes((Join-Path $source 'zero.bin'), [byte[]]::new(0))
    $data = $null
    $stdout = Join-Path $root 'server.stdout.log'
    $stderr = Join-Path $root 'server.stderr.log'
    $process = Start-Process -FilePath $serverExe -ArgumentList @("`"$served`"", '--bind', '127.0.0.1', '--port', '0') -PassThru -RedirectStandardOutput $stdout -RedirectStandardError $stderr
    $base = $null
    for ($i = 0; $i -lt 100; $i++) {
        if ($process.HasExited) { throw "Server exited: $(Get-Content $stderr -Raw)" }
        if ((Get-Content $stdout -Raw) -match '127\.0\.0\.1:(\d+)') {
            $base = "http://127.0.0.1:$($Matches[1])"
            break
        }
        Start-Sleep -Milliseconds 100
    }
    Assert-True ($null -ne $base) 'Server failed to start'
    foreach ($codec in @('lz4', 'zstd')) {
        $destination = Join-Path $root "download-$codec"
        Run-Client @('download', "$base/api/download?path=source", $destination, '--split-mib', '1', '--codec', $codec)
        Assert-File (Join-Path $source 'nested\random.bin') (Join-Path $destination 'source\nested\random.bin')
        Assert-True (Test-Path (Join-Path $destination 'source\nested\empty') -PathType Container) 'Empty directory was lost'
        Assert-True ((Get-Item (Join-Path $destination 'source\zero.bin')).Length -eq 0) 'Empty file was lost'
        Run-Client @('download', "$base/api/download?path=source", $destination) -Fail
        $archiveOutput = Join-Path $root "archive-$codec"
        Run-Client @('download', "$base/api/download?path=source&format=$codec", $archiveOutput, '--archive')
        Assert-File (Join-Path $source 'nested\random.bin') (Join-Path $archiveOutput 'source\nested\random.bin')
        Run-Client @('upload', $source, "$base/api/upload?path=uploaded-$codec", '--split-mib', '1', '--codec', $codec)
        Assert-File (Join-Path $source 'nested\random.bin') (Join-Path $served "uploaded-$codec\nested\random.bin")
        Assert-True (Test-Path (Join-Path $served "uploaded-$codec\nested\empty") -PathType Container) 'Uploaded empty directory was lost'
        Assert-True ((Get-Item (Join-Path $served "uploaded-$codec\zero.bin")).Length -eq 0) 'Uploaded empty file was lost'
        Run-Client @('upload', (Join-Path $source 'text.txt'), "$base/api/upload?path=uploaded-$codec/text.txt") -Fail
        $limited = Join-Path $root "limited-$codec"
        Run-Client @('download', "$base/api/download?path=source&format=$codec", $limited, '--archive', '--max-bytes', '1024') -Fail
        Assert-True (-not (Test-Path $limited)) 'Failed archive extraction published output'
    }
    $missing = Join-Path $root 'missing'
    Run-Client @('download', "$base/api/download?path=missing", $missing) -Fail
    Assert-True (-not (Test-Path $missing)) 'Failed download published output'
    if ($Large) {
        $largeFile = Join-Path $served 'large.bin'
        $stream = [IO.File]::Create($largeFile)
        try {
            $block = [byte[]]::new(1MB)
            for ($i = 0; $i -lt $block.Length; $i++) { $block[$i] = [byte]($i % 251) }
            for ($i = 0; $i -lt 256; $i++) { $stream.Write($block, 0, $block.Length) }
            $stream.Write($block, 0, 17)
        } finally { $stream.Dispose() }
        foreach ($codec in @('lz4', 'zstd')) {
            $largeOutput = Join-Path $root "large-$codec"
            Run-Client @('download', "$base/api/download?path=large.bin", $largeOutput, '--codec', $codec) -BoundMemory
            Assert-File $largeFile (Join-Path $largeOutput 'large.bin')
            Run-Client @('upload', $largeFile, "$base/api/upload?path=large-upload-$codec.bin", '--codec', $codec) -BoundMemory
            Assert-File $largeFile (Join-Path $served "large-upload-$codec.bin")
            Remove-Item -LiteralPath $largeOutput -Recurse -Force
            Remove-Item -LiteralPath (Join-Path $served "large-upload-$codec.bin")
        }
        Write-Output 'PASS: 256 MiB + 17 bytes crosses the default chunk boundary with peak client memory below 128 MiB for both codecs and directions.'
    }
    Assert-True (-not (Get-ChildItem $served -Force -Recurse -Filter '.zfs-upload-*')) 'Staging uploads leaked'
    Assert-True (-not (Get-ChildItem $root -Force -Filter '.zfs-client-*')) 'Client staging directory leaked'
    Write-Output 'PASS: client/server chunk download/upload, both archive codecs, nested/empty files, no-overwrite, limits, and failure cleanup.'
    $success = $true
} finally {
    if ($null -ne $process -and -not $process.HasExited) { Stop-Process -Id $process.Id; $process.WaitForExit() }
    if ($success) { Remove-Item -LiteralPath $root -Recurse -Force }
    else { Write-Warning "Diagnostics retained at $root" }
}
