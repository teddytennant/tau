//! Account sign-in for ChatGPT and xAI: OAuth 2.0 authorization code with
//! PKCE, the redirect served on the loopback address each client id is
//! registered with, and a paste fallback for machines with no browser.
//!
//! Tokens live in tau's own ~/.tau/auth.json (mode 600). tau never reads or
//! refreshes another program's token files: xAI rotates the refresh token on
//! every grant, so a second program refreshing someone else's session signs
//! that program out. Refreshes take a lock on ~/.tau/auth.json.lock and
//! re-read the file inside it, so two tau processes never spend one grant.
//!
//! Anthropic is API keys only.

use crate::config::{OAuthTokens, load_auth, lock_auth, save_auth};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub struct Account {
    pub name: &'static str,
    pub label: &'static str,
    client_id: &'static str,
    scope: &'static str,
    /// The loopback ports registered for the client id, in order.
    ports: &'static [u16],
    host: &'static str,
    path: &'static str,
    /// Refresh when the access token expires within this many seconds.
    leeway: i64,
}

pub const CHATGPT: Account = Account {
    name: "chatgpt",
    label: "ChatGPT",
    // Codex CLI's public client id; the subscription endpoint only issues
    // tokens to it, which is why wizard and every other harness use it too.
    client_id: "app_EMoamEEZ73f0CkXaXp7hrann",
    scope: "openid profile email offline_access",
    ports: &[1455, 1457],
    host: "localhost",
    path: "/auth/callback",
    leeway: 300,
};

pub const XAI: Account = Account {
    name: "xai-oauth",
    label: "xAI",
    client_id: "b1a00492-073a-47ea-816f-4c329264a828",
    scope: "openid profile email offline_access grok-cli:access api:access",
    ports: &[56121],
    host: "127.0.0.1",
    path: "/callback",
    leeway: 120,
};

pub fn account(name: &str) -> Option<&'static Account> {
    [&CHATGPT, &XAI].into_iter().find(|a| a.name == name)
}

/// Where the ChatGPT subscription API lives (the Responses API).
pub fn chatgpt_base() -> String {
    match test_url() {
        Some(t) => format!("{t}/backend-api/codex"),
        None => "https://chatgpt.com/backend-api/codex".into(),
    }
}

pub fn xai_base() -> String {
    match test_url() {
        Some(t) => format!("{t}/v1"),
        None => "https://api.x.ai/v1".into(),
    }
}

/// A local mock server standing in for the real endpoints. Debug builds only:
/// a release binary always talks to OpenAI and xAI.
fn test_url() -> Option<String> {
    if !cfg!(debug_assertions) {
        return None;
    }
    std::env::var("TAU_OAUTH_TEST_URL")
        .ok()
        .filter(|s| !s.is_empty())
}

fn ports(a: &Account) -> Vec<u16> {
    if cfg!(debug_assertions)
        && let Some(p) = std::env::var("TAU_OAUTH_TEST_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
    {
        return vec![p];
    }
    a.ports.to_vec()
}

// ---------------------------------------------------------------------------
// small pieces: randomness, PKCE, URL encoding, JWT expiry
// ---------------------------------------------------------------------------

pub fn random_bytes(n: usize) -> Result<Vec<u8>> {
    let mut b = vec![0u8; n];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .context("reading /dev/urandom")?;
    Ok(b)
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn b64url(b: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

/// S256 challenge for a verifier (RFC 7636).
pub fn challenge(verifier: &str) -> String {
    use sha2::{Digest, Sha256};
    b64url(&Sha256::digest(verifier.as_bytes()))
}

pub fn new_verifier() -> Result<String> {
    Ok(b64url(&random_bytes(64)?))
}

pub fn encode(s: &str) -> String {
    let mut o = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                o.push(b as char)
            }
            _ => o.push_str(&format!("%{b:02X}")),
        }
    }
    o
}

