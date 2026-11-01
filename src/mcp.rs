// the mcp surface. rmcp speaks the protocol; each tool runs its windows work from tools.rs.
// field doc comments are the schema descriptions that apps read

use crate::{tools, ui};
use rmcp::handler::server::{tool::schema_for_output, wrapper::Parameters};
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ProgressNotificationParam, ServerCapabilities, ServerConfig};
use rmcp::schemars::{self, JsonSchema};
use rmcp::{RoleServer, ServerHandler, service::RequestContext, tool, tool_handler, tool_router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;

const INSTRUCTIONS: &str = "Seven MCP controls a Windows computer, usually running Windows 7 or Vista. \
    Expect the tools of that era: cmd.exe, and PowerShell 2.0 if it is installed (Vista doesn't include it). \
    Call the info tool first.";

#[derive(Deserialize, Serialize, JsonSchema)]
pub struct Run {
    /// The command line, as typed at a cmd.exe prompt
    pub command: String,
    /// Seconds to wait before the command continues as a job. Default 60.
    pub timeout: Option<u64>,
}

#[derive(Deserialize, Serialize, JsonSchema)]
pub struct Job {
    /// The job number from the run, fetch, or dialog tool
    pub id: u32,
    /// Default wait
    pub action: Option<JobAction>,
    /// For wait: the most seconds to wait. Default 60.
    pub timeout: Option<u64>,
}

#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum JobAction {
    Wait,
    Poll,
    Kill,
}

#[derive(Deserialize, Serialize, JsonSchema)]
pub struct Screenshot {
    /// To zoom in, give x, y, width, and height: an area of the full screenshot, in its pixels. Seven MCP shows that area
    /// larger, up to the real pixels of the screen.
    pub x: Option<f64>,
    pub y: Option<f64>,
    pub width: Option<f64>,
    pub height: Option<f64>,
}

#[derive(Deserialize, Serialize, JsonSchema)]
pub struct Input {
    pub action: InputAction,
    /// Moves the mouse here first
    pub x: Option<f64>,
    pub y: Option<f64>,
    /// For type: the text
    pub text: Option<String>,
    /// For key: key names joined with +, such as ctrl+s, alt+f4, win+r, enter, or f5
    pub keys: Option<String>,
    /// For scroll: notches. Positive scrolls up. Default -3.
    pub amount: Option<i32>,
}

#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InputAction {
    Move,
    Click,
    DoubleClick,
    RightClick,
    Scroll,
    Type,
    Key,
}

#[derive(Deserialize, Serialize, JsonSchema)]
pub struct Notify {
    pub title: String,
    pub text: String,
}

#[derive(Deserialize, Serialize, JsonSchema)]
pub struct Dialog {
    pub title: String,
    /// The message. Keep it short.
    pub text: String,
    /// 1 to 3 button labels, from left to right. The first is the default. Use labels that tell what each button does,
    /// such as "Install now" and "Not now". Default: OK.
    #[schemars(length(min = 1, max = 3))]
    pub buttons: Option<Vec<String>>,
    /// Default info
    pub icon: Option<Icon>,
    /// Show a text box for the user to type an answer
    pub input: Option<bool>,
    /// Seconds to wait before the box continues as a job. Default 60.
    pub timeout: Option<u64>,
}

#[derive(Deserialize, Serialize, JsonSchema, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum Icon {
    Info,
    Warning,
    Error,
}

#[derive(Deserialize, Serialize, JsonSchema)]
pub struct Fetch {
    pub url: String,
    /// Full path of the file to write. Default: the Downloads folder, with the name from the URL.
    pub path: Option<String>,
    /// Seconds to wait before the download continues as a job. Default 60.
    pub timeout: Option<u64>,
}

// the result of run, fetch, dialog, and job
#[derive(Serialize, JsonSchema, Default)]
pub struct JobResult {
    /// New output since the last call
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub output: String,
    /// Bytes of earlier output that Seven MCP removed, because no app read them in time
    pub removed_bytes: Option<usize>,
    /// The job number while the job runs. Use the job tool to wait for it, get new output, or kill it.
    pub running_job: Option<u32>,
    /// run: the exit code of the command
    pub exit_code: Option<i32>,
    /// fetch: the downloaded file
    pub file: Option<File>,
    /// dialog: what the user did
    pub answer: Option<Answer>,
    /// A message from Seven MCP, such as why the job stopped
    pub note: Option<String>,
}

#[derive(Serialize, JsonSchema)]
pub struct File {
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Serialize, JsonSchema)]
pub struct Answer {
    /// The label of the button that the user clicked. Not given if the user closed the box.
    pub button: Option<String>,
    /// The text that the user typed, if the box had a text box
    pub text: Option<String>,
}

