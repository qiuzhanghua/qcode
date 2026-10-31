//! Native platform helpers: file/directory pickers with captions in the
//! machine's UI language.

use std::path::PathBuf;

/// Whether the machine's display language prefers Chinese captions for the
/// native pickers. The dialogs themselves stay native; only the caption text
/// switches between zh and en. On Windows the caption is decided inside the
/// PowerShell script from `InstalledUICulture`, so this probe is not compiled
/// there.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn native_ui_is_zh() -> bool {
    #[cfg(target_os = "macos")]
    {
        let Ok(output) = std::process::Command::new("defaults")
            .args(["read", "-g", "AppleLanguages"])
            .output()
        else {
            return false;
        };
        let text = String::from_utf8_lossy(&output.stdout);
        text.contains("\"zh")
    }
    #[cfg(target_os = "linux")]
    {
        std::env::var("LANG")
            .or_else(|_| std::env::var("LC_ALL"))
            .map(|lang| lang.starts_with("zh"))
            .unwrap_or(false)
    }
}

/// The PowerShell host used to launch the native file/folder pickers.
///
/// On zh-CN systems, `pwsh` (PowerShell 7) initializes its thread UI language
/// from the user's UI language, so the shell file dialog's built-in labels
/// ("打开(O)"/"取消"/"文件名(N):", "此电脑") render in Chinese. `powershell.exe`
/// (5.1) pins an en-US thread UI language regardless of the system language,
/// which leaves those labels in English even after setting `CurrentUICulture`
/// (its MUI language APIs are blocked and cannot change it). Prefer `pwsh`
/// when present, and fall back to the preinstalled `powershell.exe` otherwise.
#[cfg(target_os = "windows")]
fn ps_host() -> &'static str {
    static HOST: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();
    HOST.get_or_init(|| {
        let pwsh_ok = std::process::Command::new("pwsh.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", "exit 0"])
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false);
        if pwsh_ok {
            "pwsh.exe"
        } else {
            "powershell.exe"
        }
    })
}

pub(crate) fn pick_directory() -> Result<Option<PathBuf>, String> {
    #[cfg(target_os = "macos")]
    {
        let prompt = if native_ui_is_zh() {
            "打开文件夹"
        } else {
            "Open a folder"
        };
        let script = format!("POSIX path of (choose folder with prompt \"{prompt}\")");
        let output = std::process::Command::new("osascript")
            .arg("-e")
            .arg(script)
            .output()
            .map_err(|error| format!("cannot open folder picker: {error}"))?;
        if !output.status.success() {
            // Cancel is a non-zero status with an AppleScript cancel message.
            return Ok(None);
        }
        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if path.is_empty() {
            return Ok(None);
        }
        Ok(Some(PathBuf::from(path.trim_end_matches('/'))))
    }
    #[cfg(target_os = "linux")]
    {
        let title = if native_ui_is_zh() {
            "打开文件夹"
        } else {
            "Open a folder"
        };
        let output = std::process::Command::new("zenity")
            .arg("--file-selection")
            .arg("--directory")
            .arg(format!("--title={title}"))
            .output()
            .map_err(|error| format!("cannot open folder picker: {error}"))?;
        if !output.status.success() {
            return Ok(None);
        }
        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok((!path.is_empty()).then(|| PathBuf::from(path)))
    }
    #[cfg(target_os = "windows")]
    {
        // Windows: the standard OpenFileDialog (the same dialog the file
        // picker uses) configured to select a folder. The user navigates to
        // the target directory and the chosen folder is derived from the
        // dialog's file-name box. The host is `ps_host()` (pwsh preferred):
        // on zh-CN systems powershell.exe shows the dialog's built-in labels
        // in English, pwsh shows them in Chinese. `-STA` is required for the
        // dialog's message pump and `-WindowStyle Hidden` keeps the console
        // window from flashing while the dialog shows. Cancelling leaves no
        // output, which reads as Ok(None). `[Console]::OutputEncoding` must be
        // forced to UTF-8: without it the pipe carries the OEM codepage (GBK
        // on zh-CN), so non-ASCII paths arrive garbled in the UTF-8 decode.
        let script = r#"
Add-Type -AssemblyName System.Windows.Forms
# Pin the .NET UI culture to the machine's actual display language so the
# dialog's built-in labels ("此电脑", "This PC", …) never flip between zh-CN
# and en-US depending on which resource pack PowerShell happens to load.
$ui = [System.Globalization.CultureInfo]::InstalledUICulture
[Threading.Thread]::CurrentThread.CurrentUICulture = $ui
[Threading.Thread]::CurrentThread.CurrentCulture = $ui
[Windows.Forms.Application]::CurrentCulture = $ui
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
$dialog = New-Object System.Windows.Forms.OpenFileDialog
if ($ui.Name -like 'zh*') {
    $dialog.Title = '选择工作目录'
    $dialog.FileName = '选择文件夹'
    $dialog.Filter = '文件夹|*.folder'
} else {
    $dialog.Title = 'Open a folder'
    $dialog.FileName = 'Open a folder'
    $dialog.Filter = 'Folders|*.folder'
}
$dialog.FilterIndex = 1
$dialog.InitialDirectory = [Environment]::GetFolderPath('UserProfile')
$dialog.ValidateNames = $false
$dialog.CheckFileExists = $false
$dialog.CheckPathExists = $true
if ($dialog.ShowDialog() -eq [System.Windows.Forms.DialogResult]::OK) {
    $path = $dialog.FileName
    if ([System.IO.Directory]::Exists($path)) {
        Write-Output $path
    } else {
        $dir = [System.IO.Path]::GetDirectoryName($path)
        if ($dir) { Write-Output $dir }
    }
}
"#;
        let output = std::process::Command::new(ps_host())
            .args([
                "-NoProfile",
                "-STA",
                "-WindowStyle",
                "Hidden",
                "-Command",
                script,
            ])
            .output()
            .map_err(|error| format!("cannot open folder picker: {error}"))?;
        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok((!path.is_empty()).then(|| PathBuf::from(path)))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        Err(
            "the native folder picker is not implemented on this platform; type the path instead"
                .to_string(),
        )
    }
}

