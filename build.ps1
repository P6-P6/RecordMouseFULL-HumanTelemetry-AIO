<#
    Build HumanTelemetry (RecordMouseFULL-HumanTelemetry-AIO).

    Exists because `cargo build` fails on this machine from a plain shell:
      * MSVC link.exe is not on PATH (VS 18 is an Insiders/prerelease install,
        so even vswhere misses it unless -prerelease is passed).
      * From Git Bash, Git's GNU coreutils `link` shadows MSVC's link.exe and
        the failure masquerades as a compile error ("link: extra operand").

    Usage:  .\build.ps1            # debug
            .\build.ps1 -Release
            .\build.ps1 -Release -Run
#>
param(
    [switch]$Release,
    [switch]$Run,
    [switch]$Check
)

$ErrorActionPreference = "Stop"

function Find-VcVars {
    $vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
    if (Test-Path $vswhere) {
        # -prerelease is required: VS 18 Insiders is invisible without it.
        $root = & $vswhere -products * -prerelease -latest `
                    -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 `
                    -property installationPath 2>$null
        if ($root) {
            $c = Join-Path $root "VC\Auxiliary\Build\vcvars64.bat"
            if (Test-Path $c) { return $c }
        }
    }
    # Fallback: scan the usual roots.
    foreach ($base in @("C:\Program Files\Microsoft Visual Studio",
                        "${env:ProgramFiles(x86)}\Microsoft Visual Studio")) {
        if (-not (Test-Path $base)) { continue }
        $hit = Get-ChildItem $base -Recurse -Filter vcvars64.bat -ErrorAction SilentlyContinue |
               Select-Object -First 1
        if ($hit) { return $hit.FullName }
    }
    throw "vcvars64.bat not found. Install the 'Desktop development with C++' workload."
}

$vcvars = Find-VcVars
Write-Host "[build] vcvars: $vcvars" -ForegroundColor DarkGray

$cmd = if ($Check) { "cargo check" }
       elseif ($Release) { "cargo build --release" }
       else { "cargo build" }

$proj = $PSScriptRoot
cmd /c "`"$vcvars`" >nul 2>&1 && cd /d `"$proj`" && $cmd"
if ($LASTEXITCODE -ne 0) { throw "build failed ($LASTEXITCODE)" }

if ($Check) { Write-Host "[build] check ok" -ForegroundColor Green; return }

# target-dir is redirected to local disk by .cargo/config.toml (never to G:).
$cfg     = Get-Content (Join-Path $proj ".cargo\config.toml") -Raw
$tdir    = ([regex]::Match($cfg, 'target-dir\s*=\s*"([^"]+)"')).Groups[1].Value
$profile = if ($Release) { "release" } else { "debug" }
$exe     = Join-Path $tdir "$profile\HumanTelemetry.exe"

if (Test-Path $exe) {
    $size = "{0:N1} MB" -f ((Get-Item $exe).Length / 1MB)
    Write-Host "[build] ok: $exe ($size)" -ForegroundColor Green
    if ($Release) {
        Copy-Item $exe (Join-Path $proj "HumanTelemetry.exe") -Force
        Write-Host "[build] copied -> HumanTelemetry.exe" -ForegroundColor Green
    }
    if ($Run) { & $exe }
} else {
    throw "expected exe not found at $exe"
}
