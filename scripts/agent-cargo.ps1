# Single-shot timed cargo wrapper for agentic workflows.
#
# Cold `cargo test --lib` takes minutes (85MB+ MSVC link, `rusqlite bundled` C
# compile, Tauri build script), so NEVER re-run cargo to collect different
# signals. This script runs ONE cargo command, times it, snapshots sccache
# stats before/after, and tees ALL output to a temp log. Mine the log file
# afterwards instead of invoking cargo again.
#
# Usage from the repo root (PowerShell 5.1 compatible — do NOT use a `--`
# separator, it is unsupported here). Pass the cargo command positionally; if
# the first word is not a valid workdir the script routes it to cargo.
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts/agent-cargo.ps1 test --lib --no-run
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts/agent-cargo.ps1 nextest run --lib
#
# For pure test iteration with no rebuild, skip cargo entirely and run the
# existing test binary directly:
#   .\src-tauri\target\debug\deps\ducklingbrowser_lib-*.exe <filter>

param(
  [string]$Workdir = "src-tauri",
  [Parameter(ValueFromRemainingArguments = $true)]
  [string[]]$CargoArgs
)

$ErrorActionPreference = "Continue"
$repoRoot = Split-Path -Parent $PSScriptRoot
if ([System.IO.Path]::IsPathRooted($Workdir)) {
  $workPath = $Workdir
} else {
  $workPath = Join-Path $repoRoot $Workdir
}
if (-not (Test-Path -LiteralPath $workPath -PathType Container)) {
  # Caller omitted `--`, so the cargo subcommand bound to -Workdir. Recover:
  # treat it as part of the cargo args and fall back to the default workdir.
  $CargoArgs = @($Workdir) + $CargoArgs
  $workPath = Join-Path $repoRoot "src-tauri"
}

$logDir = Join-Path $env:TEMP "opencode\cargo-logs"
New-Item -ItemType Directory -Path $logDir -Force | Out-Null
$stamp = Get-Date -Format "yyyyMMdd-HHmmss"
$logFile = Join-Path $logDir "cargo-$stamp.log"

function Get-SccacheStats {
  try {
    $out = & sccache --show-stats 2>&1 | Out-String
    if ($LASTEXITCODE -eq 0) { return $out } else { return "sccache unavailable (exit $LASTEXITCODE)" }
  } catch {
    return "sccache unavailable: $($_.Exception.Message)"
  }
}

$header = @(
  "=== agent-cargo ===",
  "time:      $(Get-Date -Format o)",
  "cwd:       $workPath",
  "command:   cargo $($CargoArgs -join ' ')",
  "log:       $logFile",
  "--- sccache before ---"
  (Get-SccacheStats)
  "--- cargo output ---"
)
$header | Set-Content -LiteralPath $logFile -Encoding utf8

$sw = [System.Diagnostics.Stopwatch]::StartNew()
Push-Location -LiteralPath $workPath
try {
  & cargo @CargoArgs 2>&1 | Tee-Object -FilePath $logFile -Append | Out-Null
  $exitCode = $LASTEXITCODE
} finally {
  Pop-Location
}
$sw.Stop()
$elapsed = [math]::Round($sw.Elapsed.TotalSeconds, 1)

$footer = @(
  "--- end cargo output ---",
  "exit:      $exitCode",
  "elapsed:   ${elapsed}s",
  "--- sccache after ---"
  (Get-SccacheStats)
)
$footer | Add-Content -LiteralPath $logFile -Encoding utf8

# Some tools (nextest progress output) emit NUL bytes that break log mining
# (Select-String/Read treat the file as binary). Strip them.
$rawBytes = [System.IO.File]::ReadAllBytes($logFile)
if ($rawBytes -contains 0) {
  $cleanBytes = $rawBytes | Where-Object { $_ -ne 0 }
  [System.IO.File]::WriteAllBytes($logFile, [byte[]]$cleanBytes)
}

Write-Host "exit=$exitCode elapsed=${elapsed}s log=$logFile"
Write-Host "--- tail ---"
Get-Content -LiteralPath $logFile -Tail 15
