use crate::mcp::{self, Answer, Device, File, Icon, Info, InputAction, JobAction, JobResult};
use crate::{settings, ui, wide};
use base64::{Engine, engine::general_purpose::STANDARD as B64};
use ring::digest;
use rmcp::model::ContentBlock;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::*;
use windows_sys::Win32::Globalization::{CP_OEMCP, GetOEMCP, IsDBCSLeadByteEx, MultiByteToWideChar};
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::Graphics::GdiPlus::{
    GdipCreateBitmapFromHBITMAP, GdipDisposeImage, GdipSaveImageToFile, GdiplusShutdown, GdiplusStartup, GdiplusStartupInput,
};
use windows_sys::Win32::System::Registry::HKEY_LOCAL_MACHINE;
use windows_sys::Win32::System::Threading::{CREATE_NO_WINDOW, GetCurrentThreadId};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::Shell::{FOLDERID_Downloads, IsUserAnAdmin, SHGetKnownFolderPath};
use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN, SetCursorPos};

// windows-sys links it to combase.dll, which windows 7 doesn't have
#[link(name = "ole32")]
unsafe extern "system" {
    fn CoTaskMemFree(pv: *const std::ffi::c_void);
}

// (done, total, message). done grows with each call
pub type Progress = Arc<dyn Fn(u64, Option<u64>, &str) + Send + Sync>;

// the most output that a job keeps until a client asks for it
const LIMIT: usize = 50_000;
// a stopped job that no client asks about is forgotten after this
const FORGET: Duration = Duration::from_secs(3600);

struct Job {
    pid: u32,
    // the call that started the job, and the call that waits for it now
    call: ui::Call,
    waiter: Option<ui::Call>,
    output: String,
    // bytes removed from the front of output
    cut: usize,
    // the output can still grow, as from an open pipe
    open: bool,
    stop: bool,
    // after the job stops: what it gives, or an error
    end: Option<Result<JobResult, String>>,
    ended: Instant,
    progress: Option<Progress>,
}

static JOBS: Mutex<BTreeMap<u32, Job>> = Mutex::new(BTreeMap::new());
static CHANGED: Condvar = Condvar::new();
static NEXT_JOB: AtomicU32 = AtomicU32::new(1);

fn start(pid: u32, open: bool, call: ui::Call) -> u32 {
    let id = NEXT_JOB.fetch_add(1, Ordering::Relaxed);
    let mut jobs = JOBS.lock().unwrap();
    jobs.retain(|_, j| j.end.is_none() || j.ended.elapsed() < FORGET);
    let job = Job { pid, call, waiter: None, output: String::new(), cut: 0, open, stop: false, end: None, ended: Instant::now(), progress: None };
    jobs.insert(id, job);
    id
}

// adds output and tells a waiting client. false when kill asks the job to stop
fn add(id: u32, text: &str, done: u64, total: Option<u64>, status: &str) -> bool {
    let mut jobs = JOBS.lock().unwrap();
    let Some(job) = jobs.get_mut(&id) else { return false };
    job.output += text;
    if job.output.len() > LIMIT {
        let cut = job.output.ceil_char_boundary(job.output.len() - LIMIT);
        job.output.drain(..cut);
        job.cut += cut;
    }
    // with no status, the last line, so a progress bar shows its newest state
    let status = if status.is_empty() { job.output.trim_end().rsplit(['\r', '\n']).next().unwrap_or("") } else { status };
    if let Some(p) = job.progress.as_ref().filter(|_| !status.is_empty()) {
        p(done, total, status);
    }
    CHANGED.notify_all();
    !job.stop
}

// call before ui::Call::end, so first_wait can't overwrite the result in the window
fn finish(id: u32, end: Result<JobResult, String>) {
    if let Some(job) = JOBS.lock().unwrap().get_mut(&id) {
        (job.end, job.ended) = (Some(end), Instant::now());
    }
    CHANGED.notify_all();
}

pub fn first_wait(id: u32, timeout: Option<u64>, call: ui::Call, progress: Progress) -> Result<JobResult, String> {
    let result = wait(id, timeout.unwrap_or(60), Some(progress), call);
    // the lock keeps finish from going between the check and the post
    let jobs = JOBS.lock().unwrap();
    if jobs.get(&id).is_some_and(|j| j.end.is_none()) {
        call.end(&format!("Job {id}"));
    }
    result
}

