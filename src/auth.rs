// pairing: the browser on the other computer sends the pin shown here and gets a bearer token

use crate::{http, settings, ui};
use axum::extract::{Form, Request};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Response};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use ring::digest::{SHA256, digest};
use ring::rand::{SecureRandom, SystemRandom};
use serde_json::json;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime};

const PIN_TTL: Duration = Duration::from_secs(300);
const PIN_TRIES: u8 = 3;
// makes a guess at every pin take hours
const LOCKOUT: Duration = Duration::from_secs(60);

struct Pin {
    code: String,
    tries: u8,
    expires: Instant,
}

struct State {
    // token sha-256 -> (app name, unix time issued)
    tokens: HashMap<String, (String, u64)>,
    pin: Option<Pin>,
    locked_until: Instant,
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(|| {
    Mutex::new(State {
        tokens: settings::get("Tokens").and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default(),
        pin: None,
        locked_until: Instant::now(),
    })
});

fn random<const N: usize>() -> [u8; N] {
    let mut b = [0; N];
    SystemRandom::new().fill(&mut b).expect("no secure random numbers");
    b
}

fn sha256(s: &str) -> String {
    B64.encode(digest(&SHA256, s.as_bytes()))
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn page(body: &str) -> Html<String> {
    Html(format!(
        r#"<!doctype html><meta charset="utf-8"><meta name="viewport" content="width=device-width">
<title>Seven MCP</title>
<style>body{{font:15px "Segoe UI",Tahoma,sans-serif;max-width:40em;margin:2em auto;padding:0 1em}}h1{{margin:0}}h1 img{{width:24em;max-width:100%}}pre{{white-space:pre-wrap;word-break:break-all;background:#f0f0f0;padding:.6em}}</style>
<h1><img src="/logo.png" alt="Seven MCP, by Sineware Labs"></h1>
{body}"#
    ))
}

fn pin_page(name: &str, note: &str) -> Html<String> {
    let computer = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "this computer".into());
    page(&format!(
        r#"<p>To let an MCP client app control <b>{}</b>, enter the PIN shown on that computer.</p>
<form method="post" action="/">
<p><label>Name for the app<br><input name="name" value="{}" placeholder="Claude Code on my laptop" maxlength="60" required autofocus></label></p>
<p><label>PIN<br><input name="pin" inputmode="numeric" autocomplete="one-time-code" maxlength="4" size="5" required></label>
<button>Pair</button></p>
</form>
<p>{note}</p>"#,
        escape(&computer),
        escape(name)
    ))
}

const LOCKED: &str = "<p>Too many wrong PINs. Wait a minute, then reload this page.</p>";

pub async fn start() -> Html<String> {
    let mut s = STATE.lock().unwrap();
    let now = Instant::now();
    if s.locked_until > now {
        return page(LOCKED);
    }
    if s.pin.as_ref().is_none_or(|p| p.expires < now) {
        let code = format!("{:04}", u32::from_le_bytes(random()) % 10000);
        s.pin = Some(Pin { code, tries: 0, expires: now + PIN_TTL });
    }
    ui::notify("MCP Client Pairing Code", &format!("Type this PIN in the web browser: {}", s.pin.as_ref().unwrap().code));
    pin_page("", "")
}

pub async fn finish(headers: HeaderMap, Form(form): Form<HashMap<String, String>>) -> Html<String> {
    let mut s = STATE.lock().unwrap();
    let now = Instant::now();
    if s.locked_until > now {
        return page(LOCKED);
    }
    let Some(pin) = s.pin.as_mut().filter(|p| p.expires > now) else {
        return page("<p>This PIN has expired. Reload this page to get a new one.</p>");
    };
    let name = form.get("name").map_or("", |n| n.trim());
    if form.get("pin") != Some(&pin.code) {
        pin.tries += 1;
        let left = PIN_TRIES - pin.tries;
        if left > 0 {
            return pin_page(name, &format!("That PIN isn't right. You have {left} more tries."));
        }
        s.pin = None;
        s.locked_until = now + LOCKOUT;
        return page(LOCKED);
    }
    s.pin = None;
    let token = B64.encode(random::<32>());
    let issued = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_secs();
    let name = if name.is_empty() { "AI app" } else { name };
    s.tokens.insert(sha256(&token), (name.into(), issued));
    settings::set("Tokens", &json!(s.tokens).to_string());
    ui::paired();

    let url = escape(&format!("{}/mcp", http::base(&headers)));
    page(&format!(
        r#"<p>Paired. Set up your MCP client with one of these. This is the only time Seven MCP shows this token.</p>
<h2>Claude Code</h2>
<pre>claude mcp add --transport http seven {url} --header "Authorization: Bearer {token}"</pre>
<h2>Codex</h2>
<p>Add to ~/.codex/config.toml:</p>
<pre>[mcp_servers.seven]
url = "{url}"
http_headers = {{ Authorization = "Bearer {token}" }}</pre>
<h2>opencode</h2>
<p>Add to the "mcp" section of opencode.json:</p>
<pre>"seven": {{
  "type": "remote",
  "url": "{url}",
  "oauth": false,
  "headers": {{ "Authorization": "Bearer {token}" }}
}}</pre>
<h2>Other apps</h2>
<p>Server URL: {url}<br>Header: Authorization: Bearer {token}</p>"#
    ))
}

// guards /mcp: a browser page from another site, then the token
pub async fn check(req: Request, next: Next) -> Response {
    let base = http::base(req.headers());
    if req.headers().get(header::ORIGIN).is_some_and(|o| o != base.as_str()) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let token = req.headers().get(header::AUTHORIZATION).and_then(|h| h.to_str().ok()?.strip_prefix("Bearer "));
    if token.is_some_and(|t| STATE.lock().unwrap().tokens.contains_key(&sha256(t))) {
        return next.run(req).await;
    }
    let error = json!({ "error": format!("Seven MCP doesn't know this token. Pair again at {base}/") }).to_string();
    (StatusCode::UNAUTHORIZED, [(header::CONTENT_TYPE, "application/json"), (header::WWW_AUTHENTICATE, "Bearer")], error).into_response()
}

// (hash, name, issued), oldest first
pub fn apps() -> Vec<(String, String, u64)> {
    let mut apps: Vec<_> = STATE.lock().unwrap().tokens.iter().map(|(h, (n, t))| (h.clone(), n.clone(), *t)).collect();
    apps.sort_by_key(|a| a.2);
    apps
}

pub fn remove(hash: &str) {
    let mut s = STATE.lock().unwrap();
    s.tokens.remove(hash);
    settings::set("Tokens", &json!(s.tokens).to_string());
}
