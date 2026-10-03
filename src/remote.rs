//! The opt-in remote control plane used by native Android and the phone web remote.
//!
//! ByteTraverse is the network boundary: the normal listener is
//! `10.30.0.1:7462`, reachable from peers on the ByteTraverse mesh and not
//! from the machine's LAN interface. This module deliberately does not embed
//! ByteTraverse or duplicate its transport protocol. It exposes a small HTTP
//! API over the already-encrypted mesh and adds its own bearer authentication
//! so joining the mesh is not, by itself, authority to control Claude Code.

use crate::config::Config;
use crate::{actions, graph, memory, paths, registry, tmux, usage};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

pub const DEFAULT_BIND: &str = "10.30.0.1:7462";
pub const DEFAULT_ENDPOINT: &str = "http://10.30.0.1:7462";
pub const PHONE_CONNECT_BASE: &str = "https://toomux.com/remote/";
const API: &str = "/api/v1";
const MAX_HEADER: usize = 16 * 1024;
const MAX_BODY: usize = 64 * 1024;
const PAIR_TTL_MS: i64 = 10 * 60_000;
const WEB_PAIR_TTL_MS: i64 = 15 * 60_000;
const MAX_DEVICES: usize = 16;
const TUI_MIN_COLS: u16 = 32;
const TUI_MAX_COLS: u16 = 240;
const TUI_MIN_ROWS: u16 = 18;
const TUI_MAX_ROWS: u16 = 96;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DeviceGrant {
    id: String,
    name: String,
    token_sha256: String,
    created_ms: i64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct AuthBook {
    #[serde(default)]
    devices: Vec<DeviceGrant>,
}

#[derive(Debug, Serialize, Deserialize)]
struct PairGrant {
    code_sha256: String,
    expires_ms: i64,
    #[serde(default)]
    failed_attempts: u8,
}

#[derive(Debug, Serialize, Deserialize)]
struct WebPairGrant {
    invite_sha256: String,
    expires_ms: i64,
}

#[derive(Debug)]
struct Request {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

struct Response {
    status: u16,
    body: Vec<u8>,
}

#[derive(Deserialize)]
struct PairRequest {
    code: String,
    device_name: String,
}

#[derive(Deserialize)]
struct WebPairRequest {
    invite: String,
    device_name: String,
}

#[derive(Deserialize)]
struct PromptRequest {
    text: String,
}

#[derive(Deserialize)]
struct KeysRequest {
    keys: Vec<String>,
}

#[derive(Deserialize)]
struct SessionInputRequest {
    kind: String,
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Deserialize)]
struct TuiInput {
    kind: String,
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    x: Option<u16>,
    #[serde(default)]
    y: Option<u16>,
    #[serde(default)]
    button: Option<String>,
    #[serde(default)]
    delta: Option<i8>,
    #[serde(default)]
    session_id: Option<String>,
}

pub fn pair() -> Result<()> {
    let code = pairing_code()?;
    let grant = PairGrant {
        code_sha256: digest(&code),
        expires_ms: registry::now_ms() + PAIR_TTL_MS,
        failed_attempts: 0,
    };
    write_private(&pair_path(), &serde_json::to_vec_pretty(&grant)?)?;
    println!("{code}");
    eprintln!("pairing code expires in 10 minutes · {DEFAULT_ENDPOINT}");
    Ok(())
}

/// Mint and render the complete browser invitation without printing either raw secret.
///
/// ByteTraverse's home-box owns transport enrollment and writes its secret-bearing invitation to
/// an owner-only file. toomux reads that file locally, combines it with its own independent
/// one-time application invite in the URL fragment, deletes the temporary file, and renders only
/// a QR matrix. No workstation listener is opened by this command.
pub fn phone() -> Result<()> {
    let path = open_phone_qr()?;
    println!("toomux remote · connect a phone");
    println!("opened an exact square pairing QR in your browser · expires in 15 minutes · one use");
    println!("local pairing page: {}", path.display());
    Ok(())
}

/// Mint the purpose-bound browser invitation and return only its terminal QR rendering.
/// The raw transport/application authorities never leave this module.
pub fn phone_qr() -> Result<String> {
    let url = mint_phone_invite()?;
    qr_terminal(&url).context("phone invitation is too large to encode as QR")
}

pub fn open_phone_qr() -> Result<PathBuf> {
    let url = mint_phone_invite()?;
    let html = qr_pairing_page(&url).context("phone invitation is too large to encode as QR")?;
    let path = phone_qr_page_path();
    write_private(&path, html.as_bytes())?;
    crate::actions::open_browser_target(path.as_os_str())?;
    Ok(path)
}

fn mint_phone_invite() -> Result<String> {
    let home = crate::config::home();
    let btv = std::env::var_os("BTV_HOMEBOX_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".bytetraverse/bin/btv-homebox"));
    if !btv.is_file() {
        bail!(
            "ByteTraverse home-box is not installed at {}; install/configure ByteTraverse first",
            btv.display()
        );
    }
    let secret = home.join(".bytetraverse/secret.hex");
    let state = home.join(".bytetraverse/state.json");
    if !secret.is_file() || !state.is_file() {
        bail!(
            "ByteTraverse pairing state is missing; expected {} and {}",
            secret.display(),
            state.display()
        );
    }

    let mut nonce = [0u8; 8];
    random(&mut nonce)?;
    let invite_path = remote_dir().join(format!(".btv-phone-{}", hex(&nonce)));
    if let Some(parent) = invite_path.parent() {
        std::fs::create_dir_all(parent)?;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    }

    let output = Command::new(&btv)
        .args([
            "--secret-file",
            secret.to_string_lossy().as_ref(),
            "--state",
            state.to_string_lossy().as_ref(),
            "--connect-base",
            PHONE_CONNECT_BASE,
            "--mint-ticket-file",
            invite_path.to_string_lossy().as_ref(),
            "--pair-capability",
            "toomux",
        ])
        .output()
        .with_context(|| format!("running {}", btv.display()))?;
    if !output.status.success() {
        let _ = std::fs::remove_file(&invite_path);
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "ByteTraverse could not mint the phone invitation: {}",
            stderr.trim()
        );
    }
    let btv_invite = std::fs::read_to_string(&invite_path)
        .context("reading the owner-only ByteTraverse phone invitation")?;
    let _ = std::fs::remove_file(&invite_path);
    let app_invite = create_web_pair_invite()?;
    compose_phone_invite(btv_invite.trim(), &app_invite)
}

fn compose_phone_invite(btv_invite: &str, app_invite: &str) -> Result<String> {
    let prefix = format!("{PHONE_CONNECT_BASE}#");
    let transport = btv_invite
        .strip_prefix(&prefix)
        .context("ByteTraverse returned an invitation for an unexpected connect origin")?;
    let (secret, ticket) = transport
        .split_once('.')
        .context("ByteTraverse invitation has an unexpected fragment")?;
    if secret.len() != 64
        || ticket.len() != 32
        || !lower_hex(secret)
        || !lower_hex(ticket)
        || app_invite.len() != 64
        || !lower_hex(app_invite)
    {
        bail!("ByteTraverse or toomux invitation has an unexpected secret shape");
    }
    Ok(format!(
        "{PHONE_CONNECT_BASE}#v1.{secret}.{ticket}.{app_invite}"
    ))
}

fn lower_hex(text: &str) -> bool {
    text.bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn qr_terminal(text: &str) -> Option<String> {
    use qrcodegen::{QrCode, QrCodeEcc};
    let qr = QrCode::encode_text(text, QrCodeEcc::Medium).ok()?;
    let n = qr.size();
    const QUIET: i32 = 4;
    let dark =
        |x: i32, y: i32| -> bool { x >= 0 && y >= 0 && x < n && y < n && qr.get_module(x, y) };
    let mut out = String::new();
    let mut y = -QUIET;
    while y < n + QUIET {
        for x in -QUIET..n + QUIET {
            out.push(match (dark(x, y), dark(x, y + 1)) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            });
        }
        out.push('\n');
        y += 2;
    }
    Some(out)
}

fn qr_pairing_page(text: &str) -> Option<String> {
    use qrcodegen::{QrCode, QrCodeEcc};
    let qr = QrCode::encode_text(text, QrCodeEcc::Medium).ok()?;
    let n = qr.size();
    const QUIET: i32 = 4;
    let side = n + QUIET * 2;
    let mut path = String::new();
    for y in 0..n {
        for x in 0..n {
            if qr.get_module(x, y) {
                use std::fmt::Write as _;
                let _ = write!(&mut path, "M{} {}h1v1h-1z", x + QUIET, y + QUIET);
            }
        }
    }
    Some(format!(
        r##"<!doctype html>
<html lang="en">
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<meta name="referrer" content="no-referrer">
<title>toomux remote pairing</title>
<style>
html,body{{margin:0;min-height:100%;background:#121619;color:#f6f5f1}}
body{{min-height:100svh;display:grid;place-items:center;overflow:auto;font:15px/1.45 ui-monospace,SFMono-Regular,Menlo,monospace}}
main{{width:min(92vw,760px);padding:clamp(14px,3vh,28px) 28px;box-sizing:border-box;text-align:center}}
h1{{margin:0 0 8px;font-size:20px;color:#57e6be}}p{{margin:0 0 20px;color:#aeb3af}}
svg{{display:block;width:min(78vmin,680px,max(96px,calc(100svh - 190px)));height:auto;aspect-ratio:1/1;margin:0 auto;background:#fff;
image-rendering:pixelated;shape-rendering:crispEdges}}
.note{{margin:clamp(10px,2vh,18px) auto 0;max-width:48ch;font-size:12px;color:#687178}}
</style>
<main>
<h1>connect a phone</h1>
<p>Open toomux.com/remote/ on the phone and scan this one-use code.</p>
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {side} {side}" preserveAspectRatio="xMidYMid meet"
role="img" aria-label="toomux remote pairing QR">
<rect width="{side}" height="{side}" fill="#fff"/>
<path d="{path}" fill="#000"/>
</svg>
<div class="note">Expires in 15 minutes. The pairing authority is stored only in this owner-only local file and the QR itself.</div>
</main>
</html>"##
    ))
}

/// Create the application-authority half of a browser invitation.
///
/// The caller must keep the returned 256-bit value out of argv/stdout/history and carry it only
/// inside the QR/deep-link fragment. The host stores only its SHA-256. This is deliberately
/// separate from ByteTraverse enrollment: transport reachability is not toomux authority.
pub fn create_web_pair_invite() -> Result<String> {
    let mut bytes = [0u8; 32];
    random(&mut bytes)?;
    let invite = hex(&bytes);
    let grant = WebPairGrant {
        invite_sha256: digest(&invite),
        expires_ms: registry::now_ms() + WEB_PAIR_TTL_MS,
    };
    write_private(&web_pair_path(), &serde_json::to_vec_pretty(&grant)?)?;
    Ok(invite)
}

pub fn devices() -> Result<()> {
    let book = load_book()?;
    if book.devices.is_empty() {
        println!("no remote device is paired");
        return Ok(());
    }
    for d in book.devices {
        println!("{}\t{}\t{}", d.id, d.name, d.created_ms);
    }
    Ok(())
}

pub fn revoke(id: &str) -> Result<()> {
    let mut book = load_book()?;
    let before = book.devices.len();
    if id == "all" {
        for d in &book.devices {
            kill_tui(&d.token_sha256);
        }
        book.devices.clear();
    } else {
        if let Some(d) = book.devices.iter().find(|d| d.id == id) {
            kill_tui(&d.token_sha256);
        }
        book.devices.retain(|d| d.id != id);
    }
    if book.devices.len() == before {
        bail!("no paired device named {id}");
    }
    save_book(&book)?;
    println!("revoked {id}");
    Ok(())
}

pub fn serve(cfg: Config, bind: &str) -> Result<()> {
    let addr: SocketAddr = bind
        .parse()
        .context("--bind must be an IP address and port")?;
    if !safe_bind(addr.ip()) {
        bail!(
            "remote only binds loopback or ByteTraverse 10.30.0.0/16 addresses; got {}",
            addr.ip()
        );
    }
    let listener = TcpListener::bind(addr).with_context(|| format!("listening on {addr}"))?;
    eprintln!("toomux remote · {addr} · ByteTraverse only");
    for incoming in listener.incoming() {
        let mut stream = match incoming {
            Ok(s) => s,
            Err(e) => {
                eprintln!("remote accept: {e}");
                continue;
            }
        };
        let peer = match stream.peer_addr() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("remote peer: {e}");
                continue;
            }
        };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(8)));
        let response = if !safe_peer(addr.ip(), peer.ip()) {
            json_response(
                403,
                json!({"error":"peer is outside the ByteTraverse mesh"}),
            )
        } else {
            match read_request(&mut stream) {
                Ok(req) => route(&cfg, req),
                Err(e) => json_response(400, json!({"error": e.to_string()})),
            }
        };
        if let Err(e) = write_response(&mut stream, response) {
            eprintln!("remote write to {peer}: {e}");
        }
    }
    Ok(())
}

fn route(cfg: &Config, req: Request) -> Response {
    let path_only = req.path.split('?').next().unwrap_or(&req.path);
    if req.method == "GET" && path_only == format!("{API}/health") {
        return json_response(
            200,
            json!({"service":"toomux","api":2,"transport":"bytetraverse"}),
        );
    }
    if req.method == "POST" && path_only == format!("{API}/pair") {
        return pair_exchange(&req.body);
    }
    if req.method == "POST" && path_only == format!("{API}/web/pair") {
        return web_pair_exchange(&req.body);
    }
    if !authorized(&req.headers) {
        return json_response(401, json!({"error":"pair this device first"}));
    }
    if req.method == "POST" && path_only == format!("{API}/device/revoke") {
        return revoke_self(&req.headers);
    }
    if req.method == "GET" && path_only == format!("{API}/snapshot") {
        return json_response(200, snapshot(cfg));
    }
    if req.method == "GET" && path_only == format!("{API}/tui/frame") {
        return tui_frame(&req.headers, &req.path);
    }
    if req.method == "POST" && path_only == format!("{API}/tui/input") {
        return tui_input(&req.headers, &req.body);
    }
    if req.method == "POST" && path_only == format!("{API}/tui/close") {
        return tui_close(&req.headers);
    }
    if req.method == "GET" && path_only == format!("{API}/memory/graph") {
        return memory_graph();
    }
    if req.method == "GET" && path_only == format!("{API}/memory/page") {
        return memory_page();
    }
    let Some(rest) = path_only.strip_prefix(&format!("{API}/sessions/")) else {
        return json_response(404, json!({"error":"not found"}));
    };
    let (target, action) = rest.split_once('/').unwrap_or((rest, ""));
    if target.is_empty() {
        return json_response(404, json!({"error":"session not found"}));
    }
    match (req.method.as_str(), action) {
        ("GET", "screen") => screen(cfg, target, &req.path),
        ("POST", "prompt") => prompt(cfg, target, &req.body),
        ("POST", "keys") => keys(cfg, target, &req.body),
        ("POST", "input") => session_input(cfg, target, &req.body),
        ("POST", "activate") => activate_session(cfg, target),
        _ => json_response(404, json!({"error":"not found"})),
    }
}

fn activate_session(cfg: &Config, target: &str) -> Response {
    let all = registry::load(cfg);
    let s = match registry::find(&all, target) {
        Ok(s) => s,
        Err(e) => return json_response(404, json!({"error":e.to_string()})),
    };
    match actions::jump(s) {
        Ok(()) => json_response(200, json!({"ok":true,"id":s.id,"pid":s.pid})),
        Err(e) => json_response(409, json!({"error":e.to_string()})),
    }
}

fn snapshot(cfg: &Config) -> Value {
    let sessions = registry::load(cfg);
    let now = registry::now_ms();
    let account_usage = usage::summary(cfg, &sessions, now);
    let session_usage = usage::sessions();
    let accounts: Vec<Value> = cfg
        .accounts
        .iter()
        .enumerate()
        .map(|(i, a)| {
            let u = account_usage.get(i);
            json!({
                "name": a.name,
                "five_hour": u.and_then(|x| x.five.as_ref()).map(meter_json),
                "week": u.and_then(|x| x.week.as_ref()).map(meter_json),
                "problem": u.and_then(|x| x.problem.as_ref()),
                "at_ms": u.map(|x| x.at_ms).unwrap_or(0),
            })
        })
        .collect();
    let sessions: Vec<Value> = sessions
        .iter()
        .map(|s| {
            let (state, detail, age) = s.state_parts(now);
            let info = session_usage.get(&s.id);
            json!({
                "id": s.id,
                "title": s.title,
                "topic": s.topic,
                "cwd": s.cwd,
                "place": s.place(),
                "account": s.account_name(cfg),
                "state": state,
                "detail": detail,
                "age": age,
                "waiting_for": s.waiting_for,
                "since_ms": s.since_ms,
                "started_ms": s.started_ms,
                "dormant": s.dormant,
                "pin": s.pin.map(|n| n + 1),
                "pane": s.pane.as_ref().map(|p| p.id.as_str()),
                "context": info.and_then(|i| i.context),
                "tokens": info.and_then(|i| i.tokens),
                "cost": info.and_then(|i| i.cost),
                "model": info.and_then(|i| i.model.as_deref()),
            })
        })
        .collect();
    let hostname = std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "this machine".into());
    json!({
        "api": 1,
        "now_ms": now,
        "host": hostname,
        "transport": "bytetraverse",
        "accounts": accounts,
        "sessions": sessions,
    })
}

fn meter_json(m: &usage::Meter) -> Value {
    json!({"used":m.used,"resets_ms":m.resets_ms,"limited":m.limited})
}

fn screen(cfg: &Config, target: &str, request_path: &str) -> Response {
    let all = registry::load(cfg);
    let s = match registry::find(&all, target) {
        Ok(s) => s,
        Err(e) => return json_response(404, json!({"error":e.to_string()})),
    };
    let Some(pane) = &s.pane else {
        return json_response(409, json!({"error":"session is not in tmux"}));
    };
    let lines = request_path
        .split_once('?')
        .and_then(|(_, q)| q.split('&').find_map(|p| p.strip_prefix("lines=")))
        .and_then(|n| n.parse::<u16>().ok())
        .unwrap_or(80)
        .clamp(10, 200);
    let start = format!("-{lines}");
    match tmux::run(&["capture-pane", "-p", "-t", &pane.id, "-S", &start]) {
        Ok(text) => json_response(
            200,
            json!({"session_id":s.id,"title":s.title,"screen":text,"lines":lines}),
        ),
        Err(e) => json_response(502, json!({"error":e.to_string()})),
    }
}

fn prompt(cfg: &Config, target: &str, body: &[u8]) -> Response {
    let input: PromptRequest = match parse_json(body) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let text = input.text.trim();
    if text.is_empty() || text.len() > 8192 {
        return json_response(400, json!({"error":"prompt must be 1 to 8192 bytes"}));
    }
    let all = registry::load(cfg);
    let s = match registry::find(&all, target) {
        Ok(s) => s,
        Err(e) => return json_response(404, json!({"error":e.to_string()})),
    };
    let Some(pane) = &s.pane else {
        return json_response(409, json!({"error":"session is not in tmux"}));
    };
    if !actions::prompt_empty(&pane.id) {
        return json_response(
            409,
            json!({"error":"Claude's prompt is not empty; use the key controls instead"}),
        );
    }
    match actions::type_prompt(&pane.id, text) {
        Ok(()) => json_response(200, json!({"ok":true})),
        Err(e) => json_response(502, json!({"error":e.to_string()})),
    }
}

fn keys(cfg: &Config, target: &str, body: &[u8]) -> Response {
    let input: KeysRequest = match parse_json(body) {
        Ok(v) => v,
        Err(r) => return r,
    };
    if input.keys.is_empty() || input.keys.len() > 16 || input.keys.iter().any(|k| !allowed_key(k))
    {
        return json_response(400, json!({"error":"keys contains an unsupported key"}));
    }
    let all = registry::load(cfg);
    let s = match registry::find(&all, target) {
        Ok(s) => s,
        Err(e) => return json_response(404, json!({"error":e.to_string()})),
    };
    let Some(pane) = &s.pane else {
        return json_response(409, json!({"error":"session is not in tmux"}));
    };
    for key in &input.keys {
        if let Err(e) = tmux::run(&["send-keys", "-t", &pane.id, key]) {
            return json_response(502, json!({"error":e.to_string()}));
        }
    }
    json_response(200, json!({"ok":true}))
}

fn session_input(cfg: &Config, target: &str, body: &[u8]) -> Response {
    let input: SessionInputRequest = match parse_json(body) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let all = registry::load(cfg);
    let s = match registry::find(&all, target) {
        Ok(s) => s,
        Err(e) => return json_response(404, json!({"error":e.to_string()})),
    };
    let Some(pane) = &s.pane else {
        return json_response(409, json!({"error":"session is not in tmux"}));
    };

    let result = match input.kind.as_str() {
        "key" => {
            let Some(key) = input.key.as_deref() else {
                return json_response(400, json!({"error":"key is required"}));
            };
            if !allowed_tui_key(key) {
                return json_response(400, json!({"error":"unsupported session key"}));
            }
            tmux::run(&["send-keys", "-t", &pane.id, "--", key]).map(|_| ())
        }
        "text" | "submit" => {
            let Some(text) = input.text.as_deref() else {
                return json_response(400, json!({"error":"text is required"}));
            };
            if !valid_remote_text(text) {
                return json_response(400, json!({"error":"text is invalid"}));
            }
            if let Err(e) = tmux::run(&["send-keys", "-t", &pane.id, "-l", "--", text]) {
                return json_response(502, json!({"error":e.to_string()}));
            }
            if input.kind == "submit" {
                tmux::run(&["send-keys", "-t", &pane.id, "--", "Enter"]).map(|_| ())
            } else {
                Ok(())
            }
        }
        _ => return json_response(400, json!({"error":"unsupported session input kind"})),
    };
    match result {
        Ok(()) => json_response(200, json!({"ok":true})),
        Err(e) => json_response(502, json!({"error":e.to_string()})),
    }
}

fn valid_remote_text(text: &str) -> bool {
    !text.is_empty() && text.len() <= 16 * 1024 && !text.chars().any(|c| c == '\0' || c == '\r')
}

fn allowed_key(key: &str) -> bool {
    matches!(
        key,
        "Enter"
            | "Escape"
            | "Tab"
            | "BSpace"
            | "Up"
            | "Down"
            | "Left"
            | "Right"
            | "C-c"
            | "Space"
            | "y"
            | "n"
            | "Y"
            | "N"
            | "1"
            | "2"
            | "3"
            | "4"
            | "5"
            | "6"
            | "7"
            | "8"
            | "9"
    )
}

fn tui_frame(headers: &HashMap<String, String>, request_path: &str) -> Response {
    let Some(device) = token_digest(headers) else {
        return json_response(401, json!({"error":"pair this device first"}));
    };
    let cols = query_param(request_path, "cols")
        .and_then(|v| v.parse::<u16>().ok())
        .unwrap_or(120)
        .clamp(TUI_MIN_COLS, TUI_MAX_COLS);
    let rows = query_param(request_path, "rows")
        .and_then(|v| v.parse::<u16>().ok())
        .unwrap_or(40)
        .clamp(TUI_MIN_ROWS, TUI_MAX_ROWS);
    let pane = match ensure_tui(&device, cols, rows) {
        Ok(pane) => pane,
        Err(e) => return json_response(502, json!({"error":e.to_string()})),
    };
    let server = tui_server(&device);
    let ansi = match tui_tmux(&server, &["capture-pane", "-p", "-e", "-N", "-t", &pane]) {
        Ok(frame) => frame,
        Err(e) => return json_response(502, json!({"error":e.to_string()})),
    };
    let cursor = tui_tmux(
        &server,
        &[
            "display-message",
            "-p",
            "-t",
            &pane,
            "#{cursor_x} #{cursor_y} #{cursor_flag}",
        ],
    )
    .unwrap_or_default();
    let cursor_parts: Vec<u16> = cursor
        .split_whitespace()
        .filter_map(|part| part.parse::<u16>().ok())
        .collect();
    let cursor_x = cursor_parts
        .first()
        .copied()
        .unwrap_or(0)
        .min(cols.saturating_sub(1));
    let cursor_y = cursor_parts
        .get(1)
        .copied()
        .unwrap_or(0)
        .min(rows.saturating_sub(1));
    let cursor_visible = cursor_parts.get(2).copied().unwrap_or(0) != 0;
    let hits = semantic_session_hits(&ansi, cols);
    // Cursor-only movement is part of the rendered surface too. Fold it into
    // the version so left/right navigation is observable even when no cell
    // contents changed.
    let hits_json = serde_json::to_string(&hits).unwrap_or_default();
    let frame_sha256 = digest(&format!(
        "{ansi}\0{cursor_x}\0{cursor_y}\0{}\0{hits_json}",
        u8::from(cursor_visible)
    ));
    let same =
        query_param(request_path, "since").is_some_and(|since| constant_eq(since, &frame_sha256));
    json_response(
        200,
        if same {
            json!({
                "same":true,
                "sha256":frame_sha256,
                "cols":cols,
                "rows":rows,
                "session_hits":hits,
                "cursor":{"x":cursor_x,"y":cursor_y,"visible":cursor_visible},
            })
        } else {
            json!({
                "same":false,
                "sha256":frame_sha256,
                "cols":cols,
                "rows":rows,
                "ansi":ansi,
                "session_hits":hits,
                "cursor":{"x":cursor_x,"y":cursor_y,"visible":cursor_visible},
            })
        },
    )
}

fn semantic_session_hits(ansi: &str, cols: u16) -> Vec<Value> {
    let cfg = Config::load().unwrap_or_default();
    let sessions = registry::load(&cfg);
    let text = strip_ansi_for_hits(ansi);
    let lines: Vec<&str> = text.lines().collect();
    let mut hits = Vec::new();
    for (row, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start_matches(|c: char| c == '▎' || c.is_whitespace());
        for s in &sessions {
            let active_marker = format!("● {}", s.title);
            let attention_marker = format!("◆ {}", s.title);
            let idle_marker = format!("○ {}", s.title);
            if trimmed.contains(&active_marker)
                || trimmed.contains(&attention_marker)
                || trimmed.contains(&idle_marker)
            {
                hits.push(json!({
                    "id":s.id,
                    "pid":s.pid,
                    "row":row + 1,
                    "height":2,
                    "x":1,
                    "width":cols,
                }));
                break;
            }
        }
    }
    hits
}

fn strip_ansi_for_hits(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\u{1b}' {
            out.push(ch);
            continue;
        }
        match chars.peek().copied() {
            Some('[') => {
                chars.next();
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            Some(']') => {
                chars.next();
                let mut esc = false;
                for c in chars.by_ref() {
                    if c == '\u{7}' || (esc && c == '\\') {
                        break;
                    }
                    esc = c == '\u{1b}';
                }
            }
            _ => {
                chars.next();
            }
        }
    }
    out
}

fn tui_input(headers: &HashMap<String, String>, body: &[u8]) -> Response {
    let Some(device) = token_digest(headers) else {
        return json_response(401, json!({"error":"pair this device first"}));
    };
    let input: TuiInput = match parse_json(body) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let server = tui_server(&device);
    let pane = match tui_pane(&server) {
        Ok(pane) => pane,
        Err(e) => return json_response(409, json!({"error":e.to_string()})),
    };
    let result = match input.kind.as_str() {
        "session-activate" => {
            let Some(id) = input.session_id.as_deref() else {
                return json_response(
                    400,
                    json!({"error":"session-activate session_id is required"}),
                );
            };
            let cfg = Config::load().unwrap_or_default();
            let all = registry::load(&cfg);
            if let Err(e) = registry::find(&all, id) {
                return json_response(404, json!({"error":e.to_string()}));
            }
            let geometry = match tui_tmux(
                &server,
                &["display-message", "-p", "-t", &pane, "#{pane_width}"],
            ) {
                Ok(v) => v,
                Err(e) => return json_response(502, json!({"error":e.to_string()})),
            };
            let cols = geometry
                .trim()
                .parse::<u16>()
                .unwrap_or(120)
                .clamp(TUI_MIN_COLS, TUI_MAX_COLS);
            let ansi = match tui_tmux(&server, &["capture-pane", "-p", "-e", "-N", "-t", &pane]) {
                Ok(v) => v,
                Err(e) => return json_response(502, json!({"error":e.to_string()})),
            };
            let hits = semantic_session_hits(&ansi, cols);
            let Some(target) = hits
                .iter()
                .position(|hit| hit.get("id").and_then(Value::as_str) == Some(id))
            else {
                return json_response(
                    409,
                    json!({"error":"session is not selectable in the current TUI view"}),
                );
            };
            let mut keys = vec!["Home"; 1];
            keys.extend(std::iter::repeat_n("Down", target));
            if let Err(e) = tui_tmux(&server, &["send-keys", "-t", &pane, "--", keys[0]]) {
                return json_response(502, json!({"error":e.to_string()}));
            }
            for key in keys.into_iter().skip(1) {
                if let Err(e) = tui_tmux(&server, &["send-keys", "-t", &pane, "--", key]) {
                    return json_response(502, json!({"error":e.to_string()}));
                }
            }
            Ok(())
        }
        "key" => {
            let Some(key) = input.key.as_deref() else {
                return json_response(400, json!({"error":"key is required"}));
            };
            if !allowed_tui_key(key) {
                return json_response(400, json!({"error":"unsupported TUI key"}));
            }
            // The option terminator makes a key name data even if it starts with '-'.
            tui_tmux(&server, &["send-keys", "-t", &pane, "--", key]).map(|_| ())
        }
        "text" => {
            let Some(text) = input.text.as_deref() else {
                return json_response(400, json!({"error":"text is required"}));
            };
            if !valid_remote_text(text) {
                return json_response(400, json!({"error":"text is invalid"}));
            }
            tui_tmux(&server, &["send-keys", "-t", &pane, "-l", "--", text]).map(|_| ())
        }
        "tap" => {
            let (Some(x), Some(y)) = (input.x, input.y) else {
                return json_response(400, json!({"error":"tap needs x and y"}));
            };
            let button = match input.button.as_deref().unwrap_or("left") {
                "left" => 0,
                "right" => 2,
                _ => return json_response(400, json!({"error":"unsupported mouse button"})),
            };
            send_tui_mouse(&server, &pane, button, x, y, true)
        }
        "scroll" => {
            let (Some(x), Some(y), Some(delta)) = (input.x, input.y, input.delta) else {
                return json_response(400, json!({"error":"scroll needs x, y and delta"}));
            };
            let button = if delta > 0 { 64 } else { 65 };
            send_tui_mouse(&server, &pane, button, x, y, false)
        }
        _ => return json_response(400, json!({"error":"unsupported TUI input kind"})),
    };
    match result {
        Ok(()) => json_response(200, json!({"ok":true})),
        Err(e) => json_response(502, json!({"error":e.to_string()})),
    }
}

fn tui_close(headers: &HashMap<String, String>) -> Response {
    let Some(device) = token_digest(headers) else {
        return json_response(401, json!({"error":"pair this device first"}));
    };
    kill_tui(&device);
    json_response(200, json!({"ok":true}))
}

fn memory_graph() -> Response {
    match memory::Memory::open().and_then(|mem| graph::build(&mem)) {
        Ok(graph) => match serde_json::to_value(graph) {
            Ok(value) => json_response(200, value),
            Err(e) => json_response(500, json!({"error":e.to_string()})),
        },
        Err(e) => json_response(500, json!({"error":e.to_string()})),
    }
}

fn memory_page() -> Response {
    match memory::Memory::open()
        .and_then(|mem| graph::build(&mem))
        .and_then(|graph| graph::page(&graph, None))
    {
        Ok(html) => json_response(200, json!({"html":html})),
        Err(e) => json_response(500, json!({"error":e.to_string()})),
    }
}

fn ensure_tui(device: &str, cols: u16, rows: u16) -> Result<String> {
    let server = tui_server(device);
    let alive = crate::tmux::command()
        .args(["-L", &server, "has-session", "-t", "app"])
        .status()
        .is_ok_and(|s| s.success());
    if !alive {
        let exe = std::env::current_exe().context("finding the toomux executable")?;
        let command = format!(
            "TOOMUX_REMOTE_ANDROID=1 {}",
            shell_words::join([exe.display().to_string(), "shell".to_string()])
        );
        tui_tmux(
            &server,
            &[
                "new-session",
                "-d",
                "-x",
                &cols.to_string(),
                "-y",
                &rows.to_string(),
                "-s",
                "app",
                &command,
            ],
        )?;
        let _ = tui_tmux(&server, &["set-option", "-g", "status", "off"]);
        std::thread::sleep(Duration::from_millis(120));
    }
    tui_tmux(
        &server,
        &[
            "resize-window",
            "-t",
            "app",
            "-x",
            &cols.to_string(),
            "-y",
            &rows.to_string(),
        ],
    )?;
    tui_pane(&server)
}

fn tui_server(device: &str) -> String {
    let id = device.get(..12).unwrap_or(device);
    format!("toomux-remote-{id}")
}

fn tui_pane(server: &str) -> Result<String> {
    tui_tmux(server, &["list-panes", "-t", "app", "-F", "#{pane_id}"])
        .map(|s| s.lines().next().unwrap_or("").trim().to_string())
        .and_then(|p| {
            if p.starts_with('%') {
                Ok(p)
            } else {
                bail!("remote TUI is not running")
            }
        })
}

fn tui_tmux(server: &str, args: &[&str]) -> Result<String> {
    let out = crate::tmux::command()
        .arg("-L")
        .arg(server)
        .args(args)
        .output()
        .with_context(|| format!("running tmux {}", args.first().copied().unwrap_or("")))?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

fn kill_tui(device: &str) {
    let server = tui_server(device);
    let _ = crate::tmux::command()
        .args(["-L", &server, "kill-server"])
        .status();
}

fn allowed_tui_key(key: &str) -> bool {
    if key.is_empty() || key.len() > 24 || key.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return false;
    }
    let mut rest = key;
    let mut modifiers = 0;
    while let Some(next) = ["C-", "M-", "S-"]
        .into_iter()
        .find_map(|prefix| rest.strip_prefix(prefix))
    {
        modifiers += 1;
        if modifiers > 3 {
            return false;
        }
        rest = next;
    }
    let named = matches!(
        rest,
        "Enter"
            | "Escape"
            | "Tab"
            | "BTab"
            | "BSpace"
            | "Up"
            | "Down"
            | "Left"
            | "Right"
            | "PageUp"
            | "PageDown"
            | "PPage"
            | "NPage"
            | "Home"
            | "End"
            | "Insert"
            | "Delete"
            | "IC"
            | "DC"
            | "Space"
            | "F1"
            | "F2"
            | "F3"
            | "F4"
            | "F5"
            | "F6"
            | "F7"
            | "F8"
            | "F9"
            | "F10"
            | "F11"
            | "F12"
    );
    let one = rest
        .chars()
        .next()
        .is_some_and(|c| rest.chars().count() == 1 && c.is_ascii_graphic());
    named || one
}

fn send_tui_mouse(
    server: &str,
    pane: &str,
    button: u8,
    x: u16,
    y: u16,
    release: bool,
) -> Result<()> {
    let geometry = tui_tmux(
        server,
        &[
            "display-message",
            "-p",
            "-t",
            pane,
            "#{pane_width} #{pane_height}",
        ],
    )?;
    let mut parts = geometry.split_whitespace();
    let cols = parts
        .next()
        .and_then(|v| v.parse::<u16>().ok())
        .context("reading TUI width")?;
    let rows = parts
        .next()
        .and_then(|v| v.parse::<u16>().ok())
        .context("reading TUI height")?;
    if x == 0 || y == 0 || x > cols || y > rows {
        bail!("mouse coordinate is outside the TUI");
    }
    let mut bytes = format!("\x1b[<{button};{x};{y}M").into_bytes();
    if release {
        bytes.extend(format!("\x1b[<{button};{x};{y}m").into_bytes());
    }
    let hex: Vec<String> = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let mut args: Vec<&str> = vec!["send-keys", "-t", pane, "-H"];
    args.extend(hex.iter().map(String::as_str));
    tui_tmux(server, &args).map(|_| ())
}

fn query_param<'a>(path: &'a str, name: &str) -> Option<&'a str> {
    let query = path.split_once('?')?.1;
    query.split('&').find_map(|part| {
        part.split_once('=')
            .filter(|(k, _)| *k == name)
            .map(|(_, v)| v)
    })
}

