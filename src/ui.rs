// the tray applet icon and the window

use crate::mcp::Icon;
use crate::{auth, settings, wide};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use std::sync::Mutex;
use std::sync::atomic::{AtomicPtr, Ordering};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Globalization::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::Graphics::GdiPlus::{
    GdipCreateBitmapFromScan0, GdipDeleteGraphics, GdipDisposeImage, GdipDrawImageRectI, GdipGetImageGraphicsContext,
    GdipGraphicsClear, GdipSetInterpolationMode, GdiplusShutdown, GdiplusStartup, GdiplusStartupInput,
    InterpolationModeHighQualityBicubic,
};
use windows_sys::Win32::System::Diagnostics::Debug::MessageBeep;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Time::*;
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus};
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

// ids from res/sevenmcp.rc
const STATUS: i32 = 100;
const ADMIN: i32 = 101;
const RESTART: i32 = 102;
const PORT: i32 = 103;
const APPLY: i32 = 104;
const HINT: i32 = 105;
const APPS: i32 = 106;
const REMOVE: i32 = 107;
const LOG: i32 = 108;
const OUTPUT: i32 = 109;
const OPEN: i32 = 10;
const EXIT: i32 = 11;
const ABOUT: i32 = 12;
const UWU: i32 = 13;
const CHAN: i32 = 14;
const ABOUT_TEXT: i32 = 200;
const LOGO: i32 = 201;
const LINK: i32 = 202;
const ASK_ICON: i32 = 300;
const ASK_TEXT: i32 = 301;
const ASK_INPUT: i32 = 302;
const ASK_BUTTON: i32 = 310;

// rgba pngs from build.rs
const LOGO_PNG: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/logo.png"));
const CHAN_PNG: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/sineware-chan.png"));
// PixelFormat32bppPARGB, which windows-sys doesn't have
const PARGB: i32 = 0xe200b;

// the log keeps this many calls, and this much output (utf-16 units) for each
const MAX_CALLS: usize = 500;
const MAX_OUTPUT: usize = 65_536;

const TRAY: u32 = WM_APP;
const EVENT: u32 = WM_APP + 1;

enum Event {
    Call(String, String),
    Output(usize, String),
    End(usize, String),
    Notify(String, String),
    Paired,
}

static WINDOW: AtomicPtr<core::ffi::c_void> = AtomicPtr::new(null_mut());
static NEXT_CALL: Mutex<usize> = Mutex::new(0);
// one message wakes the window for many events: windows drops posts past 10000 in a queue
static EVENTS: Mutex<Vec<Event>> = Mutex::new(Vec::new());

struct State {
    port: u16,
    taskbar_created: u32,
    // what the output box shows for each call, and whether a \r waits to rewrite the line
    calls: VecDeque<(Vec<u16>, bool)>,
    // the call id of the first row
    first: usize,
    apps: Vec<String>,
    hinted: bool,
}

thread_local! {
    static STATE: RefCell<State> =
        const { RefCell::new(State { port: 0, taskbar_created: 0, calls: VecDeque::new(), first: 0, apps: Vec::new(), hinted: false }) };
}

// a box from the dialog tool. each one has its own thread
struct Ask {
    title: String,
    text: String,
    icon: HICON,
    buttons: Vec<String>,
    input: bool,
    stopped: Box<dyn Fn() -> bool>,
    answer: String,
}

thread_local! {
    static ASK: RefCell<Option<Ask>> = const { RefCell::new(None) };
}

fn post(e: Event) {
    let mut events = EVENTS.lock().unwrap();
    if events.is_empty() {
        unsafe { PostMessageW(WINDOW.load(Ordering::Relaxed), EVENT, 0, 0) };
    }
    events.push(e);
}

#[derive(Clone, Copy, PartialEq)]
pub struct Call(usize);

pub fn call(tool: &str, detail: &str) -> Call {
    // the lock keeps the ids in the order that the window gets them
    let mut next = NEXT_CALL.lock().unwrap();
    post(Event::Call(tool.into(), detail.into()));
    *next += 1;
    Call(*next - 1)
}

