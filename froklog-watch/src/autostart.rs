//! Start on login, via the XDG autostart directory.
//!
//! A desktop file in ~/.config/autostart is how every Linux desktop agrees to
//! launch something at login, and it is the right mechanism for a tray app:
//! the file only runs once there is a graphical session to put an icon in. A
//! systemd user unit would need that dependency spelled out and would still be
//! launching a GUI, so this is both simpler and more correct.
//!
//! The entry is written on demand rather than by the installer, so the toggle
//! in the window always reflects the truth: the file exists or it does not.

use std::path::PathBuf;

use anyhow::{Context, Result};

pub fn path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("autostart")
        .join("froklog-watch.desktop")
}

pub fn is_enabled() -> bool {
    path().exists()
}

/// Autostart whatever binary is running now, not whatever happens to be on
/// PATH — otherwise testing a build here would silently arm a different one.
fn entry(exe: &str) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=froklog watch\n\
         Comment=Keep every character's EverQuest log flowing into froklog\n\
         Exec={exe} --hidden\n\
         Icon=froklog-watch\n\
         Terminal=false\n\
         StartupWMClass=froklog-watch\n\
         X-GNOME-Autostart-enabled=true\n"
    )
}

pub fn enable() -> Result<()> {
    let exe = std::env::current_exe().context("finding this binary")?;
    let p = path();
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(&p, entry(&exe.to_string_lossy()))
        .with_context(|| format!("writing {}", p.display()))?;
    Ok(())
}

pub fn disable() -> Result<()> {
    let p = path();
    match std::fs::remove_file(&p) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing {}", p.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The toggle has to work both ways: ticking it writes the entry, and
    /// unticking it must actually remove it, not just stop rewriting it.
    #[test]
    fn enable_then_disable_round_trips() {
        let tmp = std::env::temp_dir().join("froklog-watch-autostart-test");
        let _ = std::fs::remove_dir_all(&tmp);
        std::env::set_var("XDG_CONFIG_HOME", &tmp);

        assert!(!is_enabled());
        enable().expect("enable");
        assert!(is_enabled(), "entry should exist after enabling");
        let body = std::fs::read_to_string(path()).unwrap();
        assert!(body.contains("--hidden"));

        disable().expect("disable");
        assert!(!is_enabled(), "entry should be gone after disabling");
        disable().expect("disabling twice is not an error");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn entry_launches_hidden_and_names_the_binary() {
        let e = entry("/home/someone/.local/bin/froklog-watch");
        // --hidden matters: an app that starts on login should go to the tray,
        // not throw a window at you every time you sign in.
        assert!(e.contains("Exec=/home/someone/.local/bin/froklog-watch --hidden"));
        assert!(e.contains("StartupWMClass=froklog-watch"));
    }
}