pub fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let hexval = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => match (hexval(b[i + 1]), hexval(b[i + 2])) {
                (Some(h), Some(l)) => {
                    out.push(h * 16 + l);
                    i += 3;
                    continue;
                }
                _ => out.push(b'%'),
            },
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn query(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", encode(k), encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

pub fn parse_query(q: &str) -> Vec<(String, String)> {
    q.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| match p.split_once('=') {
            Some((k, v)) => (decode(k), decode(v)),
            None => (decode(p), String::new()),
        })
        .collect()
}

fn claims(jwt: &str) -> Option<Value> {
    use base64::Engine;
    let payload = jwt.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// The unverified `exp` of a JWT: a client deciding when to refresh its own
/// token, not a server deciding whether to trust one.
pub fn jwt_exp(token: &str) -> Option<i64> {
    claims(token)?.get("exp")?.as_i64()
}

pub fn account_id(id_token: &str) -> Option<String> {
    claims(id_token)?
        .get("https://api.openai.com/auth")?
        .get("chatgpt_account_id")?
        .as_str()
        .map(String::from)
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// A token with no readable `exp` counts as live; a 401 still forces a refresh.
pub fn expires_soon(token: &str, leeway: i64) -> bool {
    jwt_exp(token).is_some_and(|exp| exp <= now() + leeway)
}

// ---------------------------------------------------------------------------
// endpoints
// ---------------------------------------------------------------------------

/// Credentials only ever go to https on x.ai or a subdomain of it.
pub fn pin_xai(url: &str) -> Result<()> {
    if test_url().is_some_and(|t| url.starts_with(&t)) {
        return Ok(());
    }
    let rest = url
        .strip_prefix("https://")
        .with_context(|| format!("{url} is not https; refusing to send credentials"))?;
    let host = rest.split(['/', '?', ':']).next().unwrap_or("");
    if host == "x.ai" || host.ends_with(".x.ai") {
        Ok(())
    } else {
        bail!("{url} is not on x.ai; refusing to send credentials")
    }
}

struct Endpoints {
    authorize: String,
    token: String,
}

fn endpoints(a: &Account) -> Result<Endpoints> {
    if a.name == CHATGPT.name {
        let base = test_url().unwrap_or_else(|| "https://auth.openai.com".into());
        return Ok(Endpoints {
            authorize: format!("{base}/oauth/authorize"),
            token: format!("{base}/oauth/token"),
        });
    }
    let url = match test_url() {
        Some(t) => format!("{t}/.well-known/openid-configuration"),
        None => "https://auth.x.ai/.well-known/openid-configuration".into(),
    };
    let resp = crate::provider::agent().get(&url).call()?;
    let st = resp.status().as_u16();
    if st >= 300 {
        bail!("xAI discovery at {url} returned HTTP {st}");
    }
    let v: Value = serde_json::from_str(&resp.into_body().read_to_string()?)?;
    let authorize = v["authorization_endpoint"]
        .as_str()
        .context("discovery has no authorization_endpoint")?
        .to_string();
    let token = v["token_endpoint"]
        .as_str()
        .context("discovery has no token_endpoint")?
        .to_string();
    pin_xai(&authorize)?;
    pin_xai(&token)?;
    Ok(Endpoints { authorize, token })
}

// ---------------------------------------------------------------------------
// the sign-in
// ---------------------------------------------------------------------------

pub struct Pending {
    pub account: &'static Account,
    pub url: String,
    pub port: u16,
    state: String,
    verifier: String,
    redirect_uri: String,
    token_endpoint: String,
    listener: TcpListener,
}

fn bind(a: &Account) -> Result<(TcpListener, u16)> {
    let ps = ports(a);
    for &p in &ps {
        if let Ok(l) = TcpListener::bind(("127.0.0.1", p)) {
            return Ok((l, p));
        }
    }
    let list: Vec<String> = ps.iter().map(|p| p.to_string()).collect();
    bail!(
        "could not listen on 127.0.0.1:{} for the {} sign-in. It is the only redirect {} accepts for this client, so close whatever holds it (another sign-in?) and try again",
        list.join(" or "),
        a.label,
        a.label
    )
}

/// Binds the callback port and builds the URL to send the user to.
pub fn begin(a: &'static Account) -> Result<Pending> {
    let (listener, port) = bind(a)?;
    let ep = endpoints(a)?;
    let verifier = new_verifier()?;
    let state = hex(&random_bytes(16)?);
    let redirect_uri = format!("http://{}:{port}{}", a.host, a.path);
    let chal = challenge(&verifier);
    let mut pairs: Vec<(&str, &str)> = vec![
        ("response_type", "code"),
        ("client_id", a.client_id),
        ("redirect_uri", &redirect_uri),
        ("scope", a.scope),
        ("code_challenge", &chal),
        ("code_challenge_method", "S256"),
        ("state", &state),
    ];
    let nonce = hex(&random_bytes(16)?);
    if a.name == CHATGPT.name {
        pairs.extend([
            ("id_token_add_organizations", "true"),
            ("codex_cli_simplified_flow", "true"),
            ("originator", "codex_cli_rs"),
        ]);
    } else {
        // xAI turns away clients that are not allowlisted unless a plan is named
        pairs.extend([
            ("nonce", nonce.as_str()),
            ("plan", "generic"),
            ("referrer", "tau"),
        ]);
    }
    let url = format!("{}?{}", ep.authorize, query(&pairs));
    Ok(Pending {
        account: a,
        url,
        port,
        state,
        verifier,
        redirect_uri,
        token_endpoint: ep.token,
        listener,
    })
}

#[derive(Debug, PartialEq)]
pub enum Callback {
    Code(String),
    Failed(String),
    Ignored,
}

/// Reads a request target like `/callback?code=...&state=...`.
pub fn classify(target: &str, path: &str, state: &str) -> Callback {
    let (p, q) = target.split_once('?').unwrap_or((target, ""));
    if p != path {
        return Callback::Ignored;
    }
    let pairs = parse_query(q);
    let get = |k: &str| pairs.iter().find(|(a, _)| a == k).map(|(_, v)| v.clone());
    if let Some(e) = get("error") {
        let detail = get("error_description").unwrap_or(e);
        if detail.contains("missing_codex_entitlement") {
            return Callback::Failed("this ChatGPT plan does not include Codex access".into());
        }
        return Callback::Failed(format!("the sign-in page returned an error: {detail}"));
    }
    if get("state").as_deref() != Some(state) {
        return Callback::Failed("the sign-in state did not match; start again".into());
    }
    match get("code") {
        Some(c) if !c.is_empty() => Callback::Code(c),
        _ => Callback::Failed("the redirect carried no authorization code".into()),
    }
}

/// Turns whatever was pasted (a full URL, a scheme-less copy, the query on
/// its own, or a bare code) into a callback target. None when it is not one.
pub fn paste_target(line: &str, path: &str, state: &str) -> Option<String> {
    let line = line.trim().trim_matches(['"', '\'']);
    let line = line.split('#').next().unwrap_or(line);
    if line.is_empty() {
        return None;
    }
    let q = match line.split_once('?') {
        Some((_, q)) => q,
        None if line.contains("code=") => line,
        None if line.len() >= 8
            && line
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '~')) =>
        {
            return Some(format!(
                "{path}?{}",
                query(&[("code", line), ("state", state)])
            ));
        }
        None => return None,
    };
    (!q.is_empty()).then(|| format!("{path}?{q}"))
}