fn pair_exchange(body: &[u8]) -> Response {
    let input: PairRequest = match parse_json(body) {
        Ok(v) => v,
        Err(r) => return r,
    };
    if input.code.len() != 8 || !input.code.bytes().all(|b| b.is_ascii_digit()) {
        return json_response(400, json!({"error":"pairing code must be eight digits"}));
    }
    let name = input.device_name.trim();
    if !valid_device_name(name) {
        return json_response(400, json!({"error":"device name is invalid"}));
    }
    let mut grant: PairGrant = match std::fs::read(pair_path())
        .ok()
        .and_then(|b| serde_json::from_slice::<PairGrant>(&b).ok())
    {
        Some(g) if g.expires_ms >= registry::now_ms() => g,
        _ => {
            return json_response(
                410,
                json!({"error":"pairing code expired; run toomux remote pair again"}),
            );
        }
    };
    if !constant_eq(&grant.code_sha256, &digest(&input.code)) {
        grant.failed_attempts = grant.failed_attempts.saturating_add(1);
        if grant.failed_attempts >= 8 {
            let _ = std::fs::remove_file(pair_path());
            return json_response(
                410,
                json!({"error":"pairing code disabled after too many attempts; make a new one"}),
            );
        }
        let encoded = match serde_json::to_vec_pretty(&grant) {
            Ok(bytes) => bytes,
            Err(e) => return json_response(500, json!({"error":e.to_string()})),
        };
        if let Err(e) = write_private(&pair_path(), &encoded) {
            return json_response(500, json!({"error":e.to_string()}));
        }
        return json_response(403, json!({"error":"pairing code is not valid"}));
    }
    let (id, token) = match issue_device(name, "and") {
        Ok(pair) => pair,
        Err(response) => return response,
    };
    let _ = std::fs::remove_file(pair_path());
    json_response(
        200,
        json!({"device_id":id,"token":token,"endpoint":DEFAULT_ENDPOINT,"api":2}),
    )
}

