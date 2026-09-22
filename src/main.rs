mod agent;
mod config;
mod depth;
mod edit;
mod exec;
mod export;
mod files;
mod job;
mod log;
mod login;
mod oauth;
mod prices;
mod prompt;
mod provider;
mod skills;
mod theme;
mod tui;
mod update;
mod widgets;

use agent::{Agent, Ev};
use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use config::Settings;
use log::Session;
use std::io::{IsTerminal, Read, Write};

#[derive(Parser)]
#[command(
    name = "tau",
    version,
    about = "pi, twice. A coding agent with one tool.",
    args_conflicts_with_subcommands = true
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
    /// Run headless: the answer goes to stdout, progress to stderr
    #[arg(short = 'p', long = "print")]
    print: bool,
    /// What to do. With -p and no prompt, read it from stdin
    prompt: Vec<String>,
    /// Continue the latest session started in this directory
    #[arg(short = 'c', long = "continue")]
    cont: bool,
    /// Pick an earlier session to continue
    #[arg(short = 'r')]
    pick: bool,
    /// Continue a session by id
    #[arg(long, value_name = "ID")]
    resume: Option<String>,
    /// Start as a copy of a session at a node: SESSION/NODE, or SESSION for its head
    #[arg(long, value_name = "NODE")]
    from: Option<String>,
    /// Model name. Also read from TAU_MODEL
    #[arg(long, env = "TAU_MODEL", hide_env = true)]
    model: Option<String>,
    /// anthropic, openai, xai, openrouter, chatgpt, xai-oauth or custom
    #[arg(long)]
    provider: Option<String>,
    /// auto, low, medium, high, xhigh or max
    #[arg(long)]
    thinking: Option<String>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Apply SEARCH/REPLACE blocks from stdin to FILE
    Edit { file: std::path::PathBuf },
    /// Commands that outlived their timeout
    Job {
        #[command(subcommand)]
        cmd: JobCmd,
    },
    /// Add a provider: `tau login` picks one, `tau login chatgpt` or
    /// `tau login xai` signs in with an account and works over SSH
    Login {
        provider: Option<String>,
        /// Start a new account session even if one is saved
        #[arg(long)]
        force: bool,
    },
    /// Forget a saved key
    Logout { provider: String },
    /// List the models the current provider offers
    Models,
    /// Write a session to a standalone HTML file
    Export {
        /// Session id; the latest in this directory by default
        id: Option<String>,
        #[arg(short, long)]
        out: Option<String>,
    },
    /// Replace this binary with the latest release
    Update {
        /// Only say whether there is a newer release
        #[arg(long)]
        check: bool,
    },
}

#[derive(Subcommand)]
enum JobCmd {
    List,
    /// Wait for a job and print its output
    Wait {
        id: u32,
        #[arg(default_value_t = 600)]
        timeout: u64,
    },
    Tail {
        id: u32,
        #[arg(short, default_value_t = 40)]
        n: usize,
    },
    Kill {
        id: u32,
    },
}

fn main() {
    let cli = Cli::parse();
    let code = match real_main(cli) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("tau: {e:#}");
            if exec::cancelled() { 130 } else { 1 }
        }
    };
    std::process::exit(code);
}

fn cwd_string() -> String {
    std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default()
}