/// Open a native file picker and return the chosen file, or `None` on cancel.
/// Mirrors [`pick_directory`] per platform: osascript / zenity / PowerShell.
pub(crate) fn pick_file() -> Result<Option<PathBuf>, String> {
    #[cfg(target_os = "macos")]
    {
        let prompt = if native_ui_is_zh() {
            "选择文件"
        } else {
            "Choose a file"
        };
        let script = format!("POSIX path of (choose file with prompt \"{prompt}\")");
        let output = std::process::Command::new("osascript")
            .arg("-e")
            .arg(script)
            .output()
            .map_err(|error| format!("cannot open file picker: {error}"))?;
        if !output.status.success() {
            return Ok(None);
        }
        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok((!path.is_empty()).then(|| PathBuf::from(path)))
    }
    #[cfg(target_os = "linux")]
    {
        let title = if native_ui_is_zh() {
            "选择文件"
        } else {
            "Choose a file"
        };
        let output = std::process::Command::new("zenity")
            .arg("--file-selection")
            .arg(format!("--title={title}"))
            .output()
            .map_err(|error| format!("cannot open file picker: {error}"))?;
        if !output.status.success() {
            return Ok(None);
        }
        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok((!path.is_empty()).then(|| PathBuf::from(path)))
    }
    #[cfg(target_os = "windows")]
    {
        // Windows: PowerShell's WinForms OpenFileDialog, hosted by `ps_host()`
        // (pwsh preferred so the dialog's built-in labels follow the system
        // language; see ps_host). Same encoding and STA requirements as the
        // folder picker above.
        let script = r#"
Add-Type -AssemblyName System.Windows.Forms
# Same culture pin as pick_directory above — keep WinForms built-in strings
# ("此电脑", "This PC", …) consistent with the machine's display language.
$ui = [System.Globalization.CultureInfo]::InstalledUICulture
[Threading.Thread]::CurrentThread.CurrentUICulture = $ui
[Threading.Thread]::CurrentThread.CurrentCulture = $ui
[Windows.Forms.Application]::CurrentCulture = $ui
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
$dialog = New-Object System.Windows.Forms.OpenFileDialog
if ($ui.Name -like 'zh*') {
    $dialog.Title = '选择文件'
} else {
    $dialog.Title = 'Choose a file'
}
if ($dialog.ShowDialog() -eq [System.Windows.Forms.DialogResult]::OK) {
    Write-Output $dialog.FileName
}
"#;
        let output = std::process::Command::new(ps_host())
            .args([
                "-NoProfile",
                "-STA",
                "-WindowStyle",
                "Hidden",
                "-Command",
                script,
            ])
            .output()
            .map_err(|error| format!("cannot open file picker: {error}"))?;
        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok((!path.is_empty()).then(|| PathBuf::from(path)))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        Err(
            "the native file picker is not implemented on this platform; type the path instead"
                .to_string(),
        )
    }
}