fn web_pair_exchange(body: &[u8]) -> Response {
    let input: WebPairRequest = match parse_json(body) {
        Ok(v) => v,
        Err(r) => return r,
    };
    if input.invite.len() != 64
        || !input
            .invite
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return json_response(400, json!({"error":"web pairing invite is invalid"}));
    }
    let name = input.device_name.trim();
    if !valid_device_name(name) {
        return json_response(400, json!({"error":"device name is invalid"}));
    }
    let grant: WebPairGrant = match std::fs::read(web_pair_path())
        .ok()
        .and_then(|b| serde_json::from_slice::<WebPairGrant>(&b).ok())
    {
        Some(g) if g.expires_ms >= registry::now_ms() => g,
        _ => {
            let _ = std::fs::remove_file(web_pair_path());
            let _ = std::fs::remove_file(phone_qr_page_path());
            return json_response(
                410,
                json!({"error":"web pairing invite expired; make a new phone QR"}),
            );
        }
    };
    if !constant_eq(&grant.invite_sha256, &digest(&input.invite)) {
        return json_response(403, json!({"error":"web pairing invite is not valid"}));
    }

    // Match the Android ceremony: lack of durable device capacity is recoverable operator state,
    // not a reason to burn an otherwise-valid one-time invitation. The remote server handles one
    // request at a time, and issue_device repeats this check before the durable write.
    match load_book() {
        Ok(book) if book.devices.len() >= MAX_DEVICES => {
            return json_response(
                409,
                json!({"error":"too many paired devices; revoke one first"}),
            );
        }
        Ok(_) => {}
        Err(e) => return json_response(500, json!({"error":e.to_string()})),
    }

    // Spend before issuing authority. A storage failure after this point is inconvenient but
    // fail-closed: the same QR can never be replayed to mint a second device token.
    if let Err(e) = std::fs::remove_file(web_pair_path()) {
        return json_response(
            500,
            json!({"error":format!("spending web pairing invite: {e}")}),
        );
    }
    let _ = std::fs::remove_file(phone_qr_page_path());
    let (id, token) = match issue_device(name, "web") {
        Ok(pair) => pair,
        Err(response) => return response,
    };
    json_response(
        200,
        json!({"device_id":id,"token":token,"transport":"bytetraverse","api":2}),
    )
}

