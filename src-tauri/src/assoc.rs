#[cfg(windows)]
pub fn set(on: bool) {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;
    let Ok(exe) = std::env::current_exe() else { return };
    let cmd = format!("\"{}\" \"%1\"", exe.display());
    let icon = format!("\"{}\",0", exe.display());
    let root = RegKey::predef(HKEY_CURRENT_USER);
    let Ok(classes) = root.open_subkey_with_flags("Software\\Classes", winreg::enums::KEY_ALL_ACCESS) else { return };
    if on {
        if let Ok((m, _)) = classes.create_subkey("magnet") {
            let _ = m.set_value("", &"URL:Magnet link");
            let _ = m.set_value("URL Protocol", &"");
            let _ = m.set_value("DDD", &"1");
            if let Ok((i, _)) = m.create_subkey("DefaultIcon") {
                let _ = i.set_value("", &icon);
            }
            if let Ok((c, _)) = m.create_subkey("shell\\open\\command") {
                let _ = c.set_value("", &cmd);
            }
        }
        if let Ok((t, _)) = classes.create_subkey("DDD.torrent") {
            let _ = t.set_value("", &"Torrent file");
            if let Ok((i, _)) = t.create_subkey("DefaultIcon") {
                let _ = i.set_value("", &icon);
            }
            if let Ok((c, _)) = t.create_subkey("shell\\open\\command") {
                let _ = c.set_value("", &cmd);
            }
        }
        if let Ok((e, _)) = classes.create_subkey(".torrent") {
            let _ = e.set_value("", &"DDD.torrent");
        }
    } else {
        let ours = classes.open_subkey("magnet").and_then(|m| m.get_value::<String, _>("DDD")).map(|v| v == "1").unwrap_or(false);
        if ours {
            let _ = classes.delete_subkey_all("magnet");
        }
        let ours = classes.open_subkey(".torrent").and_then(|m| m.get_value::<String, _>("")).map(|v| v == "DDD.torrent").unwrap_or(false);
        if ours {
            let _ = classes.delete_subkey_all(".torrent");
        }
        let _ = classes.delete_subkey_all("DDD.torrent");
    }
    notify_shell();
}

#[cfg(windows)]
fn notify_shell() {
    #[link(name = "shell32")]
    extern "system" {
        fn SHChangeNotify(event: i32, flags: u32, a: *const core::ffi::c_void, b: *const core::ffi::c_void);
    }
    unsafe { SHChangeNotify(0x0800_0000, 0, std::ptr::null(), std::ptr::null()) };
}

#[cfg(not(windows))]
pub fn set(_on: bool) {}
