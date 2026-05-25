# Knapsack one-line installer (Windows):
#   irm https://knapsack.dev/install.ps1 | iex
# Downloads the release binary, verifies its checksum, installs to %USERPROFILE%\.knapsack\bin,
# then wires the Claude Code hook + MCP server via `knapsack install --apply`.
# Override: $env:KNAPSACK_VERSION  $env:KNAPSACK_REPO  $env:KNAPSACK_BASE_URL
$ErrorActionPreference = "Stop"

$repo    = if ($env:KNAPSACK_REPO) { $env:KNAPSACK_REPO } else { "knapsack-dev/knapsack" }
$version = if ($env:KNAPSACK_VERSION) { $env:KNAPSACK_VERSION } else { "latest" }
$base    = if ($env:KNAPSACK_BASE_URL) { $env:KNAPSACK_BASE_URL } else { "https://github.com/$repo/releases" }

if (-not [Environment]::Is64BitOperatingSystem) { throw "knapsack: 32-bit Windows is unsupported" }
$cpu = if ($env:PROCESSOR_ARCHITECTURE -eq "ARM64") { "aarch64" } else { "x86_64" }
$asset = "knapsack-$cpu-pc-windows-msvc.zip"
$url = if ($version -eq "latest") { "$base/latest/download/$asset" } else { "$base/download/$version/$asset" }

$tmp = New-Item -ItemType Directory -Path (Join-Path $env:TEMP ("knapsack-" + [guid]::NewGuid()))
try {
  Write-Host "knapsack: downloading $url"
  Invoke-WebRequest -Uri $url          -OutFile (Join-Path $tmp $asset)
  Invoke-WebRequest -Uri "$url.sha256" -OutFile (Join-Path $tmp "$asset.sha256")

  Write-Host "knapsack: verifying checksum"
  $expected = ((Get-Content (Join-Path $tmp "$asset.sha256")) -split '\s+')[0].Trim().ToLower()
  $actual   = (Get-FileHash (Join-Path $tmp $asset) -Algorithm SHA256).Hash.ToLower()
  if ($actual -ne $expected) { throw "knapsack: checksum mismatch (expected $expected, got $actual)" }

  Expand-Archive -Path (Join-Path $tmp $asset) -DestinationPath $tmp -Force
  $dest = Join-Path $env:USERPROFILE ".knapsack\bin"
  New-Item -ItemType Directory -Force -Path $dest | Out-Null
  Copy-Item (Join-Path $tmp "knapsack.exe") (Join-Path $dest "knapsack.exe") -Force
  $bin = Join-Path $dest "knapsack.exe"
  Write-Host "knapsack: installed $bin"

  $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
  if ($userPath -notlike "*$dest*") {
    Write-Host "knapsack: add to PATH ->  setx PATH `"$dest;%PATH%`""
  }

  # Wire the hook + MCP, back up config, smoke test, doctor, print rollback.
  & $bin install --apply
}
finally {
  Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
