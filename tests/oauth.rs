//! Account sign-in against a local mock of the OAuth and API endpoints. The
//! debug binary honours TAU_OAUTH_TEST_URL and TAU_OAUTH_TEST_PORT; a release
//! build ignores both.

use base64::Engine;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn b64(b: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn jwt(claims: Value) -> String {
    format!(
        "{}.{}.sig",
        b64(b"{\"alg\":\"none\"}"),
        b64(claims.to_string().as_bytes())
    )
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[derive(Default)]
struct State {
    base: String,
    /// code -> (challenge, redirect_uri)
    codes: HashMap<String, (String, String)>,
    /// refresh tokens that may still be spent
    live_refresh: HashSet<String>,
    refreshes: u32,
    exchanges: u32,
    reject_refresh: bool,
    /// access tokens the API accepts
    valid_access: HashSet<String>,
    /// what the exchange saw, for the PKCE assertions
    pkce_ok: bool,
    challenge_echoed: bool,
    api_headers: Vec<HashMap<String, String>>,
    serial: u32,
}

impl State {
    fn issue(&mut self, exp_in: i64) -> Value {
        self.serial += 1;
        let access = jwt(json!({"exp": now() + exp_in, "n": self.serial}));
        let refresh = format!("rt-{}", self.serial);
        self.valid_access.insert(access.clone());
        self.live_refresh.insert(refresh.clone());
        json!({
            "access_token": access,
            "refresh_token": refresh,
            "token_type": "Bearer",
            "id_token": jwt(json!({"https://api.openai.com/auth": {"chatgpt_account_id": "acct-42"}})),
        })
    }
}

struct Req {
    method: String,
    path: String,
    query: String,
    headers: HashMap<String, String>,
    body: String,
}

fn read_req(s: &mut TcpStream) -> Option<Req> {
    let mut r = BufReader::new(s.try_clone().ok()?);
    let mut line = String::new();
    r.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();
    let mut headers = HashMap::new();
    loop {
        let mut h = String::new();
        r.read_line(&mut h).ok()?;
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let n: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0; n];
    r.read_exact(&mut body).ok()?;
    let (path, query) = target.split_once('?').unwrap_or((&target, ""));
    Some(Req {
        method,
        path: path.to_string(),
        query: query.to_string(),
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

fn form(s: &str) -> HashMap<String, String> {
    s.split('&')
        .filter_map(|p| p.split_once('='))
        .map(|(k, v)| (decode(k), decode(v)))
        .collect()
}

fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut o = vec![];
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            o.push(u8::from_str_radix(&s[i + 1..i + 3], 16).unwrap());
            i += 3;
        } else {
            o.push(if b[i] == b'+' { b' ' } else { b[i] });
            i += 1;
        }
    }
    String::from_utf8(o).unwrap()
}

fn reply(s: &mut TcpStream, status: &str, ctype: &str, body: &str) {
    let _ = write!(
        s,
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

fn handle(st: &Arc<Mutex<State>>, mut s: TcpStream) {
    let Some(r) = read_req(&mut s) else { return };
    let base = st.lock().unwrap().base.clone();
    match (r.method.as_str(), r.path.as_str()) {
        ("GET", "/.well-known/openid-configuration") => reply(
            &mut s,
            "200 OK",
            "application/json",
            &json!({"authorization_endpoint": format!("{base}/authorize"), "token_endpoint": format!("{base}/token")}).to_string(),
        ),
        ("GET", "/authorize" | "/oauth/authorize") => {
            // the "browser": sign in, then say where the redirect goes
            let q = form(&r.query);
            let mut g = st.lock().unwrap();
            g.serial += 1;
            let code = format!("code-{}", g.serial);
            g.codes.insert(code.clone(), (q["code_challenge"].clone(), q["redirect_uri"].clone()));
            let loc = format!("{}?code={code}&state={}", q["redirect_uri"], q["state"]);
            reply(&mut s, "200 OK", "text/plain", &loc);
        }
        ("POST", "/token" | "/oauth/token") => {
            let p: HashMap<String, String> = if r.headers.get("content-type").is_some_and(|c| c.contains("json")) {
                serde_json::from_str::<HashMap<String, String>>(&r.body).unwrap()
            } else {
                form(&r.body)
            };
            let mut g = st.lock().unwrap();
            match p["grant_type"].as_str() {
                "authorization_code" => {
                    let Some((chal, uri)) = g.codes.remove(&p["code"]) else {
                        return reply(&mut s, "400 Bad Request", "application/json", r#"{"error":"invalid_grant"}"#);
                    };
                    g.pkce_ok = b64(&Sha256::digest(p["code_verifier"].as_bytes())) == chal && uri == p["redirect_uri"];
                    g.challenge_echoed = p.get("code_challenge") == Some(&chal);
                    g.exchanges += 1;
                    let t = g.issue(3600);
                    reply(&mut s, "200 OK", "application/json", &t.to_string());
                }
                "refresh_token" => {
                    let rt = p["refresh_token"].clone();
                    if g.reject_refresh || !g.live_refresh.remove(&rt) {
                        return reply(&mut s, "400 Bad Request", "application/json", r#"{"error":"invalid_grant"}"#);
                    }
                    g.refreshes += 1;
                    let t = g.issue(3600);
                    drop(g);
                    // slow enough that a second process would arrive mid-refresh
                    std::thread::sleep(Duration::from_millis(300));
                    reply(&mut s, "200 OK", "application/json", &t.to_string());
                }
                _ => reply(&mut s, "400 Bad Request", "text/plain", "grant"),
            }
        }
        ("GET", "/v1/models") => reply(&mut s, "200 OK", "application/json", r#"{"data":[{"id":"grok-4.7","context_window":500000}]}"#),
        ("GET", "/backend-api/codex/models") => reply(&mut s, "200 OK", "application/json", r#"{"data":[{"id":"gpt-5.6-sol"}]}"#),
        ("POST", "/v1/chat/completions" | "/backend-api/codex/responses") => {
            let mut g = st.lock().unwrap();
            g.api_headers.push(r.headers.clone());
            let auth = r.headers.get("authorization").cloned().unwrap_or_default();
            if !g.valid_access.contains(auth.trim_start_matches("Bearer ")) {
                return reply(&mut s, "401 Unauthorized", "application/json", r#"{"error":"bad token"}"#);
            }
            drop(g);
            let body = if r.path.ends_with("completions") {
                "data: {\"choices\":[{\"delta\":{\"content\":\"hello from mock\"}}]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":3}}\n\ndata: [DONE]\n\n".to_string()
            } else if r.body.contains("function_call_output") {
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"done\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":20,\"output_tokens\":1}}}\n\n".to_string()
            } else {
                "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"reasoning\",\"id\":\"rs_1\",\"encrypted_content\":\"gAAA\",\"summary\":[]}}\n\ndata: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"name\":\"bash\",\"arguments\":\"{\\\"command\\\":\\\"echo from-bash\\\"}\",\"call_id\":\"call_1\"}}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}}\n\n".to_string()
            };
            reply(&mut s, "200 OK", "text/event-stream", &body);
        }
        _ => reply(&mut s, "404 Not Found", "text/plain", "no"),
    }
}

fn mock() -> (String, Arc<Mutex<State>>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
    let st = Arc::new(Mutex::new(State {
        base: base.clone(),
        ..Default::default()
    }));
    let st2 = st.clone();
    std::thread::spawn(move || {
        for c in l.incoming().flatten() {
            let st = st2.clone();
            std::thread::spawn(move || handle(&st, c));
        }
    });
    (base, st)
}

fn cmd(home: &Path, base: &str, port: u16, args: &[&str]) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_tau"));
    c.args(args)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap())
        .env("HOME", home)
        .env("TAU_OAUTH_TEST_URL", base)
        .env("TAU_OAUTH_TEST_PORT", port.to_string())
        .env("TAU_OFFLINE", "1")
        .current_dir(home);
    c
}

/// Reads the child's stdout until the sign-in URL shows up.
fn authorize_url(child: &mut Child, base: &str) -> String {
    let out = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let base = base.to_string();
    std::thread::spawn(move || {
        for line in BufReader::new(out).lines().map_while(Result::ok) {
            if line.starts_with(&base) {
                let _ = tx.send(line);
            }
        }
    });
    rx.recv_timeout(Duration::from_secs(20))
        .expect("tau printed the sign-in URL")
}

fn browse(url: &str) -> String {
    ureq::get(url)
        .call()
        .unwrap()
        .into_body()
        .read_to_string()
        .unwrap()
}

fn wait(mut child: Child) -> Output {
    let deadline = Instant::now() + Duration::from_secs(30);
    while child.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "tau did not finish");
        std::thread::sleep(Duration::from_millis(50));
    }
    child.wait_with_output().unwrap()
}

fn auth(home: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(home.join(".tau/auth.json")).unwrap()).unwrap()
}

fn mode(p: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(p).unwrap().permissions().mode() & 0o777
}

#[test]
fn xai_sign_in_through_the_browser_redirect() {
    let d = tempfile::tempdir().unwrap();
    let (base, st) = mock();
    let port = free_port();
    let mut child = cmd(d.path(), &base, port, &["login", "xai"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let url = authorize_url(&mut child, &base);
    assert!(url.contains("code_challenge_method=S256"), "{url}");
    assert!(url.contains("plan=generic"), "{url}");
    let redirect = browse(&url);
    assert!(
        redirect.starts_with(&format!("http://127.0.0.1:{port}/callback?code=")),
        "{redirect}"
    );
    // the browser follows the redirect to tau's listener
    let page = browse(&redirect);
    assert!(page.contains("Signed in to tau"), "{page}");
    let o = wait(child);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));

    let g = st.lock().unwrap();
    assert!(
        g.pkce_ok,
        "the verifier matched the challenge and the redirect_uri matched"
    );
    assert!(
        g.challenge_echoed,
        "xAI's exchange also carries the challenge"
    );
    drop(g);
    let a = auth(d.path());
    assert_eq!(a["xai-oauth"]["oauth"]["refresh_token"], "rt-2");
    assert_eq!(
        a["xai-oauth"]["oauth"]["token_endpoint"],
        format!("{base}/token")
    );
    assert_eq!(mode(&d.path().join(".tau/auth.json")), 0o600);

    // a second login does not replace a live session unless forced
    let o = cmd(d.path(), &base, port, &["login", "xai"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&o.stdout).contains("already signed in"));
}

#[test]
fn chatgpt_sign_in_by_pasting_the_redirect() {
    let d = tempfile::tempdir().unwrap();
    let (base, st) = mock();
    let port = free_port();
    let mut child = cmd(d.path(), &base, port, &["login", "chatgpt"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let url = authorize_url(&mut child, &base);
    assert!(url.contains("originator=codex_cli_rs"), "{url}");
    let redirect = browse(&url);
    assert!(
        redirect.starts_with(&format!("http://localhost:{port}/auth/callback?code=")),
        "{redirect}"
    );
    // over SSH the redirect fails on the laptop; the user pastes its address,
    // with the laptop's host in it, after one bad paste
    let pasted = redirect.replace(&format!("localhost:{port}"), "localhost:1455");
    let mut stdin = child.stdin.take().unwrap();
    writeln!(stdin, "not a url").unwrap();
    writeln!(stdin, "{pasted}").unwrap();
    let o = wait(child);
    let err = String::from_utf8_lossy(&o.stderr).to_string();
    assert!(o.status.success(), "{err}");
    assert!(err.contains("That is not the redirect URL"), "{err}");
    assert!(st.lock().unwrap().pkce_ok);
    let a = auth(d.path());
    assert_eq!(a["chatgpt"]["oauth"]["account_id"], "acct-42");
    assert_eq!(mode(&d.path().join(".tau/auth.json")), 0o600);
}

/// Writes a saved xAI session whose access token expires in `exp_in` seconds.
fn seed(home: &Path, st: &Arc<Mutex<State>>, name: &str, exp_in: i64, valid: bool) {
    let mut g = st.lock().unwrap();
    let access = jwt(json!({"exp": now() + exp_in, "seed": true}));
    if valid {
        g.valid_access.insert(access.clone());
    }
    g.live_refresh.insert("rt-seed".into());
    let base = g.base.clone();
    drop(g);
    fs::create_dir_all(home.join(".tau")).unwrap();
    fs::write(
        home.join(".tau/auth.json"),
        json!({name: {"oauth": {"access_token": access, "refresh_token": "rt-seed", "account_id": "acct-42", "token_endpoint": format!("{base}/{}", if name == "chatgpt" { "oauth/token" } else { "token" })}}}).to_string(),
    )
    .unwrap();
}

#[test]
fn an_expiring_token_is_refreshed_and_the_rotation_saved() {
    let d = tempfile::tempdir().unwrap();
    let (base, st) = mock();
    seed(d.path(), &st, "xai-oauth", 30, true);
    let o = cmd(
        d.path(),
        &base,
        free_port(),
        &["-p", "--provider", "xai-oauth", "hi"],
    )
    .output()
    .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(String::from_utf8_lossy(&o.stdout).trim(), "hello from mock");
    assert_eq!(st.lock().unwrap().refreshes, 1);
    let a = auth(d.path());
    assert_ne!(
        a["xai-oauth"]["oauth"]["refresh_token"], "rt-seed",
        "the rotated token was saved"
    );
    assert_eq!(mode(&d.path().join(".tau/auth.json")), 0o600);
}

#[test]
fn two_processes_spend_one_grant() {
    let d = tempfile::tempdir().unwrap();
    let (base, st) = mock();
    seed(d.path(), &st, "xai-oauth", 30, true);
    let kids: Vec<Child> = (0..3)
        .map(|_| {
            cmd(
                d.path(),
                &base,
                free_port(),
                &["-p", "--provider", "xai-oauth", "hi"],
            )
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
        })
        .collect();
    for k in kids {
        let o = wait(k);
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    }
    // without the lock the second process would spend rt-seed again, get a
    // 400, and sign everyone out
    assert_eq!(st.lock().unwrap().refreshes, 1);
    assert!(auth(d.path())["xai-oauth"]["oauth"]["access_token"].is_string());
}

#[test]
fn a_401_refreshes_once_and_retries() {
    let d = tempfile::tempdir().unwrap();
    let (base, st) = mock();
    // far from expiry, but the server no longer accepts it
    seed(d.path(), &st, "xai-oauth", 3600, false);
    let o = cmd(
        d.path(),
        &base,
        free_port(),
        &["-p", "--provider", "xai-oauth", "hi"],
    )
    .output()
    .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(st.lock().unwrap().refreshes, 1);
}

#[test]
fn a_revoked_grant_signs_out_and_says_how_to_sign_in() {
    let d = tempfile::tempdir().unwrap();
    let (base, st) = mock();
    seed(d.path(), &st, "xai-oauth", 30, true);
    st.lock().unwrap().reject_refresh = true;
    let o = cmd(
        d.path(),
        &base,
        free_port(),
        &["-p", "--provider", "xai-oauth", "hi"],
    )
    .output()
    .unwrap();
    assert!(!o.status.success());
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(err.contains("tau login xai"), "{err}");
    assert!(auth(d.path()).get("xai-oauth").is_none());
}

#[test]
fn chatgpt_requests_carry_the_account_and_run_bash() {
    let d = tempfile::tempdir().unwrap();
    let (base, st) = mock();
    seed(d.path(), &st, "chatgpt", 3600, true);
    let o = cmd(
        d.path(),
        &base,
        free_port(),
        &["-p", "--provider", "chatgpt", "go"],
    )
    .output()
    .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(String::from_utf8_lossy(&o.stdout).trim(), "done");
    let g = st.lock().unwrap();
    assert_eq!(g.api_headers.len(), 2);
    let h = &g.api_headers[0];
    assert_eq!(h["chatgpt-account-id"], "acct-42");
    assert_eq!(h["originator"], "codex_cli_rs");
    drop(g);
    // the bash call ran and its output went back
    let id = fs::read_dir(d.path().join(".tau/sessions"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let ev = fs::read_to_string(id.join("events.jsonl")).unwrap();
    assert!(ev.contains("from-bash"), "{ev}");
    assert!(
        ev.contains("encrypted_content"),
        "reasoning is kept for replay: {ev}"
    );
}
