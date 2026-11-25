//! Run a downloaded installer and let it replace this app.
//!
//! A small hidden PowerShell script, written next to the installer, waits
//! for this process to exit, runs the setup silently (`/S`, and `/R` so the
//! NSIS installer starts the new version when done), then deletes the
//! installer, itself and the update folder. If the setup fails, it starts
//! the existing app again so the user is never left with nothing running.

use std::path::Path;

/// Spawn the updater. The caller closes the app right after `Ok`.
#[cfg(windows)]
pub fn install_and_restart(installer: &Path) -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const DETACHED_PROCESS: u32 = 0x0000_0008;

    let dir = installer.parent().ok_or_else(|| std::io::Error::other("installer has no folder"))?;
    let exe = std::env::current_exe()?;
    let q = |p: &Path| p.to_string_lossy().replace('\'', "''");
    let script = format!(
        "$ErrorActionPreference = 'SilentlyContinue'\r\n\
         Wait-Process -Id {pid} -Timeout 30\r\n\
         $p = Start-Process -FilePath '{setup}' -ArgumentList '/S','/R' -Wait -PassThru\r\n\
         if (-not $p -or $p.ExitCode -ne 0) {{ Start-Process -FilePath '{exe}' }}\r\n\
         Start-Sleep -Seconds 2\r\n\
         Remove-Item -LiteralPath '{setup}' -Force\r\n\
         Remove-Item -LiteralPath '{dir}' -Recurse -Force\r\n",
        pid = std::process::id(),
        setup = q(installer),
        exe = q(&exe),
        dir = q(dir),
    );
    let script_path = dir.join("apply-update.ps1");
    std::fs::write(&script_path, script)?;
    std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-WindowStyle", "Hidden", "-File"])
        .arg(&script_path)
        .creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS)
        .spawn()?;
    Ok(())
}

#[cfg(not(windows))]
pub fn install_and_restart(_installer: &Path) -> std::io::Result<()> {
    Err(std::io::Error::other("automatic install is only available on Windows"))
}
