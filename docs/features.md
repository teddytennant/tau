# Everything else

The long list, so the README can stay short. None of this adds a tool for the model. It still has one, bash.

## Providers and models

`tau login`, or `/login` in the TUI, picks a provider and checks the key with a real request that lists models. You pick the default from that list. `/model` (ctrl+l) shows the live list with context sizes and prices; ctrl+f marks favorites and ctrl+p cycles through them. `tau models` prints the list. If a list can't be read, the fallbacks are `claude-opus-5-5`, `gpt-5.5`, `grok-4.7` and `anthropic/claude-opus-5.5`.

Keys live in `~/.tau/auth.json` (mode 600). Environment variables win over it: `ANTHROPIC_API_KEY` (or `ANTHROPIC_AUTH_TOKEN`), `OPENAI_API_KEY`, `XAI_API_KEY`, `OPENROUTER_API_KEY`, and `OPENAI_BASE_URL` for a custom server. `/logout` or `tau logout NAME` forgets a saved key.

Account sign-in works for ChatGPT (Plus, Pro, Team, through the same Codex endpoint and client id Codex uses) and xAI. It is OAuth with PKCE: tau listens on the loopback address the provider redirects to (1455 or 1457 for ChatGPT, 56121 for xAI), and `tau login chatgpt` or `tau login xai` also takes the redirect URL pasted on stdin, for a machine with no browser. `--force` replaces a saved session. Tokens refresh before they expire and once after a 401, under a lock on `~/.tau/auth.json.lock`, so several tau processes never spend the same refresh token. tau keeps its own tokens and never touches another program's token files. Anthropic is API keys only.

The status line shows the cost of each session. Prices come from OpenRouter's public model list: a snapshot ships in the binary, and the TUI refreshes it into `~/.tau/prices.tsv` when that copy is a week old. Account sessions show no cost, since the subscription pays for them.

## Thinking

shift+tab or `/thinking` cycles auto, low, medium, high, xhigh and max. On Anthropic that is `output_config.effort`; on chat completions it is `reasoning_effort` (xhigh and max become high). auto sends nothing and leaves it to the model. `--thinking` sets it for one run.

## The editor

enter sends. shift+enter or ctrl+j adds a line. ctrl+g opens `$VISUAL` or `$EDITOR`. up and down walk your history.

While tau works, enter steers: the message goes in before its next turn. alt+enter waits until it is completely done. esc interrupts and puts anything queued back in the editor. ctrl+c clears, interrupts, and quits on a second press.

`@` fuzzy-finds files (from git when the directory is a repository). On send, each `@path` that exists is inlined; a directory becomes a listing and an image is attached. Paste or drag an image path in, or ctrl+v to paste a clipboard image (needs wl-paste, xclip or pngpaste). Images work with every built-in provider if the model reads images.

`!cmd` runs a command and adds its output to the context without asking the model anything. `!!cmd` runs it and keeps it to yourself.

## Sessions

`/resume` (or `tau -r`) picks an earlier session, this directory's first. `/tree` (or esc twice) shows the whole session tree: pick one of your messages to rewrite it from just before it, or an answer to continue from there. `/fork` does the same into a new session, `/clone` copies the current branch, `/new` starts over. `/name` labels a session. `/session` shows ids, paths, tokens and cost.

`/export [file]` or `tau export` writes the current branch to one HTML file with no external requests. `/copy` puts the last answer on the clipboard.

## Compaction

When the context gets within 16k tokens of the model's window (or a quarter of it on a small window), tau asks the model for a summary and appends it as a node. The last ~20k tokens, cut at one of your messages, stay word for word after the summary. If the provider still says the prompt is too long, tau compacts and retries once. `/compact [focus]` does it by hand. `/settings` turns it off or changes how much is kept.

## Context files, skills and templates

`AGENTS.md` is read from `~/.tau`, then every directory from `/` down to where you started. In a directory with no `AGENTS.md`, `CLAUDE.md` is used.

A skill is `~/.tau/skills/NAME/SKILL.md` (or a loose `.md` with a `description:` in its front matter). Its name, description and path go in the system prompt, and the model cats the file when a task matches. `/skill:NAME args` sends it yourself. Add directories like `~/.claude/skills` under `skill_dirs` in `~/.tau/settings.json`.

A prompt template is `~/.tau/prompts/NAME.md`, used as `/NAME args`. It takes `$1`, `$@`, `$ARGUMENTS`, `${1:-default}`, `${@:2}` and `${@:2:1}`.

## Looks

`/settings` picks the theme. `dark` and `light` are built in; `~/.tau/themes/NAME.json` overrides any of `text accent user tool error dim code status_fg status_bg border selected_bg` with `#rrggbb`, a color name or a 256-color number, and `"base": "light"` starts from the light theme. The status line shows the model, thinking level, a context meter, cost and cache hit rate.

## Things tau leaves out on purpose

MCP, permission prompts, a sandbox, plan mode and extra model tools. If you want one of them, write it as a program: `~/.tau/bin` for the model, `~/.tau/commands` and `~/.tau/panels` for you.