fn real_main(cli: Cli) -> Result<i32> {
    match cli.cmd {
        Some(Cmd::Edit { file }) => {
            let mut input = String::new();
            std::io::stdin().read_to_string(&mut input)?;
            print!("{}", edit::run(&file, &input)?);
            return Ok(0);
        }
        Some(Cmd::Job { cmd }) => {
            return match cmd {
                JobCmd::List => job::list().map(|_| 0),
                JobCmd::Wait { id, timeout } => job::wait(id, timeout),
                JobCmd::Tail { id, n } => job::tail(id, n).map(|_| 0),
                JobCmd::Kill { id } => job::kill(id).map(|_| 0),
            };
        }
        Some(Cmd::Login {
            provider: Some(p),
            force,
        }) => {
            let a = match p.as_str() {
                "chatgpt" | "codex" => &oauth::CHATGPT,
                "xai" | "xai-oauth" | "grok" => &oauth::XAI,
                _ => bail!(
                    "`tau login {p}`: account sign-in is for chatgpt and xai. For an API key, run `tau login` and pick the provider"
                ),
            };
            install_ctrlc();
            oauth::login_cli(a, force)?;
            return Ok(0);
        }
        Some(Cmd::Login { provider: None, .. }) => {
            if !std::io::stdout().is_terminal() {
                bail!(
                    "tau login needs a terminal. Or set ANTHROPIC_API_KEY, OPENAI_API_KEY, XAI_API_KEY or OPENROUTER_API_KEY"
                );
            }
            match login::standalone(false)? {
                Some((spec, model)) => println!("saved. tau now uses {model} via {}", spec.name),
                None => println!("nothing changed"),
            }
            return Ok(0);
        }
        Some(Cmd::Logout { provider }) => {
            if login::logout(&provider)? {
                println!("forgot the {provider} key");
            } else {
                println!("no saved key for {provider}");
            }
            return Ok(0);
        }
        Some(Cmd::Models) => {
            let s = Settings::load();
            let Some(spec) = provider::resolve(cli.provider.as_deref(), &s, &config::load_auth())?
            else {
                bail!("no provider yet. Run `tau login`");
            };
            for m in provider::list_models(&spec)? {
                let price = prices::label(&m.id)
                    .map(|l| format!("  {l}"))
                    .unwrap_or_default();
                println!("{}{price}", m.id);
            }
            return Ok(0);
        }
        Some(Cmd::Export { id, out }) => {
            let id = match id {
                Some(i) => i,
                None => Session::latest(Some(&cwd_string()))
                    .ok_or_else(|| anyhow::anyhow!("no session here"))?,
            };
            let p = export::write(&Session::open(&id)?, out.as_deref())?;
            println!("{}", p.display());
            return Ok(0);
        }
        Some(Cmd::Update { check }) => {
            update::run(check)?;
            return Ok(0);
        }
        None => {}
    }

    let probe = depth::Depth::from_env();
    if let Err(e) = probe.check() {
        bail!("{e}");
    }

    let mut prompt = cli.prompt.join(" ");
    let stdin_tty = std::io::stdin().is_terminal();
    let interactive = !cli.print && std::io::stdout().is_terminal() && stdin_tty;
    if cli.print && prompt.is_empty() && !stdin_tty {
        std::io::stdin().read_to_string(&mut prompt)?;
    }

    let mut settings = Settings::load();
    if let Some(t) = &cli.thinking {
        if !config::THINKING.contains(&t.as_str()) {
            bail!("--thinking takes one of {}", config::THINKING.join(", "));
        }
        settings.thinking = t.clone();
    }
    let Some(spec) = provider::resolve(cli.provider.as_deref(), &settings, &config::load_auth())?
    else {
        if interactive {
            // first run: the UI starts with the welcome screen and login
            return tui::run(None, (!prompt.is_empty()).then_some(prompt), false);
        }
        bail!(
            "no API key. Run `tau login`, or set ANTHROPIC_API_KEY, OPENAI_API_KEY, XAI_API_KEY or OPENROUTER_API_KEY"
        );
    };
    let model = provider::pick_model(&spec, cli.model.as_deref(), &settings)?;
    // context windows the provider reported last time, for the meter and compaction
    provider::cached_models(&spec);
    let mut provider = provider::build(&spec, &model)?;
    provider.set_thinking(&settings.thinking);

    let session = if let Some(f) = &cli.from {
        agent::fork(f, probe.depth)?
    } else if let Some(id) = &cli.resume {
        Session::open(id)?
    } else if cli.cont {
        let id = Session::latest(Some(&cwd_string()))
            .ok_or_else(|| anyhow::anyhow!("no session to continue"))?;
        Session::open(&id)?
    } else {
        // a copy started without --from still records who started it
        let parent = std::env::var("TAU_SESSION").ok().filter(|s| !s.is_empty());
        agent::new_session(parent, probe.depth)?
    };
    let mut agent = Agent::new(session, provider);

    if cli.print {
        if prompt.trim().is_empty() {
            bail!("-p needs a prompt");
        }
        return print_mode(&mut agent, &prompt);
    }
    if !interactive {
        bail!("no terminal here; use `tau -p \"...\"` for a one-shot run");
    }
    tui::run(
        Some(agent),
        (!prompt.is_empty()).then_some(prompt),
        cli.pick,
    )
}

fn install_ctrlc() {
    let _ = ctrlc::set_handler(|| {
        if exec::cancelled() {
            std::process::exit(130);
        }
        exec::interrupt();
    });
}

fn first_line(s: &str, n: usize) -> String {
    let l = s.lines().next().unwrap_or("");
    let mut t: String = l.chars().take(n).collect();
    if l.chars().count() > n || s.lines().count() > 1 {
        t.push('…');
    }
    t
}

fn print_mode(agent: &mut Agent, prompt: &str) -> Result<i32> {
    install_ctrlc();
    let mut err = std::io::stderr();
    let depth = agent.depth.depth;
    let pad = "  ".repeat(depth as usize);
    let cwd = std::env::current_dir()?;
    let (text, images) = files::inline(prompt, &cwd);
    let mut stored = vec![];
    for p in images {
        stored.push(agent.session.store_image(&p)?);
    }
    let r = agent.run_with_images(&text, stored, &mut |ev| match ev {
        Ev::ToolStart { command, .. } => {
            let _ = writeln!(err, "{pad}$ {}", first_line(command, 160));
        }
        Ev::ToolEnd { code, lines, .. } => {
            let c = code.map(|c| format!("exit {c}")).unwrap_or("job".into());
            let _ = writeln!(err, "{pad}  {c}, {lines} lines");
        }
        Ev::Compacting => {
            let _ = writeln!(err, "{pad}compacting the history");
        }
        _ => {}
    });
    let u = agent.session.own_usage();
    match r {
        Ok(text) => {
            agent.session.set_status("ok");
            println!("{text}");
            eprintln!(
                "{pad}tau: {} · ${:.4} · cache {}%",
                agent.session.id,
                u.cost,
                u.cache_pct()
            );
            Ok(0)
        }
        Err(e) => {
            agent.session.set_status("error");
            eprintln!("{pad}tau: {} · ${:.4}", agent.session.id, u.cost);
            Err(e)
        }
    }
}