#[derive(Serialize, JsonSchema)]
pub struct Info {
    pub computer: String,
    /// Such as "Windows 7 Ultimate Service Pack 1"
    pub windows: String,
    /// 6.0 is Vista, 6.1 is Windows 7
    pub version: String,
    pub build: String,
    /// The architecture of Windows: x86 or AMD64
    pub architecture: String,
    /// The architecture of Seven MCP: x86 or x64
    pub seven_mcp_architecture: String,
    /// If false, Seven MCP can't control programs that run as administrator
    pub runs_as_administrator: bool,
    pub screen_width: i32,
    pub screen_height: i32,
    /// Devices that have problems, with their hardware IDs to find drivers
    pub problem_devices: Vec<Device>,
    /// Hints about the tools of this Windows version
    pub hints: Vec<String>,
}

#[derive(Serialize, JsonSchema)]
pub struct Device {
    pub name: String,
    pub id: String,
    /// The Device Manager problem code
    pub problem: u32,
    /// What the problem code means, if Seven MCP knows it
    pub meaning: Option<String>,
}

#[derive(Clone)]
pub struct Seven;

#[tool_router]
impl Seven {
    #[tool(
        title = "Run a command",
        output_schema = schema_for_output::<JobResult>(),
        description = "Run a command line with cmd.exe and return its output and exit code. If the command doesn't stop in \
            `timeout` seconds, it continues as a job: use the job tool to get its new output, wait for it, or stop it. \
            Commands get no input, so give them options that stop prompts, such as /y or /quiet. For PowerShell 2.0, run \
            `powershell -NoProfile -Command ...`."
    )]
    async fn run(&self, Parameters(a): Parameters<Run>, ctx: RequestContext<RoleServer>) -> CallToolResult {
        blocking("run", ctx, a, |a, call, p| tools::first_wait(tools::run(&a.command, call)?, a.timeout, call, p).map(structured)).await
    }

    #[tool(
        title = "Control a job",
        output_schema = schema_for_output::<JobResult>(),
        description = "Get the new output of a job that the run, fetch, or dialog tool started. `wait` waits until the job \
            stops or `timeout` seconds pass. `poll` doesn't wait. `kill` stops the job: for run, with all the programs that it \
            started; for fetch, it also deletes the partial file; for dialog, it closes the dialog box."
    )]
    async fn job(&self, Parameters(a): Parameters<Job>, ctx: RequestContext<RoleServer>) -> CallToolResult {
        blocking("job", ctx, a, |a, call, p| tools::job(&a, call, p).map(structured)).await
    }

    #[tool(
        title = "Take a screenshot",
        description = "Take a screenshot of the main screen as a PNG image, at most 1280 pixels on the longest side. The \
            input tool uses the pixels of this image. If text is too small to read, zoom in on an area. Seven MCP can't \
            capture the screen when the computer is locked or a UAC prompt is open.",
        annotations(read_only_hint = true)
    )]
    async fn screenshot(&self, Parameters(a): Parameters<Screenshot>, ctx: RequestContext<RoleServer>) -> CallToolResult {
        blocking("screenshot", ctx, a, |a, _, _| tools::screenshot(&a).map(CallToolResult::success)).await
    }

    #[tool(
        title = "Use the mouse and keyboard",
        description = "Move or click the mouse, scroll, type text, or press keys. Coordinates are in pixels of the \
            screenshot image. If a program runs as administrator, Windows blocks input to it unless Seven MCP also runs as \
            administrator. Windows doesn't let programs send Ctrl+Alt+Del or click UAC prompts."
    )]
    async fn input(&self, Parameters(a): Parameters<Input>, ctx: RequestContext<RoleServer>) -> CallToolResult {
        blocking("input", ctx, a, |a, _, _| tools::input(&a).map(|()| done())).await
    }

    #[tool(
        title = "Show a notification",
        description = "Show a balloon from the Seven MCP icon in the notification area, to tell the user something.",
        annotations(destructive_hint = false, open_world_hint = false)
    )]
    async fn notify(&self, Parameters(a): Parameters<Notify>, ctx: RequestContext<RoleServer>) -> CallToolResult {
        blocking("notify", ctx, a, |a, _, _| {
            ui::notify(&a.title, &a.text);
            Ok(done())
        })
        .await
    }

    #[tool(
        title = "Show a dialog box",
        output_schema = schema_for_output::<JobResult>(),
        description = "Show a dialog box on the screen and wait for the user to click a button. Use it to ask the user a \
            question, or to tell the user a result that they must see, as when no person watches your own computer. The box \
            can also have a text box for an answer. If the user doesn't answer in `timeout` seconds, the box stays open as a \
            job: use the job tool to wait for the answer, or to close the box.",
        annotations(destructive_hint = false, open_world_hint = false)
    )]
    async fn dialog(&self, Parameters(a): Parameters<Dialog>, ctx: RequestContext<RoleServer>) -> CallToolResult {
        blocking("dialog", ctx, a, |a, call, p| tools::first_wait(tools::dialog(&a, call)?, a.timeout, call, p).map(structured)).await
    }

    #[tool(
        title = "Download a file",
        output_schema = schema_for_output::<JobResult>(),
        description = "Download a URL to a file. Seven MCP has its own TLS and root certificates, so it can open modern \
            HTTPS sites that Windows 7 can't. Returns the size and SHA-256 of the file. If the download doesn't stop in \
            `timeout` seconds, it continues as a job: use the job tool to wait for it or stop it.",
        annotations(open_world_hint = true)
    )]
    async fn fetch(&self, Parameters(a): Parameters<Fetch>, ctx: RequestContext<RoleServer>) -> CallToolResult {
        blocking("fetch", ctx, a, |a, call, p| tools::first_wait(tools::fetch(&a, call)?, a.timeout, call, p).map(structured)).await
    }

    #[tool(
        title = "Get computer information",
        output_schema = schema_for_output::<Info>(),
        description = "Get the Windows version, the architecture, the administrator state, the screen size, and the devices \
            that have problems, with their hardware IDs to find drivers. Also gives hints about the tools of this Windows version.",
        annotations(read_only_hint = true)
    )]
    async fn info(&self, ctx: RequestContext<RoleServer>) -> CallToolResult {
        blocking("info", ctx, json!({}), |_, _, _| Ok(structured(tools::info()))).await
    }
}

