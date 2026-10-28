fn main() {
    // Embed the application icon (res/icon.rc -> res/logo.ico) into the
    // Windows executable; it becomes the exe icon, the taskbar icon and the
    // source for the tray icon.
    #[cfg(target_os = "windows")]
    {
        let _ = embed_resource::compile("res/icon.rc", embed_resource::NONE);
    }
}
