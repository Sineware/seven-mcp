use crate::wide;
use std::ptr::{null, null_mut};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_WOW64_64KEY, KEY_WRITE, REG_SZ, RegCloseKey, RegCreateKeyExW, RegOpenKeyExW,
    RegQueryValueExW, RegSetValueExW,
};

const KEY: &str = "Software\\Seven MCP";

pub fn read(root: HKEY, path: &str, name: &str) -> Option<String> {
    let (path, name) = (wide(path), wide(name));
    let mut key = null_mut();
    unsafe {
        if RegOpenKeyExW(root, path.as_ptr(), 0, KEY_READ | KEY_WOW64_64KEY, &mut key) != 0 {
            return None;
        }
        let mut size = 0;
        let mut buf = Vec::new();
        if RegQueryValueExW(key, name.as_ptr(), null(), null_mut(), null_mut(), &mut size) == 0 {
            buf.resize(size as usize / 2, 0u16);
            if RegQueryValueExW(key, name.as_ptr(), null(), null_mut(), buf.as_mut_ptr().cast(), &mut size) != 0 {
                buf.clear();
            }
        }
        RegCloseKey(key);
        (!buf.is_empty()).then(|| String::from_utf16_lossy(&buf).trim_end_matches('\0').to_string())
    }
}

pub fn get(name: &str) -> Option<String> {
    read(HKEY_CURRENT_USER, KEY, name)
}

pub fn set(name: &str, value: &str) {
    let (path, name, value) = (wide(KEY), wide(name), wide(value));
    let mut key = null_mut();
    unsafe {
        RegCreateKeyExW(HKEY_CURRENT_USER, path.as_ptr(), 0, null(), 0, KEY_WRITE, null(), &mut key, null_mut());
        RegSetValueExW(key, name.as_ptr(), 0, REG_SZ, value.as_ptr().cast(), (value.len() * 2) as u32);
        RegCloseKey(key);
    }
}