pub fn run(command: &str, call: ui::Call) -> Result<u32, String> {
    let (mut out, w) = std::io::pipe().map_err(|e| e.to_string())?;
    let mut child = Command::new("cmd")
        .raw_arg(format!("/d /s /c \"{command}\""))
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(w.try_clone().map_err(|e| e.to_string())?)
        .stderr(w)
        .spawn()
        .map_err(|e| format!("Couldn't start cmd.exe: {e}"))?;
    let id = start(child.id(), true, call);
    std::thread::spawn(move || {
        let (mut buf, mut carry, mut done) = ([0; 4096], Vec::new(), 0);
        loop {
            let n = out.read(&mut buf).unwrap_or(0);
            carry.extend_from_slice(&buf[..n]);
            let text = oem(&mut carry, n == 0);
            done += text.len() as u64;
            call.output(&text);
            if !text.is_empty() {
                add(id, &text, done, None, "");
            }
            if n == 0 {
                break;
            }
        }
        if let Some(job) = JOBS.lock().unwrap().get_mut(&id) {
            job.open = false;
        }
        CHANGED.notify_all();
    });
    std::thread::spawn(move || {
        let code = child.wait().ok().and_then(|s| s.code()).unwrap_or(-1);
        finish(id, Ok(JobResult { exit_code: Some(code), ..Default::default() }));
        call.end(&format!("Exit code {code}"));
    });
    Ok(id)
}

pub fn job(a: &mcp::Job, call: ui::Call, progress: Progress) -> Result<JobResult, String> {
    match a.action {
        Some(JobAction::Kill) => {
            kill(a.id)?;
            wait(a.id, 5, None, call)
        }
        Some(JobAction::Poll) => wait(a.id, 0, None, call),
        _ => wait(a.id, a.timeout.unwrap_or(60), Some(progress), call),
    }
}

fn kill(id: u32) -> Result<(), String> {
    // only a running job: windows can give a stopped job's pid to a new process
    let pid = JOBS.lock().unwrap().get_mut(&id).and_then(|j| {
        j.stop = true;
        j.end.is_none().then_some(j.pid)
    });
    if let Some(pid) = pid.filter(|&p| p != 0) {
        let kill = Command::new("taskkill").args(["/f", "/t", "/pid", &pid.to_string()]).creation_flags(CREATE_NO_WINDOW).output();
        kill.map_err(|e| format!("Couldn't start taskkill: {e}"))?;
    }
    Ok(())
}

// the app cancelled a call, as with esc in claude code. a job that the call started stops, because the app
// never got its id. a wait from the job tool stops and leaves the output for the next call
pub fn cancel(call: ui::Call) {
    let mut jobs = JOBS.lock().unwrap();
    let Some((&id, job)) = jobs.iter_mut().find(|(_, j)| j.waiter == Some(call)) else { return };
    job.waiter = None;
    let started = job.call == call;
    drop(jobs);
    CHANGED.notify_all();
    if started {
        let _ = kill(id);
    }
}

