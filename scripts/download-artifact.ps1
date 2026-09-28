param(
    [long]$RunId = 0,
    [string]$Destination = '.\artifacts\windows-x64'
)
$ErrorActionPreference = 'Stop'
if (-not $env:GH_TOKEN) { throw 'Set GH_TOKEN to a token with Actions read access to kchan345/ZippedServing.' }
$headers = @{
    Authorization = "Bearer $env:GH_TOKEN"
    Accept = 'application/vnd.github+json'
    'X-GitHub-Api-Version' = '2022-11-28'
}
$api = 'https://api.github.com/repos/kchan345/ZippedServing'
if ($RunId -eq 0) {
    $runs = Invoke-RestMethod "$api/actions/workflows/windows.yml/runs?branch=main&status=success&per_page=1" -Headers $headers
    if ($runs.workflow_runs.Count -eq 0) { throw 'No successful Windows build is available.' }
    $RunId = $runs.workflow_runs[0].id
}
$run = Invoke-RestMethod "$api/actions/runs/$RunId" -Headers $headers
if ($run.conclusion -ne 'success' -or $run.head_repository.full_name -ne 'kchan345/ZippedServing') {
    throw 'Only successful builds from the requested repository can be downloaded.'
}
$result = Invoke-RestMethod "$api/actions/runs/$RunId/artifacts?per_page=100" -Headers $headers
$artifacts = @($result.artifacts | Where-Object { $_.name -eq 'zipped-file-serving-windows-x64' -and -not $_.expired })
if ($artifacts.Count -ne 1) { throw 'Expected one unexpired Windows x64 artifact.' }
New-Item -ItemType Directory -Path $Destination -Force | Out-Null
$destinationPath = (Resolve-Path $Destination).Path
$zip = Join-Path $destinationPath 'build.zip'
Invoke-WebRequest $artifacts[0].archive_download_url -Headers $headers -OutFile $zip
Expand-Archive -LiteralPath $zip -DestinationPath $destinationPath -Force
foreach ($name in @('zipped-file-serving.exe', 'zipped-file-client.exe')) {
    $exe = Join-Path $destinationPath $name
    $lines = @(Get-Content (Join-Path $destinationPath 'SHA256SUMS.txt') | Where-Object { $_ -match ('^[0-9a-fA-F]{64}  ' + [regex]::Escape($name) + '$') })
    if ($lines.Count -ne 1) { throw "Missing or duplicate checksum for $name." }
    $expected = ($lines[0] -split '\s+')[0]
    $actual = (Get-FileHash $exe -Algorithm SHA256).Hash
    if ($expected -ine $actual) { throw "Executable SHA-256 checksum mismatch: $name." }
    Write-Output "Verified executable: $exe"
}
Remove-Item -LiteralPath $zip
Write-Output "Downloaded commit $($run.head_sha) from $($run.html_url)"
