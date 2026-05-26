# Knapsack one-line installer (Windows):
#   irm https://raw.githubusercontent.com/MerlijnW70/knapsack/main/install.ps1 | iex
#
# Downloads the release binary, verifies its SHA-256, installs to %USERPROFILE%\.knapsack\bin,
# and wires the Claude Code hook + MCP server via `knapsack install`.
#
# Designed to fail loud + actionable, never silent. Network blips, BOM-prefixed configs,
# wrong architecture, a Claude Code process holding the old binary open — every one of
# these has explicit handling. Overrides: $env:KNAPSACK_VERSION  $env:KNAPSACK_REPO
# $env:KNAPSACK_BASE_URL  $env:KNAPSACK_VERBOSE (set to anything for verbose output).
$ErrorActionPreference = "Stop"
# IWR's default progress bar adds 10x+ overhead on PS 5.1 — silencing makes the
# download finish in seconds instead of minutes on a slow link.
$ProgressPreference = "SilentlyContinue"

$verbose = [bool]$env:KNAPSACK_VERBOSE
function Say { param($msg) Write-Host "knapsack: $msg" }
function Whisper { param($msg) if ($verbose) { Write-Host "  $msg" -ForegroundColor DarkGray } }
function Die {
  param($msg, $hint)
  Write-Host ""
  Write-Host "knapsack install failed:" -ForegroundColor Red
  Write-Host "  $msg" -ForegroundColor Red
  if ($hint) { Write-Host ""; Write-Host "  $hint" -ForegroundColor Yellow }
  Write-Host ""
  exit 1
}

# --- 1. detect platform -------------------------------------------------------
if (-not [Environment]::Is64BitOperatingSystem) {
  Die "32-bit Windows is not supported." "Knapsack ships x86_64 and aarch64 builds only."
}
# WOW64 fix: a 32-bit PowerShell on 64-bit Windows reports PROCESSOR_ARCHITECTURE=x86,
# which would otherwise make us download the wrong asset. PROCESSOR_ARCHITEW6432 is
# set only when running under WOW64 and carries the host architecture.
$archSrc = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
$cpu = if ($archSrc -eq "ARM64") { "aarch64" } else { "x86_64" }
Whisper "platform: $cpu-pc-windows-msvc (detected from $archSrc)"

# --- 2. resolve download URL --------------------------------------------------
$repo    = if ($env:KNAPSACK_REPO) { $env:KNAPSACK_REPO } else { "MerlijnW70/knapsack" }
$version = if ($env:KNAPSACK_VERSION) { $env:KNAPSACK_VERSION } else { "latest" }
$base    = if ($env:KNAPSACK_BASE_URL) { $env:KNAPSACK_BASE_URL } else { "https://github.com/$repo/releases" }
$asset = "knapsack-$cpu-pc-windows-msvc.zip"
$url = if ($version -eq "latest") { "$base/latest/download/$asset" } else { "$base/download/$version/$asset" }

# --- 3. download with retry ---------------------------------------------------
# Wraps Invoke-WebRequest in an exponential-backoff retry. A single Wi-Fi blip or
# brief GitHub 503 would otherwise abort the install and leave the user to figure
# out it was transient. A real User-Agent header avoids GitHub's stricter rate
# limits on the default PS Invoke-WebRequest UA.
function Fetch {
  param([string]$src, [string]$dst)
  $attempts = 3
  for ($i = 1; $i -le $attempts; $i++) {
    try {
      Invoke-WebRequest -Uri $src -OutFile $dst -UserAgent "knapsack-installer/0.1" -UseBasicParsing
      return
    } catch {
      if ($i -eq $attempts) { throw }
      $wait = [Math]::Pow(2, $i)
      Whisper "download attempt $i/$attempts failed; retrying in ${wait}s ($($_.Exception.Message))"
      Start-Sleep -Seconds $wait
    }
  }
}

$tmp = New-Item -ItemType Directory -Path (Join-Path $env:TEMP ("knapsack-" + [guid]::NewGuid()))
try {
  Say "downloading $asset"
  try {
    Fetch $url          (Join-Path $tmp $asset)
    Fetch "$url.sha256" (Join-Path $tmp "$asset.sha256")
  } catch {
    Die "couldn't download from $url" "Check your internet connection. If you're behind a proxy or firewall, set HTTPS_PROXY and re-run."
  }

  # --- 4. verify checksum -----------------------------------------------------
  Whisper "verifying SHA-256"
  $expected = ((Get-Content (Join-Path $tmp "$asset.sha256")) -split '\s+')[0].Trim().ToLower()
  $actual   = (Get-FileHash (Join-Path $tmp $asset) -Algorithm SHA256).Hash.ToLower()
  if ($actual -ne $expected) {
    Die "checksum mismatch — the download is corrupt or has been tampered with" "Expected: $expected`n  Got:      $actual`n  Re-run the installer to retry; if it persists, file an issue."
  }

  # --- 5. unpack and install --------------------------------------------------
  Expand-Archive -Path (Join-Path $tmp $asset) -DestinationPath $tmp -Force
  $dest = Join-Path $env:USERPROFILE ".knapsack\bin"
  New-Item -ItemType Directory -Force -Path $dest | Out-Null
  $bin = Join-Path $dest "knapsack.exe"

  # Detect a running knapsack.exe (the MCP server held open by Claude Code) and
  # rename-out-of-the-way before copying. Windows won't overwrite a running exe
  # but DOES allow renaming one — the in-memory image keeps working. The stale
  # file is left as `.old` for the user to clean up (or it'll go on next reboot).
  if (Test-Path $bin) {
    try {
      $stale = "$bin.old-$([guid]::NewGuid().ToString().Substring(0,8))"
      Rename-Item -Path $bin -NewName (Split-Path -Leaf $stale) -ErrorAction Stop
      Whisper "moved old knapsack.exe -> $(Split-Path -Leaf $stale) (safe to delete; will go on next reboot)"
    } catch {
      Die "couldn't replace the existing knapsack.exe at $bin" "Close any running Claude Code sessions (and any shells running ``knapsack``) and re-run the installer."
    }
  }
  Copy-Item (Join-Path $tmp "knapsack.exe") $bin -Force

  # --- 6. silent PATH update (the hook/MCP use absolute paths, so PATH is only
  # needed for ``knapsack`` in a shell — not for the product to work). We add it
  # idempotently and only mention it in verbose mode so the success surface stays
  # one line.
  $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
  if (($userPath -split ';') -notcontains $dest) {
    [Environment]::SetEnvironmentVariable("Path", "$dest;$userPath", "User")
    Whisper "added $dest to user PATH (open a new shell to use ``knapsack``)"
  }

  # --- 7. wire hook + MCP into Claude Code -----------------------------------
  # The Rust ``install`` subcommand exits non-zero if any patch failed, so a
  # broken config (BOM, malformed JSON, permission denied) propagates out as a
  # real script failure instead of silently leaving Claude Code unwired.
  & $bin install
  if ($LASTEXITCODE -ne 0) {
    Die "knapsack downloaded ok but couldn't wire into Claude Code (see message above)" "Run ``$bin doctor`` to see exactly what failed. Common cause: settings.json with a UTF-8 BOM or trailing commas."
  }

  Write-Host ""
  Write-Host "Knapsack installed. " -NoNewline -ForegroundColor Green
  Write-Host "Restart Claude Code to load it." -ForegroundColor White
  Write-Host ""
}
finally {
  Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
