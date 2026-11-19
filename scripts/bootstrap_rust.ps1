<#
.SYNOPSIS
    Bootstraps the Rust toolchain ChampionVibe needs on a Windows or cross-build machine.

.DESCRIPTION
    Installs the pieces a fresh machine needs to build and test this repository the way CI and the
    release pipeline do:

      * the Rust toolchain (cargo, rustc, rustfmt) through rustup,
      * the release targets x86_64-pc-windows-msvc and aarch64-linux-android,
      * the workspace tests with the checked-in Cargo.lock (`cargo test --workspace --locked`).

    Only $env:RUSTUP_HOME, $env:CARGO_HOME, the repository's target directory and a temporary
    rustup-init.exe download are touched. The script is safe to re-run: every step checks what is
    already installed first.

    Adding the Android target is not the same as building for Android: an aarch64-linux-android
    link additionally needs the Android NDK (`ANDROID_NDK_HOME` or `NDK_HOME`) and the tauri CLI.

.PARAMETER Toolchain
    Toolchain channel to install. Defaults to $env:BOOTSTRAP_RUST_TOOLCHAIN, or `stable`.

.PARAMETER SkipTargets
    Do not add the Windows MSVC and Android targets.

.PARAMETER SkipTests
    Do not run `cargo test --workspace --locked`.

.PARAMETER DryRun
    Print the commands instead of running them.

.EXAMPLE
    pwsh -File scripts/bootstrap_rust.ps1

.EXAMPLE
    pwsh -File scripts/bootstrap_rust.ps1 -Toolchain stable -SkipTests
#>
[CmdletBinding()]
param(
    [string]$Toolchain = $(if ($env:BOOTSTRAP_RUST_TOOLCHAIN) { $env:BOOTSTRAP_RUST_TOOLCHAIN } else { "stable" }),
    [switch]$SkipTargets,
    [switch]$SkipTests,
    [switch]$DryRun
)

$ErrorActionPreference = "Stop"

$WindowsTarget = "x86_64-pc-windows-msvc"
$AndroidTarget = "aarch64-linux-android"
$RustupInitUrl = "https://win.rustup.rs/x86_64"
$CargoBinDirectory = Join-Path $env:USERPROFILE ".cargo\bin"

function Write-Say {
    param([Parameter(Mandatory = $true)][string]$Message)
    Write-Host "==> $Message" -ForegroundColor Cyan
}

function Write-Warn {
    param([Parameter(Mandatory = $true)][string]$Message)
    Write-Warning $Message
}

function Test-Command {
    param([Parameter(Mandatory = $true)][string]$Name)
    return [bool](Get-Command $Name -ErrorAction SilentlyContinue)
}

function Invoke-Step {
    param(
        [Parameter(Mandatory = $true)][string]$Exe,
        [string[]]$Arguments = @()
    )
    $rendered = "$Exe $($Arguments -join ' ')"
    Write-Host "  `$ $rendered"
    if ($DryRun) { return }
    & $Exe @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "$rendered failed with exit code $LASTEXITCODE"
    }
}

function Set-RepositoryRoot {
    $root = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
    Set-Location $root
    if (-not (Test-Path (Join-Path $root "Cargo.toml"))) {
        throw "Cargo.toml not found in $root; run this script from a checkout"
    }
    Write-Say "repository: $root"
}

function Add-CargoToPath {
    if ((Test-Path $CargoBinDirectory) -and ($env:Path -notlike "*$CargoBinDirectory*")) {
        $env:Path = "$CargoBinDirectory;$env:Path"
    }
}

function Install-Rustup {
    Add-CargoToPath
    if ((Test-Command "rustup") -and (Test-Command "cargo") -and (Test-Command "rustc")) {
        Write-Say "rustup, cargo and rustc are already installed"
        return
    }
    if ((Test-Command "cargo") -and (Test-Command "rustc") -and -not (Test-Command "rustup")) {
        Write-Warn "found cargo/rustc without rustup; toolchain and target management will be skipped"
        return
    }

    Write-Say "installing rustup and the $Toolchain toolchain"
    $arguments = @("-y", "--default-toolchain", $Toolchain, "--profile", "minimal", "--component", "rustfmt")
    if ($DryRun) {
        Write-Host "  `$ curl.exe -L $RustupInitUrl -o rustup-init.exe"
        Write-Host "  `$ rustup-init.exe $($arguments -join ' ')"
    }
    else {
        $rustupInit = Join-Path $env:TEMP "championvibe-rustup-init.exe"
        Invoke-WebRequest -Uri $RustupInitUrl -OutFile $rustupInit -UseBasicParsing
        & $rustupInit @arguments
        if ($LASTEXITCODE -ne 0) {
            throw "rustup-init failed with exit code $LASTEXITCODE"
        }
        Remove-Item -Force $rustupInit -ErrorAction SilentlyContinue
        Add-CargoToPath
    }

    if (-not $DryRun -and -not (Test-Command "rustup")) {
        throw "rustup is still not on PATH; add $CargoBinDirectory to PATH and re-run"
    }
}

function Install-Toolchain {
    if (-not (Test-Command "rustup")) {
        return
    }
    Write-Say "ensuring the $Toolchain toolchain with rustfmt"
    Invoke-Step -Exe "rustup" -Arguments @("toolchain", "install", $Toolchain, "--profile", "minimal", "--component", "rustfmt")
    if (-not $DryRun) {
        & rustup default | Out-Null
        if ($LASTEXITCODE -ne 0) {
            Invoke-Step -Exe "rustup" -Arguments @("default", $Toolchain)
        }
    }
}

function Install-Targets {
    if ($SkipTargets) {
        Write-Say "skipping the cross targets"
        return
    }
    if (-not (Test-Command "rustup")) {
        Write-Warn "rustup is unavailable, so $WindowsTarget and $AndroidTarget were not added"
        return
    }
    Write-Say "adding the release targets: $WindowsTarget and $AndroidTarget"
    Invoke-Step -Exe "rustup" -Arguments @("target", "add", "--toolchain", $Toolchain, $WindowsTarget, $AndroidTarget)
    if (-not $DryRun) {
        $installed = (& rustup target list --toolchain $Toolchain --installed) -join "`n"
        foreach ($target in @($WindowsTarget, $AndroidTarget)) {
            if ($installed -notmatch [regex]::Escape($target)) {
                Write-Warn "$target is not installed; cross builds for it will fail until it is"
            }
        }
    }
}

function Show-ToolVersions {
    Write-Say "toolchain versions"
    if ($DryRun) { return }
    if (-not (Test-Command "rustfmt")) {
        throw "rustfmt is missing; add it with `rustup component add rustfmt`"
    }
    & cargo --version
    & rustc --version
    & rustfmt --version
}

function Invoke-WorkspaceTests {
    if ($SkipTests) {
        Write-Say "skipping the workspace tests"
        return
    }
    Write-Say "running the workspace tests with the checked-in Cargo.lock"
    $arguments = @("test", "--workspace", "--locked")
    if ($env:BOOTSTRAP_RUST_TEST_ARGS) {
        $arguments += ($env:BOOTSTRAP_RUST_TEST_ARGS -split "\s+" | Where-Object { $_ -ne "" })
    }
    Invoke-Step -Exe "cargo" -Arguments $arguments
}

Set-RepositoryRoot
Install-Rustup
Install-Toolchain
Install-Targets
Show-ToolVersions
Invoke-WorkspaceTests

if ($DryRun) {
    Write-Say "dry run: nothing was installed and no tests were executed"
    exit 0
}
Write-Say "done: cargo, rustc, rustfmt and the release targets are available"
exit 0