// gives the output since the last call, and forgets the job after it stops
fn wait(id: u32, secs: u64, progress: Option<Progress>, call: ui::Call) -> Result<JobResult, String> {
    let mut deadline = Instant::now() + Duration::from_secs(secs);
    let mut jobs = JOBS.lock().unwrap();
    let job = jobs.get_mut(&id).ok_or(format!("There is no job {id}. Maybe it stopped and its last output was already given."))?;
    (job.progress, job.waiter) = (progress, Some(call));
    loop {
        let job = &jobs[&id];
        if job.waiter != Some(call) {
            return Err(format!("The wait was cancelled. Job {id} continues: use the job tool to get its output."));
        }
        if job.end.is_some() && !job.open {
            break;
        }
        // a program that the command started can keep the output open after the command stops
        if job.end.is_some() {
            deadline = deadline.min(Instant::now() + Duration::from_secs(1));
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        jobs = CHANGED.wait_timeout(jobs, left).unwrap().0;
    }
    let job = jobs.get_mut(&id).unwrap();
    (job.progress, job.waiter) = (None, None);
    let (output, cut) = (std::mem::take(&mut job.output), std::mem::take(&mut job.cut));
    let end = match job.end.take() {
        Some(end) => {
            jobs.remove(&id);
            end.map_err(|e| if output.is_empty() { e } else { format!("{output}\n{e}") })?
        }
        None => JobResult { running_job: Some(id), ..Default::default() },
    };
    Ok(JobResult { output, removed_bytes: (cut > 0).then_some(cut), ..end })
}

// console programs write in the oem code page. a lead byte at the end waits for its second byte
fn oem(bytes: &mut Vec<u8>, all: bool) -> String {
    unsafe {
        let cp = GetOEMCP();
        let mut i = 0;
        while i < bytes.len() {
            i += if IsDBCSLeadByteEx(cp, bytes[i]) != 0 { 2 } else { 1 };
        }
        let len = if i > bytes.len() && !all { bytes.len() - 1 } else { bytes.len() };
        let part: Vec<u8> = bytes.drain(..len).collect();
        if part.is_empty() {
            return String::new();
        }
        let n = MultiByteToWideChar(CP_OEMCP, 0, part.as_ptr(), len as i32, null_mut(), 0);
        let mut wide = vec![0; n as usize];
        MultiByteToWideChar(CP_OEMCP, 0, part.as_ptr(), len as i32, wide.as_mut_ptr(), n);
        String::from_utf16_lossy(&wide)
    }
}

// width, height, and image pixels per screen pixel
fn screen() -> (i32, i32, f64) {
    let (w, h) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
    (w, h, (1280.0 / w.max(h) as f64).min(1.0))
}

pub fn screenshot(a: &mcp::Screenshot) -> Result<Vec<ContentBlock>, String> {
    let (w, h, scale) = screen();
    // the zoom area is in pixels of the full screenshot
    let at = |v: Option<f64>, default: i32| v.map_or(default, |v| (v / scale).round() as i32);
    let (sx, sy) = (at(a.x, 0).clamp(0, w - 1), at(a.y, 0).clamp(0, h - 1));
    let (sw, sh) = (at(a.width, w).clamp(1, w - sx), at(a.height, h).clamp(1, h - sy));
    let zoom = (1280.0 / sw.max(sh) as f64).min(1.0);
    let (iw, ih) = ((sw as f64 * zoom).round() as i32, (sh as f64 * zoom).round() as i32);
    let text = if (sw, sh) == (w, h) {
        format!("Screen: {w} x {h} pixels. Image: {iw} x {ih} pixels.")
    } else {
        let (n, x, y) = (zoom / scale, sx as f64 * scale, sy as f64 * scale);
        format!("This image shows the screenshot area at ({x:.0}, {y:.0}), {n:.2} times larger. For input, use screenshot \
            coordinates: x = {x:.0} + image x / {n:.2}, and y = {y:.0} + image y / {n:.2}.")
    };
    unsafe {
        let screen = GetDC(null_mut());
        let dc = CreateCompatibleDC(screen);
        let bitmap = CreateCompatibleBitmap(screen, iw, ih);
        let old = SelectObject(dc, bitmap);
        SetStretchBltMode(dc, HALFTONE);
        SetBrushOrgEx(dc, 0, 0, null_mut());
        let captured = StretchBlt(dc, 0, 0, iw, ih, screen, sx, sy, sw, sh, SRCCOPY | CAPTUREBLT) != 0;
        SelectObject(dc, old);
        DeleteDC(dc);
        ReleaseDC(null_mut(), screen);

        let path = std::env::temp_dir().join(format!("sevenmcp-{}.png", GetCurrentThreadId()));
        let (mut token, mut image) = (0, null_mut());
        let png = windows_sys::core::GUID::from_u128(0x557cf406_1a04_11d3_9a73_0000f81ef32e);
        GdiplusStartup(&mut token, &GdiplusStartupInput { GdiplusVersion: 1, ..Default::default() }, null_mut());
        GdipCreateBitmapFromHBITMAP(bitmap, null_mut(), &mut image);
        GdipSaveImageToFile(image.cast(), wide(&path.to_string_lossy()).as_ptr(), &png, null());
        GdipDisposeImage(image.cast());
        GdiplusShutdown(token);
        DeleteObject(bitmap);
        let data = std::fs::read(&path);
        let _ = std::fs::remove_file(&path);
        let data = data.ok().filter(|_| captured).ok_or("Seven MCP can't capture the screen. The computer may be locked, or a UAC prompt may be open.")?;
        Ok(vec![ContentBlock::text(text), ContentBlock::image(B64.encode(data), "image/png")])
    }
}

pub fn input(a: &mcp::Input) -> Result<(), String> {
    let mouse = |flags, data: i32| INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 { mi: MOUSEINPUT { dx: 0, dy: 0, mouseData: data as u32, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    };
    // virtual machines and remote sessions read the scan code, not the virtual key
    let key = |vk, unicode, flags| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: if vk == 0 { unicode } else { unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC) as u16 } },
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    let (down, up) = (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP);
    let inputs: Vec<INPUT> = match a.action {
        InputAction::Move => vec![],
        InputAction::Click => vec![mouse(down, 0), mouse(up, 0)],
        InputAction::DoubleClick => vec![mouse(down, 0), mouse(up, 0), mouse(down, 0), mouse(up, 0)],
        InputAction::RightClick => vec![mouse(MOUSEEVENTF_RIGHTDOWN, 0), mouse(MOUSEEVENTF_RIGHTUP, 0)],
        InputAction::Scroll => vec![mouse(MOUSEEVENTF_WHEEL, a.amount.unwrap_or(-3) * 120)],
        InputAction::Type => {
            let text = a.text.as_deref().unwrap_or("").encode_utf16().filter(|&c| c != 13);
            text.flat_map(|c| match c {
                10 => [key(VK_RETURN, 0, 0), key(VK_RETURN, 0, KEYEVENTF_KEYUP)],
                _ => [key(0, c, KEYEVENTF_UNICODE), key(0, c, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP)],
            })
            .collect()
        }
        InputAction::Key => {
            let mut keys = Vec::new();
            for name in a.keys.as_deref().unwrap_or("").to_lowercase().split('+') {
                let vk = match name.trim() {
                    "ctrl" | "control" => VK_CONTROL,
                    "alt" => VK_MENU,
                    "shift" => VK_SHIFT,
                    "win" | "windows" => VK_LWIN,
                    "enter" | "return" => VK_RETURN,
                    "tab" => VK_TAB,
                    "esc" | "escape" => VK_ESCAPE,
                    "backspace" => VK_BACK,
                    "delete" | "del" => VK_DELETE,
                    "insert" => VK_INSERT,
                    "home" => VK_HOME,
                    "end" => VK_END,
                    "pageup" => VK_PRIOR,
                    "pagedown" => VK_NEXT,
                    "up" => VK_UP,
                    "down" => VK_DOWN,
                    "left" => VK_LEFT,
                    "right" => VK_RIGHT,
                    "space" => VK_SPACE,
                    "menu" | "apps" => VK_APPS,
                    "printscreen" => VK_SNAPSHOT,
                    k if k.len() == 1 && k.as_bytes()[0].is_ascii_alphanumeric() => k.to_ascii_uppercase().as_bytes()[0] as u16,
                    k => match k.strip_prefix('f').and_then(|n| n.parse::<u16>().ok()).filter(|n| (1..=24).contains(n)) {
                        Some(n) => VK_F1 + n - 1,
                        None => return Err(format!("Seven MCP doesn't know the key \"{k}\".")),
                    },
                };
                let extended = matches!(vk, VK_UP | VK_DOWN | VK_LEFT | VK_RIGHT | VK_HOME | VK_END | VK_PRIOR | VK_NEXT | VK_INSERT | VK_DELETE | VK_LWIN | VK_APPS);
                keys.push((vk, if extended { KEYEVENTF_EXTENDEDKEY } else { 0 }));
            }
            let presses = keys.iter().map(|&(vk, f)| key(vk, 0, f));
            presses.chain(keys.iter().rev().map(|&(vk, f)| key(vk, 0, f | KEYEVENTF_KEYUP))).collect()
        }
    };
    unsafe {
        let (_, _, scale) = screen();
        if let (Some(x), Some(y)) = (a.x, a.y) {
            SetCursorPos((x / scale).round() as i32, (y / scale).round() as i32);
        }
        if SendInput(inputs.len() as u32, inputs.as_ptr(), size_of::<INPUT>() as i32) as usize != inputs.len() {
            return Err("Windows blocked the input. A program that runs as administrator, or a UAC prompt, may have the focus.".into());
        }
    }
    // menus fade in, so a screenshot just after a click can miss them
    std::thread::sleep(Duration::from_millis(500));
    Ok(())
}

