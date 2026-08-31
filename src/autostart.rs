//! Автозапуск через LaunchAgent. SMAppService требует подписанного бандла с
//! правильным Info.plist, а обычный plist работает и с ad-hoc подписью.

use anyhow::Result;
use std::path::PathBuf;

const LABEL: &str = "com.davnozdu.llm-dict";

fn plist_path() -> PathBuf {
    dirs_home()
        .join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist"))
}

fn dirs_home() -> PathBuf {
    directories::UserDirs::new()
        .map(|d| d.home_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("~"))
}

/// Путь к .app, а не к внутреннему бинарнику: так приложение стартует
/// как полноценный бандл и сохраняет выданные разрешения.
fn app_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    // .../LLM-dict.app/Contents/MacOS/llm-dict
    let app = exe.parent()?.parent()?.parent()?;
    app.extension()
        .and_then(|e| e.to_str())
        .filter(|e| *e == "app")
        .map(|_| app.to_path_buf())
}

pub fn is_enabled() -> bool {
    plist_path().exists()
}

pub fn set(enabled: bool) -> Result<()> {
    let path = plist_path();
    if !enabled {
        if path.exists() {
            let _ = std::process::Command::new("/bin/launchctl")
                .args(["unload", &path.to_string_lossy()])
                .status();
            std::fs::remove_file(&path)?;
        }
        return Ok(());
    }

    let target = app_path()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| {
            std::env::current_exe()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default()
        });

    // Через `open -a`, чтобы стартовал бандл со своим Info.plist.
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>/usr/bin/open</string>
        <string>-a</string>
        <string>{target}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <false/>
</dict>
</plist>
"#
    );

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, plist)?;
    let _ = std::process::Command::new("/bin/launchctl")
        .args(["load", &path.to_string_lossy()])
        .status();
    Ok(())
}