impl Call {
    pub fn output(&self, text: &str) {
        post(Event::Output(self.0, text.into()));
    }

    pub fn end(&self, result: &str) {
        post(Event::End(self.0, result.into()));
    }
}

pub fn notify(title: &str, text: &str) {
    post(Event::Notify(title.into(), text.into()));
}

pub fn paired() {
    post(Event::Paired);
}

pub fn error(text: &str) {
    unsafe { MessageBoxW(WINDOW.load(Ordering::Relaxed), wide(text).as_ptr(), wide("Seven MCP").as_ptr(), MB_ICONERROR) };
}

pub fn run(port: u16, serve: impl FnOnce() + Send + 'static) {
    unsafe {
        let taskbar_created = RegisterWindowMessageW(wide("TaskbarCreated").as_ptr());
        // explorer doesn't run as administrator, so uipi drops its broadcast to us when we do
        ChangeWindowMessageFilter(taskbar_created, MSGFLT_ADD);
        STATE.with_borrow_mut(|s| (s.port, s.taskbar_created) = (port, taskbar_created));
        let icc = INITCOMMONCONTROLSEX { dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32, dwICC: ICC_LISTVIEW_CLASSES | ICC_LINK_CLASS };
        InitCommonControlsEx(&icc);
        let w = CreateDialogParamW(GetModuleHandleW(null()), 1 as _, null_mut(), Some(proc), 0);
        WINDOW.store(w, Ordering::Relaxed);
        std::thread::spawn(serve);
        let mut msg = zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            if IsDialogMessageW(w, &msg) == 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}

unsafe extern "system" fn proc(w: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> isize {
    unsafe {
        let port = STATE.with_borrow(|s| s.port);
        let log = GetDlgItem(w, LOG);
        match msg {
            WM_INITDIALOG => {
                SendMessageW(w, WM_SETICON, ICON_BIG as usize, icon(SM_CXICON) as isize);
                SendMessageW(w, WM_SETICON, ICON_SMALL as usize, icon(SM_CXSMICON) as isize);
                tray(w, NIM_ADD, "", "");
                SetDlgItemInt(w, PORT, port as u32, 0);
                let admin = IsUserAnAdmin() != 0;
                let text = if admin {
                    "Seven MCP runs as administrator."
                } else {
                    "Seven MCP doesn't run as administrator. It can't control programs that run as administrator."
                };
                SetDlgItemTextW(w, ADMIN, wide(text).as_ptr());
                ShowWindow(GetDlgItem(w, RESTART), if admin { SW_HIDE } else { SW_SHOW });
                SendDlgItemMessageW(w, RESTART, BCM_SETSHIELD, 0, 1);
                columns(w, APPS, &[("Name", 190), ("Paired", 70)]);
                columns(w, LOG, &[("Time", 48), ("Tool", 40), ("Details", 176), ("Result", 50)]);
                let dc = GetDC(null_mut());
                let height = -(9 * GetDeviceCaps(dc, LOGPIXELSY as i32) / 72);
                ReleaseDC(null_mut(), dc);
                let font = CreateFontW(height, 0, 0, 0, FW_NORMAL as i32, 0, 0, 0, DEFAULT_CHARSET as u32, OUT_DEFAULT_PRECIS as u32,
                    CLIP_DEFAULT_PRECIS as u32, DEFAULT_QUALITY as u32, (FIXED_PITCH | FF_MODERN) as u32, wide("Consolas").as_ptr());
                SendDlgItemMessageW(w, OUTPUT, WM_SETFONT, font as usize, 0);
                SendDlgItemMessageW(w, OUTPUT, EM_SETLIMITTEXT, 0, 0);
                show_apps(w);
                if settings::get("Uwu").as_deref() == Some("1") {
                    uwu(w, true);
                }
                open(w);
                return 1;
            }
            WM_COMMAND => match (wp & 0xffff) as i32 {
                OPEN => open(w),
                EXIT => quit(w),
                ABOUT => _ = DialogBoxParamW(GetModuleHandleW(null()), 2 as _, w, Some(about), 0),
                IDCANCEL => {
                    ShowWindow(w, SW_HIDE);
                    if !STATE.with_borrow_mut(|s| std::mem::replace(&mut s.hinted, true)) {
                        tray(w, NIM_MODIFY, "Seven MCP is still open",
                            "AI apps can still control this computer. To open Seven MCP or to exit, click its icon.");
                    }
                }
                RESTART => restart(w, "runas"),
                UWU => {
                    let on = settings::get("Uwu").as_deref() != Some("1");
                    settings::set("Uwu", if on { "1" } else { "0" });
                    uwu(w, on);
                }
                IDOK | APPLY => {
                    let mut ok = 0;
                    let new = GetDlgItemInt(w, PORT, &mut ok, 0);
                    if ok == 0 || !(1..=65535).contains(&new) {
                        error("Type a port number from 1 to 65535.");
                    } else if std::net::TcpListener::bind(("0.0.0.0", new as u16)).is_err() {
                        error(&format!("Seven MCP can't use port {new}. Another program uses it."));
                    } else if new != port as u32 {
                        settings::set("Port", &new.to_string());
                        restart(w, "open");
                    }
                }
                PORT if (wp >> 16) as u32 == EN_CHANGE => {
                    EnableWindow(GetDlgItem(w, APPLY), (GetDlgItemInt(w, PORT, null_mut(), 0) != port as u32) as i32);
                }
                REMOVE => {
                    if let Some(i) = selected(GetDlgItem(w, APPS)) {
                        auth::remove(&STATE.with_borrow(|s| s.apps[i].clone()));
                        show_apps(w);
                    }
                }
                _ => {}
            },
            WM_NOTIFY if (*(lp as *const NMHDR)).code == LVN_ITEMCHANGED => {
                let nm = &*(lp as *const NMLISTVIEW);
                let on = nm.uNewState & LVIS_SELECTED != 0;
                match nm.hdr.idFrom as i32 {
                    APPS => _ = EnableWindow(GetDlgItem(w, REMOVE), on as i32),
                    LOG if on => {
                        let mut text = STATE.with_borrow(|s| s.calls[nm.iItem as usize].0.clone());
                        let end = text.len();
                        text.push(0);
                        let out = GetDlgItem(w, OUTPUT);
                        SetWindowTextW(out, text.as_ptr());
                        SendMessageW(out, EM_SETSEL, end, end as isize);
                        SendMessageW(out, EM_SCROLLCARET, 0, 0);
                    }
                    _ => {}
                }
            }
            TRAY => match lp as u32 {
                WM_LBUTTONUP | NIN_BALLOONUSERCLICK => open(w),
                WM_RBUTTONUP => {
                    let menu = LoadMenuW(GetModuleHandleW(null()), 1 as _);
                    let popup = GetSubMenu(menu, 0);
                    SetMenuDefaultItem(popup, OPEN as u32, 0);
                    let mut pt = zeroed();
                    GetCursorPos(&mut pt);
                    // without these the menu doesn't close when you click away (kb135788)
                    SetForegroundWindow(w);
                    TrackPopupMenu(popup, TPM_RIGHTBUTTON, pt.x, pt.y, 0, w, null());
                    PostMessageW(w, WM_NULL, 0, 0);
                    DestroyMenu(menu);
                }
                _ => {}
            },
            EVENT => {
                for e in std::mem::take(&mut *EVENTS.lock().unwrap()) {
                    match e {
                        Event::Call(tool, detail) => {
                            let count = SendMessageW(log, LVM_GETITEMCOUNT, 0, 0) as usize;
                            let follow = selected(log).is_none_or(|i| i + 1 == count);
                            let full = STATE.with_borrow_mut(|s| {
                                s.calls.push_back((Vec::new(), false));
                                let full = s.calls.len() > MAX_CALLS;
                                if full {
                                    s.calls.pop_front();
                                    s.first += 1;
                                }
                                full
                            });
                            if full {
                                SendMessageW(log, LVM_DELETEITEM, 0, 0);
                            }
                            let mut time = [0u16; 64];
                            let n = GetTimeFormatW(LOCALE_USER_DEFAULT, 0, null(), null(), time.as_mut_ptr(), 64);
                            let row = add_row(log, &[&String::from_utf16_lossy(&time[..n.max(1) as usize - 1]), &tool, &detail, "Running"]);
                            if follow {
                                let item = LVITEMW { state: LVIS_SELECTED | LVIS_FOCUSED, stateMask: LVIS_SELECTED | LVIS_FOCUSED, ..Default::default() };
                                SendMessageW(log, LVM_SETITEMSTATE, row, &item as *const _ as isize);
                                SendMessageW(log, LVM_ENSUREVISIBLE, row, 0);
                            }
                        }
                        Event::Output(id, text) => {
                            let tail = STATE.with_borrow_mut(|s| {
                                let row = id.checked_sub(s.first)?;
                                let (buf, cr) = s.calls.get_mut(row)?;
                                let mut from = buf.len();
                                for c in text.encode_utf16() {
                                    if *cr && c != b'\n' as u16 {
                                        let start = buf.iter().rposition(|&c| c == b'\n' as u16).map_or(0, |i| i + 1);
                                        buf.truncate(start);
                                        from = from.min(start);
                                    }
                                    *cr = c == b'\r' as u16;
                                    match c {
                                        0x0d => {}
                                        0x0a => buf.extend([0x0d, 0x0a]),
                                        _ => buf.push(c),
                                    }
                                }
                                if buf.len() > MAX_OUTPUT {
                                    // drops whole old lines, half the buffer at a time
                                    let cut = buf.len() - MAX_OUTPUT / 2;
                                    let cut = buf[cut..].iter().position(|&c| c == 0x0a).map_or(cut, |i| cut + i + 1);
                                    buf.drain(..cut);
                                    from = 0;
                                }
                                Some((row, from, buf[from..].to_vec()))
                            });
                            if let Some((_, from, mut tail)) = tail.filter(|t| selected(log) == Some(t.0)) {
                                tail.push(0);
                                let out = GetDlgItem(w, OUTPUT);
                                SendMessageW(out, EM_SETSEL, from, -1);
                                SendMessageW(out, EM_REPLACESEL, 0, tail.as_ptr() as isize);
                            }
                        }
                        Event::End(id, result) => {
                            if let Some(row) = STATE.with_borrow(|s| id.checked_sub(s.first)) {
                                cell(log, row, 3, &result);
                            }
                        }
                        Event::Notify(title, text) => tray(w, NIM_MODIFY, &title, &text),
                        Event::Paired => show_apps(w),
                    }
                }
            }
            m if m == STATE.with_borrow(|s| s.taskbar_created) => tray(w, NIM_ADD, "", ""),
            _ => {}
        }
        0
    }
}

unsafe extern "system" fn about(w: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> isize {
    unsafe {
        match msg {
            WM_INITDIALOG => {
                let text = format!(
                    "Version {} ({})\nBuilt with {}\nMCP Rust SDK (rmcp) {}\n\nCopyright \u{a9} 2026 Sineware\nGNU General Public License 3.0 or later",
                    env!("VERSION"),
                    env!("TARGET"),
                    env!("RUSTC_VERSION"),
                    // expands env! inside rmcp, so it gives rmcp's own version
                    rmcp::model::Implementation::from_build_env().version
                );
                SetDlgItemTextW(w, ABOUT_TEXT, wide(&text).as_ptr());
                let repo = env!("CARGO_PKG_REPOSITORY");
                let link = format!("<a>{}</a>", repo.trim_start_matches("https://"));
                SetDlgItemTextW(w, LINK, wide(&link).as_ptr());
                // a white strip, with the logo in the middle
                let mut r = zeroed();
                GetClientRect(GetDlgItem(w, LOGO), &mut r);
                set_picture(GetDlgItem(w, LOGO), picture(LOGO_PNG, r.right, r.bottom, 0xffff_ffff));
                1
            }
            WM_COMMAND if matches!((wp & 0xffff) as i32, IDOK | IDCANCEL) => {
                set_picture(GetDlgItem(w, LOGO), null_mut());
                EndDialog(w, 0);
                1
            }
            WM_NOTIFY if (*(lp as *const NMHDR)).idFrom == LINK as usize && matches!((*(lp as *const NMHDR)).code, NM_CLICK | NM_RETURN) => {
                ShellExecuteW(w, wide("open").as_ptr(), wide(env!("CARGO_PKG_REPOSITORY")).as_ptr(), null(), null(), SW_SHOWNORMAL);
                1
            }
            _ => 0,
        }
    }
}

// waits until the user clicks a button (its index) or closes the box (none), or until stopped() is true.
// also gives the text that the user typed
pub fn ask(title: &str, text: &str, icon: Icon, buttons: Vec<String>, input: bool, stopped: impl Fn() -> bool + 'static) -> (Option<usize>, String) {
    unsafe {
        let (name, sound) = match icon {
            Icon::Info => (IDI_INFORMATION, MB_ICONINFORMATION),
            Icon::Warning => (IDI_WARNING, MB_ICONWARNING),
            Icon::Error => (IDI_ERROR, MB_ICONERROR),
        };
        // the size for this dpi, not a scaled 32 px icon
        let mut icon = null_mut();
        LoadIconMetric(null_mut(), name, LIM_LARGE, &mut icon);
        ASK.set(Some(Ask { title: title.into(), text: text.into(), icon, buttons, input, stopped: Box::new(stopped), answer: String::new() }));
        MessageBeep(sound);
        let n = DialogBoxParamW(GetModuleHandleW(null()), 3 as _, null_mut(), Some(ask_proc), 0);
        let answer = ASK.take().map_or(String::new(), |a| a.answer);
        DestroyIcon(icon);
        ((n > 0).then(|| n as usize - 1), answer)
    }
}

unsafe extern "system" fn ask_proc(w: HWND, msg: u32, wp: WPARAM, _: LPARAM) -> isize {
    unsafe {
        match msg {
            WM_INITDIALOG => ASK.with_borrow(|a| {
                let a = a.as_ref().unwrap();
                SetWindowTextW(w, wide(&a.title).as_ptr());
                SendMessageW(w, WM_SETICON, ICON_BIG as usize, icon(SM_CXICON) as isize);
                SendDlgItemMessageW(w, ASK_ICON, STM_SETICON, a.icon as usize, 0);
                // 4 dlu between buttons, 7 dlu margins, and buttons at least 50 x 14 dlu
                let mut u = RECT { left: 4, top: 7, right: 50, bottom: 14 };
                MapDialogRect(w, &mut u);
                let label = GetDlgItem(w, ASK_TEXT);
                let mut r = zeroed();
                GetWindowRect(label, &mut r);
                MapWindowPoints(null_mut(), w, (&raw mut r).cast(), 2);
                let dc = GetDC(w);
                let old = SelectObject(dc, SendMessageW(w, WM_GETFONT, 0, 0) as _);
                let text = wide(&a.text);
                let mut m = RECT { right: r.right - r.left, ..zeroed() };
                DrawTextW(dc, text.as_ptr(), -1, &mut m, DT_CALCRECT | DT_WORDBREAK | DT_NOPREFIX);
                // text longer than half the screen is cut
                let h = m.bottom.clamp(r.bottom - r.top, GetSystemMetrics(SM_CYSCREEN) / 2);
                SetWindowPos(label, null_mut(), 0, 0, r.right - r.left, h, SWP_NOMOVE | SWP_NOZORDER);
                SetWindowTextW(label, text.as_ptr());
                let mut y = r.top + h + u.top;
                let edit = GetDlgItem(w, ASK_INPUT);
                if a.input {
                    SetWindowPos(edit, null_mut(), r.left, y, r.right - r.left, u.bottom, SWP_NOZORDER);
                    y += u.bottom + u.top;
                } else {
                    ShowWindow(edit, SW_HIDE);
                }
                // from the right, like a message box
                let mut x = r.right;
                for i in (0..3).rev() {
                    let button = GetDlgItem(w, ASK_BUTTON + i);
                    let Some(name) = a.buttons.get(i as usize) else {
                        ShowWindow(button, SW_HIDE);
                        continue;
                    };
                    let name = wide(&name.replace('&', "&&"));
                    let mut size = zeroed();
                    GetTextExtentPoint32W(dc, name.as_ptr(), name.len() as i32 - 1, &mut size);
                    let width = (size.cx + 4 * u.left).max(u.right);
                    x -= width;
                    SetWindowTextW(button, name.as_ptr());
                    SetWindowPos(button, null_mut(), x, y, width, u.bottom, SWP_NOZORDER);
                    x -= u.left;
                }
                SelectObject(dc, old);
                ReleaseDC(w, dc);
                let (mut frame, mut client, mut work) = (zeroed(), zeroed(), zeroed::<RECT>());
                GetWindowRect(w, &mut frame);
                GetClientRect(w, &mut client);
                SystemParametersInfoW(SPI_GETWORKAREA, 0, (&raw mut work).cast(), 0);
                let (fw, fh) = (frame.right - frame.left, frame.bottom - frame.top - client.bottom + y + u.bottom + u.top);
                SetWindowPos(w, null_mut(), (work.left + work.right - fw) / 2, (work.top + work.bottom - fh) / 2, fw, fh, SWP_NOZORDER);
                SetFocus(GetDlgItem(w, if a.input { ASK_INPUT } else { ASK_BUTTON }));
                SetTimer(w, 1, 500, None);
                0
            }),
            WM_TIMER if ASK.with_borrow(|a| (a.as_ref().unwrap().stopped)()) => {
                EndDialog(w, 0);
                1
            }
            WM_COMMAND => {
                match (wp & 0xffff) as i32 {
                    id if (ASK_BUTTON..ASK_BUTTON + 3).contains(&id) => {
                        let edit = GetDlgItem(w, ASK_INPUT);
                        let mut text = vec![0u16; GetWindowTextLengthW(edit) as usize + 1];
                        let n = GetWindowTextW(edit, text.as_mut_ptr(), text.len() as i32);
                        ASK.with_borrow_mut(|a| a.as_mut().unwrap().answer = String::from_utf16_lossy(&text[..n as usize]));
                        EndDialog(w, (id - ASK_BUTTON + 1) as isize);
                    }
                    IDCANCEL => _ = EndDialog(w, 0),
                    _ => {}
                }
                1
            }
            _ => 0,
        }
    }
}

// a png from build.rs, fitted in the middle of a w x h bitmap over an argb color. w 0 takes the png's shape.
// static controls draw the alpha of a 32-bit bitmap
unsafe fn picture(png: &[u8], w: i32, h: i32, background: u32) -> HBITMAP {
    unsafe {
        let mut reader = png::Decoder::new(std::io::Cursor::new(png)).read_info().unwrap();
        let mut pixels = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut pixels).unwrap();
        // gdi+ wants premultiplied bgra
        for p in pixels.chunks_exact_mut(4) {
            p.swap(0, 2);
            let a = p[3] as u16;
            p[..3].iter_mut().for_each(|c| *c = (*c as u16 * a / 255) as u8);
        }
        let (pw, ph) = (info.width as i32, info.height as i32);
        let w = if w == 0 { pw * h / ph } else { w };
        let header = BITMAPINFOHEADER { biSize: size_of::<BITMAPINFOHEADER>() as u32, biWidth: w, biHeight: -h, biPlanes: 1, biBitCount: 32, ..zeroed() };
        let mut bits = null_mut();
        let bitmap = CreateDIBSection(null_mut(), &BITMAPINFO { bmiHeader: header, ..zeroed() }, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
        let (mut token, mut src, mut dst, mut g) = (0, null_mut(), null_mut(), null_mut());
        GdiplusStartup(&mut token, &GdiplusStartupInput { GdiplusVersion: 1, ..Default::default() }, null_mut());
        GdipCreateBitmapFromScan0(pw, ph, pw * 4, PARGB, pixels.as_ptr(), &mut src);
        GdipCreateBitmapFromScan0(w, h, w * 4, PARGB, bits.cast(), &mut dst);
        GdipGetImageGraphicsContext(dst.cast(), &mut g);
        GdipGraphicsClear(g, background);
        GdipSetInterpolationMode(g, InterpolationModeHighQualityBicubic);
        let scale = (w as f64 / pw as f64).min(h as f64 / ph as f64);
        let (dw, dh) = ((pw as f64 * scale) as i32, (ph as f64 * scale) as i32);
        GdipDrawImageRectI(g, src.cast(), (w - dw) / 2, (h - dh) / 2, dw, dh);
        GdipDeleteGraphics(g);
        GdipDisposeImage(dst.cast());
        GdipDisposeImage(src.cast());
        GdiplusShutdown(token);
        bitmap
    }
}

// the control keeps a copy of a bitmap that has alpha, so both can need deleting
unsafe fn set_picture(control: HWND, bitmap: HBITMAP) {
    unsafe {
        DeleteObject(SendMessageW(control, STM_SETIMAGE, IMAGE_BITMAP as usize, bitmap as isize) as _);
        if SendMessageW(control, STM_GETIMAGE, IMAGE_BITMAP as usize, 0) as HBITMAP != bitmap {
            DeleteObject(bitmap);
        }
    }
}

// sineware-chan, in a column on the right of the window
unsafe fn uwu(w: HWND, on: bool) {
    unsafe {
        let (mut client, mut frame) = (zeroed(), zeroed());
        GetClientRect(w, &mut client);
        GetWindowRect(w, &mut frame);
        // the width of the content, and a margin
        let mut content = RECT { right: 360, bottom: 7, ..zeroed() };
        MapDialogRect(w, &mut content);
        let margin = content.bottom;
        let bitmap = if on { picture(CHAN_PNG, 0, client.bottom - 2 * margin, 0) } else { null_mut() };
        let mut bm: BITMAP = zeroed();
        GetObjectW(bitmap, size_of::<BITMAP>() as i32, (&raw mut bm).cast());
        let chan = GetDlgItem(w, CHAN);
        set_picture(chan, bitmap);
        SetWindowPos(chan, null_mut(), content.right, margin, bm.bmWidth, bm.bmHeight, SWP_NOZORDER);
        let width = frame.right - frame.left - client.right + content.right + if on { bm.bmWidth + margin } else { 0 };
        SetWindowPos(w, null_mut(), 0, 0, width, frame.bottom - frame.top, SWP_NOMOVE | SWP_NOZORDER);
    }
}

unsafe fn open(w: HWND) {
    unsafe {
        let port = STATE.with_borrow(|s| s.port);
        // finds the address of the network card with the default route; it sends nothing
        let ip = std::net::UdpSocket::bind("0.0.0.0:0")
            .and_then(|s| s.connect("192.0.2.1:9").and_then(|_| s.local_addr()))
            .map_or("localhost".into(), |a| a.ip().to_string());
        SetDlgItemTextW(w, STATUS, wide(&format!("Listening on http://{ip}:{port}/mcp")).as_ptr());
        SetDlgItemTextW(w, HINT, wide(&format!("To pair an MCP client, open http://{ip}:{port}/ in a web browser on its host.")).as_ptr());
        ShowWindow(w, SW_SHOWNORMAL);
        SetForegroundWindow(w);
    }
}

unsafe fn quit(w: HWND) {
    unsafe {
        tray(w, NIM_DELETE, "", "");
        PostQuitMessage(0);
    }
}

unsafe fn restart(w: HWND, verb: &str) {
    unsafe {
        let exe = wide(&std::env::current_exe().unwrap_or_default().to_string_lossy());
        if ShellExecuteW(w, wide(verb).as_ptr(), exe.as_ptr(), null(), null(), SW_SHOWNORMAL) as isize > 32 {
            quit(w);
        }
    }
}

unsafe fn icon(metric: SYSTEM_METRICS_INDEX) -> HICON {
    unsafe {
        let size = GetSystemMetrics(metric);
        LoadImageW(GetModuleHandleW(null()), 1 as _, IMAGE_ICON, size, size, LR_SHARED)
    }
}

// a balloon shows when text isn't empty
unsafe fn tray(w: HWND, action: NOTIFY_ICON_MESSAGE, title: &str, text: &str) {
    unsafe {
        fn fill<const N: usize>(s: &str) -> [u16; N] {
            let mut a = [0; N];
            a[..N - 1].iter_mut().zip(s.encode_utf16()).for_each(|(d, c)| *d = c);
            a
        }
        let nid = NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: w,
            uID: 1,
            uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP | NIF_INFO,
            uCallbackMessage: TRAY,
            hIcon: icon(SM_CXSMICON),
            szTip: fill("Seven MCP"),
            szInfoTitle: fill(title),
            szInfo: fill(text),
            dwInfoFlags: NIIF_INFO,
            ..zeroed()
        };
        Shell_NotifyIconW(action, &nid);
    }
}

unsafe fn show_apps(w: HWND) {
    unsafe {
        let list = GetDlgItem(w, APPS);
        SendMessageW(list, LVM_DELETEALLITEMS, 0, 0);
        let apps = auth::apps();
        for (_, name, issued) in &apps {
            let t = (issued + 11_644_473_600) * 10_000_000;
            let ft = FILETIME { dwLowDateTime: t as u32, dwHighDateTime: (t >> 32) as u32 };
            let (mut utc, mut local) = (zeroed(), zeroed());
            FileTimeToSystemTime(&ft, &mut utc);
            SystemTimeToTzSpecificLocalTime(null(), &utc, &mut local);
            let mut date = [0u16; 64];
            let n = GetDateFormatW(LOCALE_USER_DEFAULT, DATE_SHORTDATE, &local, null(), date.as_mut_ptr(), 64);
            add_row(list, &[name, &String::from_utf16_lossy(&date[..n.max(1) as usize - 1])]);
        }
        STATE.with_borrow_mut(|s| s.apps = apps.into_iter().map(|a| a.0).collect());
        EnableWindow(GetDlgItem(w, REMOVE), 0);
    }
}

// widths are in dialog units
unsafe fn columns(w: HWND, id: i32, cols: &[(&str, i32)]) {
    unsafe {
        let list = GetDlgItem(w, id);
        SendMessageW(list, LVM_SETEXTENDEDLISTVIEWSTYLE, 0, (LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER) as isize);
        for (i, (name, width)) in cols.iter().enumerate() {
            let mut r = RECT { left: 0, top: 0, right: *width, bottom: 0 };
            MapDialogRect(w, &mut r);
            let mut name = wide(name);
            let col = LVCOLUMNW { mask: LVCF_TEXT | LVCF_WIDTH, cx: r.right, pszText: name.as_mut_ptr(), ..Default::default() };
            SendMessageW(list, LVM_INSERTCOLUMNW, i, &col as *const _ as isize);
        }
    }
}

unsafe fn add_row(list: HWND, cells: &[&str]) -> usize {
    unsafe {
        let row = SendMessageW(list, LVM_GETITEMCOUNT, 0, 0) as usize;
        for (col, text) in cells.iter().enumerate() {
            cell(list, row, col as i32, text);
        }
        row
    }
}

// column 0 inserts the row
unsafe fn cell(list: HWND, row: usize, col: i32, text: &str) {
    unsafe {
        let mut text = wide(text);
        let item = LVITEMW { mask: LVIF_TEXT, iItem: row as i32, iSubItem: col, pszText: text.as_mut_ptr(), ..Default::default() };
        SendMessageW(list, if col == 0 { LVM_INSERTITEMW } else { LVM_SETITEMTEXTW }, row, &item as *const _ as isize);
    }
}

unsafe fn selected(list: HWND) -> Option<usize> {
    unsafe {
        let i = SendMessageW(list, LVM_GETNEXTITEM, usize::MAX, LVNI_SELECTED as isize);
        (i >= 0).then_some(i as usize)
    }
}