fn valid_device_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && !name.chars().any(char::is_control)
}

fn issue_device(name: &str, id_prefix: &str) -> std::result::Result<(String, String), Response> {
    let mut token_bytes = [0u8; 32];
    if let Err(e) = random(&mut token_bytes) {
        return Err(json_response(500, json!({"error":e.to_string()})));
    }
    let token = hex(&token_bytes);
    let token_sha256 = digest(&token);
    let id = format!("{id_prefix}_{}", &token_sha256[..12]);
    let mut book = load_book().map_err(|e| json_response(500, json!({"error":e.to_string()})))?;
    if book.devices.len() >= MAX_DEVICES {
        return Err(json_response(
            409,
            json!({"error":"too many paired devices; revoke one first"}),
        ));
    }
    book.devices.retain(|d| d.id != id);
    book.devices.push(DeviceGrant {
        id: id.clone(),
        name: name.to_string(),
        token_sha256,
        created_ms: registry::now_ms(),
    });
    save_book(&book).map_err(|e| json_response(500, json!({"error":e.to_string()})))?;
    Ok((id, token))
}

fn authorized(headers: &HashMap<String, String>) -> bool {
    let Some(got) = token_digest(headers) else {
        return false;
    };
    load_book().is_ok_and(|book| {
        book.devices
            .iter()
            .any(|d| constant_eq(&d.token_sha256, &got))
    })
}