/// Advice for a session whose browser is on another machine, or None.
pub fn remote_hint(port: u16) -> Option<String> {
    let get = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let ssh = ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"]
        .iter()
        .any(|k| get(k).is_some());
    let headless = cfg!(not(target_os = "macos"))
        && get("DISPLAY").is_none()
        && get("WAYLAND_DISPLAY").is_none();
    if !ssh && !headless {
        return None;
    }
    let dest = get("SSH_CONNECTION")
        .and_then(|c| c.split_whitespace().nth(2).map(String::from))
        .map(|h| match get("USER") {
            Some(u) => format!("{u}@{h}"),
            None => h,
        })
        .unwrap_or_else(|| "you@this-machine".into());
    Some(format!(
        "No browser here. Either forward the port and open the URL on your own machine:\n  ssh -N -L {port}:127.0.0.1:{port} {dest}\nor open the URL anywhere, let the last redirect fail to load, and paste that page's address here."
    ))
}

fn respond(s: &mut TcpStream, status: &str, msg: &str) {
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>tau</title><body style=\"font:15px system-ui;display:flex;align-items:center;justify-content:center;height:100vh;margin:0\"><p>{}</p>",
        crate::export::escape(msg)
    );
    let _ = write!(
        s,
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

fn read_target(s: &mut TcpStream) -> Option<String> {
    s.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let mut buf = vec![0u8; 8192];
    let mut n = 0;
    while n < buf.len() {
        match s.read(&mut buf[n..]) {
            Ok(0) | Err(_) => break,
            Ok(k) => {
                n += k;
                if buf[..n].windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
        }
    }
    let req = String::from_utf8_lossy(&buf[..n]);
    req.lines()
        .next()?
        .split_whitespace()
        .nth(1)
        .map(String::from)
}

impl Pending {
    /// Waits for the browser's redirect or a pasted one, whichever comes
    /// first, for up to five minutes. `cancel` gives up at once.
    pub fn wait(
        &self,
        paste: Option<&Receiver<String>>,
        cancel: &dyn Fn() -> bool,
        mut on_bad_paste: impl FnMut(),
    ) -> Result<String> {
        self.listener.set_nonblocking(true)?;
        let deadline = Instant::now() + Duration::from_secs(300);
        loop {
            if cancel() {
                bail!("sign-in cancelled");
            }
            if Instant::now() > deadline {
                bail!("timed out waiting for the sign-in (5 minutes)");
            }
            match self.listener.accept() {
                Ok((mut s, _)) => {
                    let _ = s.set_nonblocking(false);
                    if let Some(t) = read_target(&mut s) {
                        match classify(&t, self.account.path, &self.state) {
                            Callback::Ignored => respond(&mut s, "404 Not Found", "Not found."),
                            Callback::Failed(m) => {
                                respond(&mut s, "200 OK", &format!("Sign-in failed: {m}"));
                                bail!(m);
                            }
                            Callback::Code(c) => {
                                respond(
                                    &mut s,
                                    "200 OK",
                                    "Signed in to tau. You can close this tab.",
                                );
                                return Ok(c);
                            }
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e.into()),
            }
            if let Some(rx) = paste {
                while let Ok(line) = rx.try_recv() {
                    match paste_target(&line, self.account.path, &self.state)
                        .map(|t| classify(&t, self.account.path, &self.state))
                    {
                        Some(Callback::Code(c)) => return Ok(c),
                        Some(Callback::Failed(m)) => bail!(m),
                        _ => on_bad_paste(),
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// Trades the code for tokens and saves them.
    pub fn finish(self, code: &str) -> Result<OAuthTokens> {
        let a = self.account;
        let chal = challenge(&self.verifier);
        let mut form: Vec<(&str, &str)> = vec![
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", &self.redirect_uri),
            ("client_id", a.client_id),
            ("code_verifier", &self.verifier),
        ];
        if a.name == XAI.name {
            // xAI also wants the challenge echoed on the exchange
            pin_xai(&self.token_endpoint)?;
            form.extend([
                ("code_challenge", chal.as_str()),
                ("code_challenge_method", "S256"),
            ]);
        }
        let v = post_form(&self.token_endpoint, &form).map_err(|(st, body)| {
            anyhow::anyhow!("{} token exchange failed (HTTP {st}): {body}", a.label)
        })?;
        let t = tokens_from(&v, None, &self.token_endpoint)?;
        store(a.name, &t)?;
        Ok(t)
    }
}

fn tokens_from(v: &Value, old: Option<&OAuthTokens>, endpoint: &str) -> Result<OAuthTokens> {
    let access = v["access_token"]
        .as_str()
        .context("the token response has no access_token")?
        .to_string();
    let id_token = v["id_token"]
        .as_str()
        .map(String::from)
        .or_else(|| old.and_then(|o| o.id_token.clone()));
    Ok(OAuthTokens {
        access_token: access,
        // a refresh that does not rotate keeps the old refresh token
        refresh_token: v["refresh_token"]
            .as_str()
            .map(String::from)
            .or_else(|| old.and_then(|o| o.refresh_token.clone())),
        account_id: id_token
            .as_deref()
            .and_then(account_id)
            .or_else(|| old.and_then(|o| o.account_id.clone())),
        id_token,
        token_endpoint: Some(endpoint.to_string()),
    })
}

fn store(name: &str, t: &OAuthTokens) -> Result<()> {
    crate::config::update_auth(|a| {
        let e = a.entry(name.to_string()).or_default();
        e.oauth = Some(t.clone());
        e.key.clear();
    })
}

/// POSTs a form, returning JSON or (status, body).
fn post_form(url: &str, form: &[(&str, &str)]) -> std::result::Result<Value, (u16, String)> {
    let resp = crate::provider::agent()
        .post(url)
        .header("content-type", "application/x-www-form-urlencoded")
        .header("accept", "application/json")
        .send(query(form))
        .map_err(|e| (0, format!("could not reach {url}: {e}")))?;
    read_json(resp)
}

fn post_json(url: &str, body: &Value) -> std::result::Result<Value, (u16, String)> {
    let resp = crate::provider::agent()
        .post(url)
        .header("accept", "application/json")
        .send_json(body)
        .map_err(|e| (0, format!("could not reach {url}: {e}")))?;
    read_json(resp)
}

fn read_json(resp: ureq::http::Response<ureq::Body>) -> std::result::Result<Value, (u16, String)> {
    let st = resp.status().as_u16();
    let body = resp.into_body().read_to_string().unwrap_or_default();
    if st >= 300 {
        return Err((st, body.chars().take(500).collect()));
    }
    serde_json::from_str(&body).map_err(|e| (st, format!("not JSON: {e}")))
}

// ---------------------------------------------------------------------------
// using and refreshing the tokens
// ---------------------------------------------------------------------------

pub fn signed_in(name: &str) -> bool {
    load_auth()
        .get(name)
        .and_then(|c| c.oauth.as_ref())
        .is_some_and(|t| !t.access_token.is_empty())
}

fn disk(name: &str) -> Option<OAuthTokens> {
    load_auth().get(name).and_then(|c| c.oauth.clone())
}

/// A bearer token (and ChatGPT account id), refreshed first if it is close
/// to expiring.
pub fn bearer(a: &Account) -> Result<(String, Option<String>)> {
    let t = disk(a.name)
        .with_context(|| format!("not signed in to {}; run `tau login {}`", a.label, short(a)))?;
    if !expires_soon(&t.access_token, a.leeway) {
        return Ok((t.access_token, t.account_id));
    }
    let t = refresh(a, &t.access_token)?;
    Ok((t.access_token, t.account_id))
}

/// Refresh after the API said 401 with `rejected`. Skipped when another tau
/// already replaced that token.
pub fn refresh_after_401(a: &Account, rejected: &str) -> Result<(String, Option<String>)> {
    let t = refresh(a, rejected)?;
    Ok((t.access_token, t.account_id))
}

fn short(a: &Account) -> &'static str {
    if a.name == XAI.name { "xai" } else { "chatgpt" }
}

/// Refreshes the grant unless the access token on disk is no longer `stale`,
/// which means another process already did. Runs under the auth lock, so the
/// check and the refresh are one step.
fn refresh(a: &Account, stale: &str) -> Result<OAuthTokens> {
    let _lock = lock_auth(Duration::from_secs(30))?;
    let cur = disk(a.name)
        .with_context(|| format!("not signed in to {}; run `tau login {}`", a.label, short(a)))?;
    if cur.access_token != stale && !expires_soon(&cur.access_token, a.leeway) {
        return Ok(cur);
    }
    let Some(rt) = cur.refresh_token.clone() else {
        bail!(
            "the {} session has no refresh token; run `tau login {}`",
            a.label,
            short(a)
        );
    };
    let endpoint = match &cur.token_endpoint {
        Some(e) if a.name == XAI.name => {
            pin_xai(e)?;
            e.clone()
        }
        Some(e) => e.clone(),
        None => endpoints(a)?.token,
    };
    let res = if a.name == CHATGPT.name {
        post_json(
            &endpoint,
            &json!({"client_id": a.client_id, "grant_type": "refresh_token", "refresh_token": rt}),
        )
    } else {
        post_form(
            &endpoint,
            &[
                ("grant_type", "refresh_token"),
                ("client_id", a.client_id),
                ("refresh_token", &rt),
            ],
        )
    };
    match res {
        Ok(v) => {
            let t = tokens_from(&v, Some(&cur), &endpoint)?;
            // written while still holding the lock, before anyone can read the old grant
            let mut auth = load_auth();
            auth.entry(a.name.to_string()).or_default().oauth = Some(t.clone());
            save_auth(&auth)?;
            Ok(t)
        }
        Err((400 | 401, body)) => {
            let mut auth = load_auth();
            if let Some(e) = auth.get_mut(a.name) {
                e.oauth = None;
            }
            auth.retain(|_, c| c.oauth.is_some() || !c.key.is_empty() || c.base_url.is_some());
            save_auth(&auth)?;
            bail!(
                "the {} session was revoked or expired (HTTP: {body}); run `tau login {}`",
                a.label,
                short(a)
            )
        }
        Err((st, body)) => bail!("{} token refresh failed (HTTP {st}): {body}", a.label),
    }
}

/// The terminal sign-in: print the URL, open a browser if there is one, and
/// take the redirect from the browser or from a paste on stdin.
pub fn login_cli(a: &'static Account, force: bool) -> Result<()> {
    if signed_in(a.name) && !force {
        println!(
            "already signed in to {}. `tau login {} --force` starts a new session.",
            a.label,
            short(a)
        );
        return Ok(());
    }
    let p = begin(a)?;
    println!(
        "Open this URL to sign in with your {} account:\n\n{}\n",
        a.label, p.url
    );
    if let Some(h) = remote_hint(p.port) {
        println!("{h}\n");
    } else {
        for opener in ["xdg-open", "open"] {
            if std::process::Command::new(opener)
                .arg(&p.url)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .is_ok()
            {
                break;
            }
        }
    }
    eprint!("Waiting for the browser (5 minutes). Or paste the redirect URL here: ");
    let _ = std::io::stderr().flush();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut line = String::new();
        loop {
            line.clear();
            match stdin.read_line(&mut line) {
                Ok(0) | Err(_) => return,
                Ok(_) => {
                    if tx.send(line.trim().to_string()).is_err() {
                        return;
                    }
                }
            }
        }
    });
    let code = p.wait(Some(&rx), &|| crate::exec::cancelled(), || {
        eprint!("That is not the redirect URL (it has ?code=...&state=...). Try again: ");
    })?;
    eprintln!();
    p.finish(&code)?;
    let mut s = crate::config::Settings::load();
    s.provider = Some(a.name.to_string());
    s.save()?;
    println!(
        "Signed in to {}. Tokens are in {}.",
        a.label,
        crate::log::tau_home().join("auth.json").display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jwt(payload: Value) -> String {
        format!(
            "{}.{}.sig",
            b64url(b"{\"alg\":\"none\"}"),
            b64url(payload.to_string().as_bytes())
        )
    }

    #[test]
    fn pkce_matches_the_rfc_vector() {
        assert_eq!(
            challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        let v = new_verifier().unwrap();
        assert!((43..=128).contains(&v.len()));
        assert_ne!(v, new_verifier().unwrap());
    }

    #[test]
    fn url_encoding_round_trips() {
        let s = "http://127.0.0.1:1455/auth/callback?x=a b&c/d";
        assert_eq!(decode(&encode(s)), s);
        assert_eq!(encode("a b/c"), "a%20b%2Fc");
        assert_eq!(
            parse_query("code=a%2Bb&state=s+t"),
            [
                ("code".into(), "a+b".into()),
                ("state".into(), "s t".into())
            ]
        );
        assert_eq!(decode("100%"), "100%");
    }

    #[test]
    fn jwt_claims() {
        assert_eq!(jwt_exp(&jwt(json!({"exp": 1234}))), Some(1234));
        assert_eq!(jwt_exp("opaque"), None);
        let id = jwt(json!({"https://api.openai.com/auth": {"chatgpt_account_id": "acct-1"}}));
        assert_eq!(account_id(&id).as_deref(), Some("acct-1"));
        assert!(expires_soon(&jwt(json!({"exp": now() + 60})), 120));
        assert!(!expires_soon(&jwt(json!({"exp": now() + 600})), 120));
        assert!(!expires_soon("opaque", 120));
    }

    #[test]
    fn callback_needs_path_state_and_code() {
        assert_eq!(
            classify("/callback?code=c&state=s", "/callback", "s"),
            Callback::Code("c".into())
        );
        assert!(matches!(
            classify("/callback?code=c&state=x", "/callback", "s"),
            Callback::Failed(_)
        ));
        assert!(matches!(
            classify("/callback?state=s", "/callback", "s"),
            Callback::Failed(_)
        ));
        assert!(matches!(
            classify("/auth/callback?error=x&error_description=missing_codex_entitlement", "/auth/callback", "s"),
            Callback::Failed(m) if m.contains("Codex")
        ));
        assert_eq!(
            classify("/favicon.ico", "/callback", "s"),
            Callback::Ignored
        );
    }

    #[test]
    fn every_shape_of_paste_is_understood() {
        let want = Some("/callback?code=abc&state=s7".to_string());
        for p in [
            "http://127.0.0.1:56121/callback?code=abc&state=s7",
            "127.0.0.1:56121/callback?code=abc&state=s7",
            "  \"http://localhost:56121/other?code=abc&state=s7#\" ",
            "?code=abc&state=s7",
            "code=abc&state=s7",
        ] {
            assert_eq!(paste_target(p, "/callback", "s7"), want, "{p}");
        }
        assert_eq!(
            paste_target("ac_01HQZ-x.y_z~", "/callback", "s7").as_deref(),
            Some("/callback?code=ac_01HQZ-x.y_z~&state=s7")
        );
        for p in [
            "",
            "y",
            "no idea",
            "http://127.0.0.1:1/callback",
            "/callback?",
        ] {
            assert_eq!(paste_target(p, "/callback", "s7"), None, "{p}");
        }
    }

    #[test]
    fn credentials_only_go_to_x_ai() {
        pin_xai("https://auth.x.ai/oauth2/token").unwrap();
        pin_xai("https://x.ai/token").unwrap();
        assert!(pin_xai("http://auth.x.ai/token").is_err());
        assert!(pin_xai("https://notx.ai/token").is_err());
        assert!(pin_xai("https://x.ai.evil.example/token").is_err());
    }
}
