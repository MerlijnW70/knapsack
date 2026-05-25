<p align="center">
  <img src="knapsack_logo.jpg" alt="Knapsack logo" width="240">
</p>

# 🎒 Knapsack

**Stop paying for output Claude Code has already seen.**
Knapsack shrinks the noisy command output and file reads that flood your context window — so your tokens go to thinking, not re-reading. Nothing is lost: Claude can pull back the exact original any time.

[![Release](https://img.shields.io/github/v/release/MerlijnW70/knapsack?label=release&color=2ea44f)](https://github.com/MerlijnW70/knapsack/releases)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue)](#license)
![Platforms](https://img.shields.io/badge/platforms-Windows%20·%20macOS%20·%20Linux-informational)
![Dependencies](https://img.shields.io/badge/runtime%20deps-0-brightgreen)

**[Install](#install) · [Why](#why-youll-want-it) · [How it works](#how-it-works) · [Commands](#commands) · [FAQ](#faq)**

---

## Install

One line. Restart Claude Code. Done.

**Windows (PowerShell)**
```powershell
irm https://raw.githubusercontent.com/MerlijnW70/knapsack/main/install.ps1 | iex
```

**macOS / Linux**
```sh
curl -fsSL https://raw.githubusercontent.com/MerlijnW70/knapsack/main/install.sh | sh
```

The installer downloads a tiny binary, verifies its checksum, **backs up** your Claude Code config, wires itself in, and runs a self-check. Then just **restart Claude Code** — that's it.

<details>
<summary>Prefer to build it yourself?</summary>

```sh
git clone https://github.com/MerlijnW70/knapsack
cd knapsack
cargo build --release
./target/release/knapsack install --apply
```
</details>

---

## Why you'll want it

Agents re-read the same files and re-run the same tests over and over. Every time, the **full output** gets dumped back into the context window — burning tokens (and money) on text Claude has already seen.

Knapsack quietly compresses that output as it comes in. When Claude actually needs a detail, it asks for the exact bytes back.

- 💸 **Up to ~90% fewer tokens** on the repeated reads and edit→test loops agents do most.
- 🔒 **Nothing is lost.** Recall is byte-exact — character for character, every time.
- ⚡ **Invisible.** It runs as a Claude Code hook. Install once, then forget it's there.
- 🪶 **Tiny & safe.** One small binary, zero runtime dependencies, and it backs up your config before touching anything.

---

## How it works

1. A command runs — tests, a build, a big file read — and its output would normally flood the context.
2. Knapsack saves the exact output and shows Claude a **compact summary** plus a small handle.
3. If Claude needs the details, it **expands the handle** and gets the exact lines back (whole, a line range, or a search match).
4. Re-running the same thing? Claude already has it — so you only pay for **what actually changed**.

> The first time something is seen, it's sent in full. Every repeat after that is nearly free.

---

## What you get

- **Automatic compression** of noisy command output, the moment it runs.
- **On-demand recall** — Claude fetches the full output, a line range, or a search match, byte-exact.
- **A live savings scoreboard** — run `knapsack metrics` any time to see tokens saved.
- **Cross-platform** — Windows, macOS (Intel & Apple Silicon), and Linux.

---

## Commands

| Command | What it does |
| --- | --- |
| `knapsack doctor` | Health check — confirms the hook and MCP server point at the same installed binary |
| `knapsack metrics` | Shows how many tokens you've saved so far |
| `knapsack uninstall` | Cleanly removes it (add `--purge` to also delete its cache) |

> `knapsack install --apply` (run for you by the installer) is what wires it into Claude Code.
> If `doctor` ever reports drift, `knapsack install --repair` re-points the hook and MCP server back at the installed binary.

---

## FAQ

**Will it lose any of my output?**
No. Recall is byte-exact — Claude can always get the original back, character for character.

**Will it mess up my Claude Code config?**
It backs up your `settings.json` and `~/.claude.json` before making any change, and `knapsack uninstall` reverses everything cleanly.

**Do I need to do anything after installing?**
Just restart Claude Code so it picks up the new hook.

**What does it need to run?**
Nothing extra — it's a single self-contained binary with zero runtime dependencies.

**How do I remove it?**
```sh
knapsack uninstall          # remove it, keep the cache
knapsack uninstall --purge  # remove it and delete the cache
```

---

## License

MIT — free to use, modify, and share.