fn token_digest(headers: &HashMap<String, String>) -> Option<String> {
    let token = headers
        .get("authorization")
        .and_then(|h| h.strip_prefix("Bearer "))?;
    if token.len() != 64 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some(digest(token))
}

fn revoke_self(headers: &HashMap<String, String>) -> Response {
    let Some(got) = token_digest(headers) else {
        return json_response(401, json!({"error":"pair this device first"}));
    };
    let mut book = match load_book() {
        Ok(book) => book,
        Err(e) => return json_response(500, json!({"error":e.to_string()})),
    };
    let before = book.devices.len();
    book.devices.retain(|d| !constant_eq(&d.token_sha256, &got));
    if book.devices.len() == before {
        return json_response(401, json!({"error":"device is no longer paired"}));
    }
    match save_book(&book) {
        Ok(()) => {
            kill_tui(&got);
            json_response(200, json!({"ok":true}))
        }
        Err(e) => json_response(500, json!({"error":e.to_string()})),
    }
}

fn parse_json<T: for<'de> Deserialize<'de>>(body: &[u8]) -> std::result::Result<T, Response> {
    serde_json::from_slice(body)
        .map_err(|_| json_response(400, json!({"error":"request body is not valid JSON"})))
}

fn read_request(stream: &mut TcpStream) -> Result<Request> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut first = String::new();
    reader.read_line(&mut first)?;
    if first.len() > MAX_HEADER || !first.ends_with('\n') {
        bail!("invalid request line");
    }
    let mut parts = first.trim_end_matches(['\r', '\n']).split_whitespace();
    let method = parts.next().context("missing method")?.to_string();
    let path = parts.next().context("missing path")?.to_string();
    if parts.next() != Some("HTTP/1.1") || parts.next().is_some() {
        bail!("HTTP/1.1 is required");
    }
    if !matches!(method.as_str(), "GET" | "POST") || !path.starts_with('/') {
        bail!("unsupported request");
    }
    let mut headers = HashMap::new();
    let mut used = first.len();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        used += line.len();
        if used > MAX_HEADER {
            bail!("request headers are too large");
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        let (name, value) = line
            .trim_end_matches(['\r', '\n'])
            .split_once(':')
            .context("malformed header")?;
        let name = name.trim().to_ascii_lowercase();
        if name.is_empty() || headers.contains_key(&name) {
            bail!("duplicate or empty header");
        }
        headers.insert(name, value.trim().to_string());
    }
    if headers.contains_key("transfer-encoding") {
        bail!("chunked requests are not accepted");
    }
    let len = headers
        .get("content-length")
        .map(|v| v.parse::<usize>())
        .transpose()
        .context("invalid content-length")?
        .unwrap_or(0);
    if len > MAX_BODY {
        bail!("request body is too large");
    }
    if method == "POST" && len == 0 {
        bail!("POST requests need a body");
    }
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body)?;
    Ok(Request {
        method,
        path,
        headers,
        body,
    })
}