pub fn fetch(a: &mcp::Fetch, call: ui::Call) -> Result<u32, String> {
    let url = a.url.clone();
    let path = match a.path.as_deref() {
        Some(p) => p.into(),
        None => unsafe {
            let name = url.split(['?', '#']).next().unwrap_or("").rsplit('/').next().filter(|n| !n.is_empty()).unwrap_or("download");
            // the user can move the downloads folder
            let mut dir = null_mut();
            if SHGetKnownFolderPath(&FOLDERID_Downloads, 0, null_mut(), &mut dir) != 0 {
                CoTaskMemFree(dir.cast());
                return Err("Seven MCP can't find the Downloads folder. Give a path.".into());
            }
            let len = (0..).take_while(|&i| *dir.add(i) != 0).count();
            let folder = String::from_utf16_lossy(std::slice::from_raw_parts(dir, len));
            CoTaskMemFree(dir.cast());
            std::path::Path::new(&folder).join(name)
        },
    };
    // a server that stops sending fails the download, so kill can't wait on it forever
    let agent = ureq::Agent::new_with_config(ureq::Agent::config_builder().timeout_per_read(Some(Duration::from_secs(60))).build());
    let mut response = agent.get(&url).call().map_err(|e| format!("Couldn't download {url}: {e}"))?;
    let mut file = std::fs::File::create(&path).map_err(|e| format!("Couldn't write {}: {e}", path.display()))?;
    let id = start(0, false, call);
    std::thread::spawn(move || {
        let total = response.body().content_length();
        let mut body = response.body_mut().as_reader();
        let mut hash = digest::Context::new(&digest::SHA256);
        let mb = |n: u64| format!("{:.1} MB", n as f64 / 1048576.0);
        let (mut buf, mut done, mut shown) = (vec![0; 65536], 0, Instant::now());
        let copied = (|| -> std::io::Result<()> {
            loop {
                let n = body.read(&mut buf)?;
                if n == 0 {
                    return Ok(());
                }
                file.write_all(&buf[..n])?;
                hash.update(&buf[..n]);
                done += n as u64;
                if shown.elapsed() > Duration::from_millis(500) {
                    let status = format!("{} of {}", mb(done), total.map_or("unknown size".into(), mb));
                    call.output(&format!("\r{status}"));
                    if !add(id, "", done, total, &status) {
                        return Err(std::io::ErrorKind::Interrupted.into());
                    }
                    shown = Instant::now();
                }
            }
        })();
        drop(file);
        let (end, shown) = match copied {
            Ok(()) => {
                let sha256: String = hash.finish().as_ref().iter().map(|b| format!("{b:02x}")).collect();
                let shown = format!("Saved {} ({done} bytes) to {}.\nSHA-256: {sha256}", mb(done), path.display());
                (Ok(JobResult { file: Some(File { path: path.display().to_string(), bytes: done, sha256 }), ..Default::default() }), shown)
            }
            Err(e) => {
                let _ = std::fs::remove_file(&path);
                if e.kind() == std::io::ErrorKind::Interrupted {
                    let note = "The job was killed. Seven MCP deleted the partial file.".to_string();
                    (Ok(JobResult { note: Some(note.clone()), ..Default::default() }), note)
                } else {
                    let e = format!("The download of {url} stopped: {e}. Seven MCP deleted the partial file.");
                    (Err(e.clone()), e)
                }
            }
        };
        let error = end.is_err();
        finish(id, end);
        call.output(&format!("\n{shown}"));
        call.end(if error { "Error" } else { "Done" });
    });
    Ok(id)
}

