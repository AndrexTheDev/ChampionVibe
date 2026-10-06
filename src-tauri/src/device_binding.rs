use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

/// Hashes a native platform identifier with an application-specific domain
/// separator. The raw value is never persisted or sent to the issuer.
pub fn binding_from_identifier(platform_id: &str) -> Result<String> {
    let platform_id = platform_id.trim();
    if platform_id.is_empty() {
        bail!("the operating system did not provide a device identifier");
    }

    let mut digest = Sha256::new();
    digest.update(b"ChampionVibe/device-binding/v1\0");
    digest.update(platform_id.to_ascii_lowercase().as_bytes());
    Ok(hex::encode(digest.finalize()))
}

#[cfg(any(target_os = "windows", target_os = "linux"))]
pub fn device_binding() -> Result<String> {
    binding_from_identifier(&read_platform_identifier()?)
}

#[cfg(target_os = "android")]
pub fn device_binding() -> Result<String> {
    bail!("Android identity must be read through the native Tauri device plugin")
}

#[cfg(target_os = "windows")]
fn read_platform_identifier() -> Result<String> {
    use std::process::{Command, Stdio};

    // Use fixed PowerShell/WMI commands (no user-supplied command fragments) to
    // read the motherboard serial and CPU ProcessorId required by the license
    // binding. The raw identifiers are immediately hashed by the caller.
    let script = r#"
$ErrorActionPreference = 'Stop'
$board = Get-CimInstance -ClassName Win32_BaseBoard | Select-Object -First 1 -ExpandProperty SerialNumber
$cpu = Get-CimInstance -ClassName Win32_Processor | Select-Object -First 1 -ExpandProperty ProcessorId
if ([string]::IsNullOrWhiteSpace($board) -or [string]::IsNullOrWhiteSpace($cpu)) { exit 3 }
[Console]::Out.WriteLine($board.Trim())
[Console]::Out.WriteLine($cpu.Trim())
"#;
    let output = Command::new("powershell.exe")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            script,
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .context("could not query Windows motherboard and CPU identifiers through WMI")?;
    if !output.status.success() {
        bail!("Windows WMI did not return motherboard and CPU identifiers");
    }

    let output = String::from_utf8(output.stdout)
        .context("Windows returned non-UTF-8 hardware identifiers")?;
    let mut lines = output.lines().map(str::trim);
    let board = lines.next().filter(|value| !value.is_empty());
    let cpu = lines.next().filter(|value| !value.is_empty());
    let (Some(board), Some(cpu)) = (board, cpu) else {
        bail!("Windows WMI returned incomplete hardware identifiers");
    };

    let is_placeholder = |value: &str| {
        matches!(
            value.to_ascii_lowercase().as_str(),
            "default string"
                | "to be filled by o.e.m."
                | "system serial number"
                | "unknown"
                | "none"
                | "not specified"
        ) || value
            .chars()
            .all(|character| character == '0' || character == '-')
    };
    if is_placeholder(board) || is_placeholder(cpu) {
        bail!("Windows firmware did not expose usable motherboard and CPU identifiers");
    }

    Ok(format!("motherboard:{board}|cpu:{cpu}"))
}

// Linux is a development-only fallback that permits local tests and previews.
#[cfg(target_os = "linux")]
fn read_platform_identifier() -> Result<String> {
    std::fs::read_to_string("/etc/machine-id")
        .or_else(|_| std::fs::read_to_string("/var/lib/dbus/machine-id"))
        .context("no local machine id was found (development fallback)")
}

#[cfg(not(any(target_os = "windows", target_os = "android", target_os = "linux")))]
fn read_platform_identifier() -> Result<String> {
    bail!("ChampionVibe device binding is implemented for Windows and Android")
}

#[cfg(test)]
mod tests {
    use super::binding_from_identifier;
    use sha2::{Digest, Sha256};

    #[test]
    fn binding_is_stable_and_application_scoped() {
        let first = binding_from_identifier(" Example-Platform-ID ").unwrap();
        let repeated = binding_from_identifier("example-platform-id").unwrap();
        assert_eq!(first, repeated);
        assert_eq!(first.len(), 64);

        let mut other_domain = Sha256::new();
        other_domain.update(b"other-app/v1\0");
        other_domain.update(b"example-platform-id");
        assert_ne!(first, hex::encode(other_domain.finalize()));
    }
}
