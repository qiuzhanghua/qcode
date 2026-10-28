fn main() {
    // Embed the application and tray icons (res/icon.rc -> res/icons/windows)
    // into the Windows executable: 101 is the colour app icon (exe, taskbar),
    // 102 is the tray mark.
    #[cfg(target_os = "windows")]
    {
        let _ = embed_resource::compile("res/icon.rc", embed_resource::NONE);
    }
}