pub fn dialog(a: &mcp::Dialog, call: ui::Call) -> Result<u32, String> {
    let buttons = a.buttons.clone().unwrap_or_else(|| vec!["OK".into()]);
    if !(1..=3).contains(&buttons.len()) {
        return Err("Give 1 to 3 button labels.".into());
    }
    let (title, text, icon, input) = (a.title.clone(), a.text.clone(), a.icon.unwrap_or(Icon::Info), a.input.unwrap_or(false));
    let id = start(0, false, call);
    std::thread::spawn(move || {
        let stopped = move || JOBS.lock().unwrap().get(&id).is_none_or(|j| j.stop);
        let (button, answer) = ui::ask(&title, &text, icon, buttons.clone(), input, stopped);
        let killed = button.is_none() && stopped();
        let shown = match button {
            Some(i) if input => format!("The user clicked \"{}\". Text: {answer}", buttons[i]),
            Some(i) => format!("The user clicked \"{}\".", buttons[i]),
            None if killed => "The job was killed. Seven MCP closed the dialog box.".into(),
            None => "The user closed the dialog box and didn't click a button.".into(),
        };
        let end = if killed {
            JobResult { note: Some(shown.clone()), ..Default::default() }
        } else {
            let answer = Answer { button: button.map(|i| buttons[i].clone()), text: input.then_some(answer) };
            JobResult { answer: Some(answer), ..Default::default() }
        };
        finish(id, Ok(end));
        call.output(&shown);
        call.end("Done");
    });
    Ok(id)
}

