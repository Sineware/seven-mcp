// install in program files, uninstall from programs and features, and updates from github releases

use crate::{settings, ui, wide};
use ring::digest::{SHA256, digest};
use serde::Deserialize;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::{CloseHandle, HWND};
use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_DELAY_UNTIL_REBOOT, MoveFileExW};
use windows_sys::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RegDeleteKeyValueW, RegDeleteTreeW};
use windows_sys::Win32::System::Threading::{CREATE_NO_WINDOW, GetExitCodeProcess, WaitForSingleObject};
use windows_sys::Win32::UI::Shell::{IsUserAnAdmin, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
use windows_sys::Win32::UI::WindowsAndMessaging::{IDYES, MB_ICONQUESTION, MB_YESNO, MessageBoxW, SW_SHOWNORMAL};

const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const UNINSTALL: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\Seven MCP";
const LATEST: &str = "https://api.github.com/repos/Sineware/seven-mcp/releases/latest";
#[cfg(target_arch = "x86_64")]
const ASSET: &str = "SevenMCP-x64.exe";
#[cfg(target_arch = "x86")]
const ASSET: &str = "SevenMCP-x86.exe";

#[derive(Deserialize)]
pub struct Release {
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
    digest: Option<String>,
}

// a 32-bit process gets "program files (x86)"
pub fn exe() -> PathBuf {
    Path::new(&std::env::var("ProgramFiles").unwrap_or_default()).join(r"Seven MCP\SevenMCP.exe")
}

pub fn installed() -> bool {
    std::env::current_exe().is_ok_and(|e| e.to_string_lossy().eq_ignore_ascii_case(&exe().to_string_lossy()))
}

// runs as administrator, from the portable exe or a downloaded update
pub fn install() {
    let exe = exe();
    let old = exe.with_extension("old");
    // windows can rename a running exe, but can't write over it
    _ = std::fs::rename(&exe, &old);
    let copied = std::fs::create_dir_all(exe.parent().unwrap()).and_then(|_| std::fs::copy(std::env::current_exe()?, &exe));
    if let Err(e) = copied {
        ui::error(&format!("Seven MCP can't install in {}: {e}", exe.display()));
        std::process::exit(1);
    }
    unsafe { MoveFileExW(wide(&old.to_string_lossy()).as_ptr(), null(), MOVEFILE_DELAY_UNTIL_REBOOT) };
    let quoted = format!("\"{}\"", exe.display());
    settings::write(HKEY_LOCAL_MACHINE, RUN, "Seven MCP", &quoted);
    for (name, value) in [
        ("DisplayName", "Seven MCP"),
        ("DisplayVersion", env!("VERSION")),
        ("Publisher", "Sineware"),
        ("DisplayIcon", &quoted),
        ("UninstallString", &format!("{quoted} /uninstall")),
        ("URLInfoAbout", env!("CARGO_PKG_REPOSITORY")),
    ] {
        settings::write(HKEY_LOCAL_MACHINE, UNINSTALL, name, value);
    }
}

// programs and features starts this without administrator rights
pub fn uninstall() {
    let exe = std::env::current_exe().unwrap_or_default();
    unsafe {
        if IsUserAnAdmin() == 0 {
            elevate(null_mut(), &exe, "/uninstall");
            return;
        }
        let text = wide("Do you want to remove Seven MCP from this computer?");
        if MessageBoxW(null_mut(), text.as_ptr(), wide("Seven MCP").as_ptr(), MB_YESNO | MB_ICONQUESTION) != IDYES {
            return;
        }
        let me = format!("PID ne {}", std::process::id());
        _ = Command::new("taskkill").args(["/f", "/im", "SevenMCP.exe", "/fi", &me]).creation_flags(CREATE_NO_WINDOW).status();
        RegDeleteKeyValueW(HKEY_LOCAL_MACHINE, wide(RUN).as_ptr(), wide("Seven MCP").as_ptr());
        RegDeleteTreeW(HKEY_LOCAL_MACHINE, wide(UNINSTALL).as_ptr());
    }
    // a running exe can't delete its own folder, so cmd deletes it after we stop
    let dir = exe.parent().unwrap_or(&exe).display().to_string();
    _ = Command::new("cmd")
        .raw_arg(format!("/c ping -n 3 127.0.0.1 >nul & rd /s /q \"{dir}\""))
        .current_dir(std::env::temp_dir())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
}

// true if the program ran as administrator and stopped with exit code 0 in a minute
pub fn elevate(w: HWND, exe: &Path, args: &str) -> bool {
    let (verb, file, args) = (wide("runas"), wide(&exe.to_string_lossy()), wide(args));
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        hwnd: w,
        lpVerb: verb.as_ptr(),
        lpFile: file.as_ptr(),
        lpParameters: args.as_ptr(),
        nShow: SW_SHOWNORMAL,
        ..unsafe { std::mem::zeroed() }
    };
    let mut code = 1;
    unsafe {
        if ShellExecuteExW(&mut info) != 0 {
            // 0.2.0 and older have no /install, and start a server that doesn't stop
            WaitForSingleObject(info.hProcess, 60_000);
            GetExitCodeProcess(info.hProcess, &mut code);
            CloseHandle(info.hProcess);
        }
    }
    code == 0
}

fn agent() -> ureq::Agent {
    ureq::Agent::new_with_config(ureq::Agent::config_builder().timeout_global(Some(std::time::Duration::from_secs(60))).build())
}

pub fn latest() -> Result<Release, String> {
    let json = agent().get(LATEST).call().and_then(|mut r| r.body_mut().read_to_string()).map_err(|e| e.to_string())?;
    serde_json::from_str(&json).map_err(|e| e.to_string())
}

impl Release {
    pub fn version(&self) -> &str {
        self.tag_name.trim_start_matches('v')
    }

    // a dev build sorts after the release it comes from
    pub fn newer(&self) -> bool {
        match (semver::Version::parse(self.version()), semver::Version::parse(env!("VERSION"))) {
            (Ok(latest), Ok(this)) => latest > this,
            _ => false,
        }
    }

    // the exe for this architecture, in the temp folder, after a sha-256 check
    pub fn download(&self) -> Result<PathBuf, String> {
        let asset = self.assets.iter().find(|a| a.name == ASSET).ok_or(format!("The release has no {ASSET}."))?;
        let mut response = agent().get(&asset.browser_download_url).call().map_err(|e| e.to_string())?;
        let bytes = response.body_mut().with_config().limit(64 << 20).read_to_vec().map_err(|e| e.to_string())?;
        let hash: String = digest(&SHA256, &bytes).as_ref().iter().map(|b| format!("{b:02x}")).collect();
        if asset.digest.as_deref() != Some(&format!("sha256:{hash}")) {
            return Err("The SHA-256 of the download isn't correct.".into());
        }
        let path = std::env::temp_dir().join(ASSET);
        std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
        Ok(path)
    }
}