#[tool_handler]
impl ServerHandler for Seven {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("Seven MCP", env!("VERSION")))
            .with_instructions(INSTRUCTIONS)
    }
}

// the tools wait on windows and on jobs, so they run on tokio's blocking threads. the window logs each call
async fn blocking<A: Serialize + Send + 'static>(
    name: &'static str,
    ctx: RequestContext<RoleServer>,
    args: A,
    tool: fn(A, ui::Call, tools::Progress) -> Result<CallToolResult, String>,
) -> CallToolResult {
    let mut detail = serde_json::to_value(&args).unwrap_or_default();
    no_nulls(&mut detail);
    let call = ui::call(name, detail["command"].as_str().map_or(&detail.to_string(), |c| c));
    // the app cancels by closing the request, and rmcp fires this token
    let ct = ctx.ct.clone();
    let watch = tokio::spawn(async move {
        ct.cancelled().await;
        let _ = tokio::task::spawn_blocking(move || tools::cancel(call)).await;
    });
    let progress = progress(ctx);
    let result = tokio::task::spawn_blocking(move || tool(args, call, progress)).await.unwrap_or_else(|e| Err(e.to_string()));
    watch.abort();
    let result = result.unwrap_or_else(|e| CallToolResult::error(vec![ContentBlock::text(e)]));
    // a job shows its own output in the window
    if !matches!(name, "run" | "fetch" | "dialog") {
        call.output(result.content.first().and_then(|c| c.as_text()).map_or("", |t| &t.text));
        call.end(if result.is_error == Some(true) { "Error" } else { "Done" });
    }
    result
}

// typed results go in structuredContent, and as json text for apps that don't read it
fn structured(value: impl Serialize) -> CallToolResult {
    let mut value = serde_json::to_value(value).unwrap_or_default();
    no_nulls(&mut value);
    CallToolResult::structured(value)
}

fn done() -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text("Done.")])
}

// a field that isn't there reads better than null, in results and in the log
fn no_nulls(v: &mut Value) {
    if let Some(o) = v.as_object_mut() {
        o.retain(|_, v| !v.is_null());
        o.values_mut().for_each(no_nulls);
    }
}

// job progress goes out in order as notifications/progress, if the app gave a progress token
fn progress(ctx: RequestContext<RoleServer>) -> tools::Progress {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(u64, Option<u64>, String)>();
    if let Some(token) = ctx.meta.get_progress_token() {
        tokio::spawn(async move {
            while let Some((done, total, message)) = rx.recv().await {
                let p = ProgressNotificationParam::new(token.clone(), done as f64).with_message(message);
                let p = match total {
                    Some(t) => p.with_total(t as f64),
                    None => p,
                };
                let _ = ctx.peer.notify_progress(p).await;
            }
        });
    }
    Arc::new(move |done, total, message| _ = tx.send((done, total, message.into())))
}