pub fn info() -> Info {
    let reg = |name| settings::read(HKEY_LOCAL_MACHINE, "SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion", name).unwrap_or_default();
    let env = |name| std::env::var(name).unwrap_or_default();
    let (screen_width, screen_height, _) = screen();
    let mut problem_devices = Vec::new();
    unsafe {
        let mut len = 0;
        CM_Get_Device_ID_List_SizeW(&mut len, null(), CM_GETIDLIST_FILTER_NONE);
        let mut ids = vec![0u16; len as usize];
        CM_Get_Device_ID_ListW(null(), ids.as_mut_ptr(), len, CM_GETIDLIST_FILTER_NONE);
        for id in ids.split(|&c| c == 0).filter(|id| !id.is_empty()) {
            let (mut node, mut status, mut problem) = (0, 0, 0);
            let found = CM_Locate_DevNodeW(&mut node, [id, &[0]].concat().as_ptr(), CM_LOCATE_DEVNODE_NORMAL) == CR_SUCCESS;
            if !found || CM_Get_DevNode_Status(&mut status, &mut problem, node, 0) != CR_SUCCESS || status & DN_HAS_PROBLEM == 0 {
                continue;
            }
            let mut name = [0u16; 256];
            for property in [CM_DRP_FRIENDLYNAME, CM_DRP_DEVICEDESC] {
                let mut size = 512;
                if CM_Get_DevNode_Registry_PropertyW(node, property, null_mut(), name.as_mut_ptr().cast(), &mut size, 0) == CR_SUCCESS {
                    break;
                }
            }
            let meaning = match problem {
                1 => Some("not configured"),
                10 => Some("can't start"),
                22 => Some("disabled"),
                28 => Some("no driver"),
                31 => Some("driver doesn't load"),
                43 => Some("stopped by Windows"),
                _ => None,
            };
            problem_devices.push(Device {
                name: String::from_utf16_lossy(&name).trim_end_matches('\0').into(),
                id: String::from_utf16_lossy(id),
                problem,
                meaning: meaning.map(Into::into),
            });
        }
    }
    let version = reg("CurrentVersion");
    let mut hints: Vec<String> = Vec::new();
    if version == "6.0" || version == "6.1" {
        hints.extend([
            "To add and install a driver, run `pnputil -i -a <file.inf>`. This pnputil has no /add-driver.".into(),
            "PowerShell 2.0 and Internet Explorer can't open many modern HTTPS sites. Use the fetch tool to download.".into(),
            "There is no curl, winget, or tar. To expand a .zip, use PowerShell: $s = New-Object -ComObject Shell.Application; \
                $s.NameSpace('<folder>').CopyHere($s.NameSpace('<file.zip>').Items(), 16)".into(),
            "To install a .msu update, run `wusa <file.msu> /quiet /norestart`.".into(),
            "A UAC prompt opens on the secure desktop. The screenshot and input tools can't reach it.".into(),
        ]);
    }
    if version == "6.0" {
        hints.push("Vista has no PowerShell unless Windows Management Framework 2.0 is installed, which needs Service Pack 2. \
            To find out, run `powershell -Command $PSVersionTable`. Without it, use cmd.exe.".into());
    }
    Info {
        computer: env("COMPUTERNAME"),
        windows: format!("{} {}", reg("ProductName"), reg("CSDVersion")).trim().into(),
        version,
        build: reg("CurrentBuildNumber"),
        architecture: std::env::var("PROCESSOR_ARCHITEW6432").unwrap_or_else(|_| env("PROCESSOR_ARCHITECTURE")),
        seven_mcp_architecture: if cfg!(target_arch = "x86") { "x86" } else { "x64" }.into(),
        runs_as_administrator: unsafe { IsUserAnAdmin() != 0 },
        screen_width,
        screen_height,
        problem_devices,
        hints,
    }
}