fn write_response(stream: &mut TcpStream, response: Response) -> Result<()> {
    let reason = match response.status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        410 => "Gone",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        _ => "Error",
    };
    write!(
        stream,
        "HTTP/1.1 {} {}\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\nX-Content-Type-Options: nosniff\r\n\r\n",
        response.status,
        reason,
        response.body.len()
    )?;
    stream.write_all(&response.body)?;
    stream.flush()?;
    Ok(())
}

fn json_response(status: u16, value: Value) -> Response {
    Response {
        status,
        body: serde_json::to_vec(&value).unwrap_or_else(|_| b"{\"error\":\"json\"}".to_vec()),
    }
}

fn safe_bind(ip: IpAddr) -> bool {
    ip.is_loopback() || matches!(ip, IpAddr::V4(v) if byte_traverse(v))
}

fn safe_peer(bind: IpAddr, peer: IpAddr) -> bool {
    if bind.is_loopback() {
        return peer.is_loopback();
    }
    matches!(peer, IpAddr::V4(v) if byte_traverse(v))
}

fn byte_traverse(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    o[0] == 10 && o[1] == 30
}

fn remote_dir() -> PathBuf {
    paths::state().join("remote")
}

fn pair_path() -> PathBuf {
    remote_dir().join("pair.json")
}

