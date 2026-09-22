//! `/export` and `tau export`: a session's current branch as one HTML file
//! with no external requests.

use crate::log::{Body, Session};
use std::collections::HashMap;

pub fn escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&#39;"),
            c => o.push(c),
        }
    }
    o
}

const CSS: &str = r#"
:root{--bg:#fdfdfb;--fg:#1d1d1b;--dim:#6b6b66;--user:#0b6bcb;--tool:#8a5a00;--box:#f1f0ec;--line:#e2e0da}
@media (prefers-color-scheme:dark){:root{--bg:#141413;--fg:#e8e6df;--dim:#9a988f;--user:#6cb6ff;--tool:#e3b341;--box:#1f1f1d;--line:#2e2e2b}}
*{box-sizing:border-box}
body{margin:0;background:var(--bg);color:var(--fg);font:15px/1.55 ui-sans-serif,system-ui,sans-serif}
main{max-width:860px;margin:0 auto;padding:32px 16px 64px}
h1{font-size:20px;margin:0 0 4px}
.meta{color:var(--dim);font-size:13px;margin-bottom:28px}
.msg{margin:18px 0}
.user{border-left:3px solid var(--user);padding-left:12px;white-space:pre-wrap;font-weight:600}
.asst{white-space:pre-wrap}
.note{color:var(--dim);font-style:italic;white-space:pre-wrap}
details{background:var(--box);border:1px solid var(--line);border-radius:6px;margin:8px 0}
summary{cursor:pointer;padding:6px 10px;font-family:ui-monospace,monospace;font-size:13px;color:var(--tool);overflow-wrap:anywhere}
pre{margin:0;padding:8px 10px;overflow-x:auto;font:12.5px/1.45 ui-monospace,monospace;white-space:pre-wrap;overflow-wrap:anywhere;border-top:1px solid var(--line)}
"#;

pub fn html(s: &Session) -> String {
    let chain = s.chain(s.head);
    let mut calls: HashMap<&str, &str> = HashMap::new();
    let mut body = String::new();
    let mut usage = crate::log::Usage::default();
    for e in &chain {
        match &e.body {
            Body::System { .. } => {}
            Body::User { text, images } => {
                let img = if images.is_empty() {
                    String::new()
                } else {
                    format!(" [{} image(s)]", images.len())
                };
                body.push_str(&format!(
                    "<div class=\"msg user\">{}{img}</div>\n",
                    escape(text)
                ));
            }
            Body::Assistant {
                text,
                calls: cs,
                usage: u,
                ..
            } => {
                usage.add(u);
                if !text.trim().is_empty() {
                    body.push_str(&format!(
                        "<div class=\"msg asst\">{}</div>\n",
                        escape(text.trim())
                    ));
                }
                for c in cs {
                    calls.insert(&c.id, &c.command);
                }
            }
            Body::Tool { call_id, output } => {
                let cmd = calls.get(call_id.as_str()).copied().unwrap_or("");
                body.push_str(&format!(
                    "<details><summary>$ {}</summary><pre>{}</pre></details>\n",
                    escape(cmd),
                    escape(output)
                ));
            }
            Body::Summary { text, .. } => body.push_str(&format!(
                "<div class=\"msg note\">Compacted here. Summary:\n{}</div>\n",
                escape(text)
            )),
        }
    }
    let title = if s.meta.task.is_empty() {
        s.id.clone()
    } else {
        s.meta.task.clone()
    };
    format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\n<title>{t}</title><style>{CSS}</style></head>\n<body><main><h1>{t}</h1><div class=\"meta\">tau session {id} · {cwd} · ${cost:.4}</div>\n{body}</main></body></html>\n",
        t = escape(&title),
        id = escape(&s.id),
        cwd = escape(&s.meta.cwd),
        cost = usage.cost,
    )
}

pub fn write(s: &Session, path: Option<&str>) -> anyhow::Result<std::path::PathBuf> {
    let p = std::path::PathBuf::from(
        path.map(String::from)
            .unwrap_or_else(|| format!("tau-{}.html", s.id)),
    );
    std::fs::write(&p, html(s))?;
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::{Call, Usage};

    #[test]
    fn export_escapes_and_pairs_calls_with_output() {
        crate::log::test_home();
        let mut s = Session::create(None, None, 0).unwrap();
        s.append(Body::System {
            text: "secret system prompt".into(),
        })
        .unwrap();
        s.append(Body::User {
            text: "fix <script>alert(1)</script>".into(),
            images: vec![],
        })
        .unwrap();
        s.append(Body::Assistant {
            text: "looking & fixing".into(),
            calls: vec![Call {
                id: "c1".into(),
                command: "cat a.rs | grep '<T>'".into(),
                timeout: None,
            }],
            raw: None,
            provider: "mock".into(),
            model: "mock".into(),
            usage: Usage {
                cost: 0.25,
                ..Default::default()
            },
        })
        .unwrap();
        s.append(Body::Tool {
            call_id: "c1".into(),
            output: "fn f<T>() {}".into(),
        })
        .unwrap();
        let h = html(&s);
        assert!(h.starts_with("<!doctype html>"));
        assert!(h.contains("fix &lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(!h.contains("<script>"));
        assert!(h.contains("looking &amp; fixing"));
        assert!(h.contains(
            "<summary>$ cat a.rs | grep &#39;&lt;T&gt;&#39;</summary><pre>fn f&lt;T&gt;() {}</pre>"
        ));
        assert!(!h.contains("secret system prompt"));
        assert!(h.contains("$0.2500"));
        assert!(!h.contains("http://") && !h.contains("https://"));
    }
}
