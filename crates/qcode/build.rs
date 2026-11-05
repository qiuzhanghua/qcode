fn main() {
    // Embed the application and tray icons (res/icon.rc -> res/icons/windows)
    // into the Windows executable: 1 is the colour app icon (exe, taskbar, and
    // the GPUI window title bar — the window backend loads resource 1), 2 is
    // the tray mark loaded by qcode-gui's tray code.
    #[cfg(target_os = "windows")]
    {
        let _ = embed_resource::compile("res/icon.rc", embed_resource::NONE);
    }
}