fn web_pair_path() -> PathBuf {
    remote_dir().join("web-pair.json")
}

fn phone_qr_page_path() -> PathBuf {
    remote_dir().join("phone-pair.html")
}

fn auth_path() -> PathBuf {
    remote_dir().join("devices.json")
}

fn load_book() -> Result<AuthBook> {
    match std::fs::read(auth_path()) {
        Ok(bytes) => serde_json::from_slice(&bytes).context("reading paired devices"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(AuthBook::default()),
        Err(e) => Err(e).context("reading paired devices"),
    }
}

fn save_book(book: &AuthBook) -> Result<()> {
    write_private(&auth_path(), &serde_json::to_vec_pretty(book)?)
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().context("remote state path has no parent")?;
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    let tmp = dir.join(format!(
        ".{}.{}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    let mut f = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

fn random(out: &mut [u8]) -> Result<()> {
    let mut f = File::open("/dev/urandom").context("opening the OS random source")?;
    f.read_exact(out).context("reading the OS random source")
}

fn pairing_code() -> Result<String> {
    // Rejection sampling avoids modulo bias: 4.2 billion is the largest
    // multiple of 100 million below 2^32.
    const SPACE: u32 = 100_000_000;
    const ACCEPT_BELOW: u32 = 4_200_000_000;
    loop {
        let mut bytes = [0u8; 4];
        random(&mut bytes)?;
        let n = u32::from_be_bytes(bytes);
        if n < ACCEPT_BELOW {
            return Ok(format!("{:08}", n % SPACE));
        }
    }
}

fn digest(text: &str) -> String {
    let mut h = Sha256::new();
    h.update(text.as_bytes());
    hex(&h.finalize())
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{b:02x}");
    }
    out
}

fn constant_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.as_bytes()
        .iter()
        .zip(b.as_bytes())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_boundary_is_bytetraverse_or_loopback() {
        assert!(safe_bind("10.30.0.1".parse().unwrap()));
        assert!(safe_bind("127.0.0.1".parse().unwrap()));
        assert!(!safe_bind("0.0.0.0".parse().unwrap()));
        assert!(!safe_bind("192.168.50.3".parse().unwrap()));
        assert!(safe_peer(
            "10.30.0.1".parse().unwrap(),
            "10.30.0.3".parse().unwrap()
        ));
        assert!(!safe_peer(
            "10.30.0.1".parse().unwrap(),
            "192.168.50.3".parse().unwrap()
        ));
    }

    #[test]
    fn remote_keys_are_intentionally_narrow() {
        for key in ["Enter", "Escape", "Down", "C-c", "1", "y"] {
            assert!(allowed_key(key), "{key}");
        }
        for key in ["C-z", "F12", "run-shell", "a;b", ""] {
            assert!(!allowed_key(key), "{key}");
        }
    }

    #[test]
    fn digest_comparison_checks_every_byte() {
        let a = digest("one");
        assert!(constant_eq(&a, &a));
        assert!(!constant_eq(&a, &digest("two")));
        assert!(!constant_eq(&a, "short"));
    }

    #[test]
    fn pairing_codes_are_eight_decimal_digits() {
        for _ in 0..32 {
            let code = pairing_code().unwrap();
            assert_eq!(code.len(), 8);
            assert!(code.bytes().all(|b| b.is_ascii_digit()));
        }
    }

    #[test]
    fn web_pair_invites_are_full_entropy_hex_not_human_codes() {
        for _ in 0..16 {
            let mut bytes = [0u8; 32];
            random(&mut bytes).unwrap();
            let invite = hex(&bytes);
            assert_eq!(invite.len(), 64);
            assert!(
                invite
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            );
        }
    }

    #[test]
    fn phone_invite_keeps_both_secrets_in_the_fragment() {
        let secret = "a".repeat(64);
        let ticket = "b".repeat(32);
        let app = "c".repeat(64);
        let btv = format!("{PHONE_CONNECT_BASE}#{secret}.{ticket}");
        let out = compose_phone_invite(&btv, &app).unwrap();
        assert_eq!(
            out,
            format!("{PHONE_CONNECT_BASE}#v1.{secret}.{ticket}.{app}")
        );
        assert!(!out.contains('?'));
        assert!(qr_terminal(&out).is_some());
    }

    #[test]
    fn phone_pairing_page_is_square_vector_geometry_not_terminal_font_geometry() {
        let url = format!(
            "{PHONE_CONNECT_BASE}#v1.{}.{}.{}",
            "a".repeat(64),
            "b".repeat(32),
            "c".repeat(64)
        );
        let page = qr_pairing_page(&url).expect("synthetic invite fits a QR");
        let viewbox = page
            .split("viewBox=\"0 0 ")
            .nth(1)
            .and_then(|s| s.split('"').next())
            .expect("square SVG viewBox");
        let mut dims = viewbox.split_whitespace();
        let width = dims.next().unwrap();
        let height = dims.next().unwrap();
        assert_eq!(width, height, "QR viewBox must be square");
        assert!(page.contains("aspect-ratio:1/1"));
        assert!(page.contains("calc(100svh - 190px)"));
        assert!(page.contains("shape-rendering:crispEdges"));
        assert!(page.contains("preserveAspectRatio=\"xMidYMid meet\""));
        assert!(
            !page.contains(&url),
            "raw invite must not be rendered as visible text"
        );
    }

    #[test]
    fn phone_invite_refuses_another_bootstrap_origin() {
        let foreign = format!(
            "https://example.invalid/remote#{}.{}",
            "a".repeat(64),
            "b".repeat(32)
        );
        assert!(compose_phone_invite(&foreign, &"c".repeat(64)).is_err());
    }

    #[test]
    fn remote_device_names_are_bounded_and_single_line() {
        assert!(valid_device_name("Sam's iPhone"));
        assert!(valid_device_name(&"x".repeat(64)));
        assert!(!valid_device_name(""));
        assert!(!valid_device_name(&"x".repeat(65)));
        assert!(!valid_device_name("phone\nother"));
    }

    #[test]
    fn tui_keys_are_data_not_tmux_commands() {
        for key in ["M-m", "C-x", "S-Tab", "F12", "?", "o"] {
            assert!(allowed_tui_key(key), "{key}");
        }
        for key in [
            "",
            "-T",
            "run-shell",
            "M-run-shell",
            "C-M-S-M-x",
            "two words",
            "\n",
        ] {
            assert!(!allowed_tui_key(key), "{key:?}");
        }
    }

    #[test]
    fn remote_literal_text_is_bounded_and_never_contains_carriage_return_or_nul() {
        assert!(valid_remote_text("echo hello"));
        assert!(valid_remote_text("line one\nline two"));
        assert!(!valid_remote_text(""));
        assert!(!valid_remote_text("bad\rsubmit"));
        assert!(!valid_remote_text("bad\0byte"));
        assert!(!valid_remote_text(&"x".repeat(16 * 1024 + 1)));
    }
}
