# Single-shot timed cargo wrapper for agentic workflows.
#
# Cold `cargo test --lib` takes minutes (85MB+ MSVC link, `rusqlite bundled` C
# compile, Tauri build script), so NEVER re-run cargo to collect different
# signals. This script runs ONE cargo command, times it, and appends ALL output
# to a temp log. Mine the log file afterwards instead of invoking cargo again.
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
#
# Optional env knobs (all default to the fast local path):
#   AGENT_CARGO_SCCACHE=1         include `sccache --show-stats` before/after
#                                 (default: skipped — two extra process spawns
#                                 for information that rarely changes).
#   AGENT_CARGO_NO_INCREMENTAL=1  set CARGO_INCREMENTAL=0 for this invocation
#                                 only. sccache cannot cache incremental builds,
#                                 so a cold build with this set can actually hit
#                                 the shared cache; warm incremental rebuilds get
#                                 slower without it. Restored afterwards.

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
$logFile = Join-Path $logDir "cargo-$stamp-$PID.log"

$wantSccache = $env:AGENT_CARGO_SCCACHE -eq "1"
function Get-SccacheStats {
  if (-not $wantSccache) { return "<skipped (set AGENT_CARGO_SCCACHE=1 to include)>" }
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
# CARGO_INCREMENTAL=0 makes the build cacheable by sccache (which cannot cache
# incremental compilations). Scope it to this invocation and restore afterwards
# so the caller's session is unaffected.
$noIncremental = $env:AGENT_CARGO_NO_INCREMENTAL -eq "1"
$prevIncremental = $env:CARGO_INCREMENTAL
if ($noIncremental) { $env:CARGO_INCREMENTAL = "0" }
Push-Location -LiteralPath $workPath
try {
  # Single consumer (no Tee-Object + Out-Null fan-out): stderr merges into the
  # output stream exactly as before, but without the extra pipeline copies.
  & cargo @CargoArgs 2>&1 | Out-File -FilePath $logFile -Append -Encoding utf8
  $exitCode = $LASTEXITCODE
} finally {
  Pop-Location
  if ($noIncremental) {
    if ($null -eq $prevIncremental) { Remove-Item Env:\CARGO_INCREMENTAL } else { $env:CARGO_INCREMENTAL = $prevIncremental }
  }
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
# (Select-String/Read treat the file as binary). Strip them with a single .NET
# string pass instead of a per-byte PowerShell pipeline.
$logText = [System.IO.File]::ReadAllText($logFile)
if ($logText.Contains("`0")) {
  [System.IO.File]::WriteAllText($logFile, $logText.Replace("`0", ""))
}

Write-Host "exit=$exitCode elapsed=${elapsed}s log=$logFile"
Write-Host "--- tail ---"
Get-Content -LiteralPath $logFile -Tail 15
