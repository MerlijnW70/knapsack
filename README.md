<p align="center">
  <img src="knapsack_logo.jpg" alt="Knapsack logo" width="240">
</p>

# Knapsack

**Knapsack helps AI coders waste less context by skipping repeated files, logs, and test output.**

[![Release](https://img.shields.io/github/v/release/MerlijnW70/knapsack?label=release&color=2ea44f)](https://github.com/MerlijnW70/knapsack/releases)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue)](#license)
![Platforms](https://img.shields.io/badge/platforms-Windows%20·%20macOS%20·%20Linux-informational)

Show changes. Skip repeats. Save tokens. Stop paying your AI coder to read the same context twice.

**[Install](#install) · [What it does](#what-knapsack-does) · [Check it's working](#check-that-its-working) · [Troubleshooting](#troubleshooting) · [Uninstall](#uninstall)**

---

## What Knapsack does

AI coding agents read the same files and command output many times during edit-test loops. Every time, the full content gets dumped back into the context window — burning tokens on text the agent has already seen.

Knapsack notices repeated context, shows what changed, and keeps the original available for recall.

**Without Knapsack:**
- run tests
- AI reads a long test log
- make a small edit
- run tests again
- AI reads almost the same log again

**With Knapsack:**
- the first run shows the useful output
- later runs show mostly what changed
- repeated parts become short recall notes

If the agent needs the original back, it can fetch it any time — byte for byte, the same characters you'd see without Knapsack.

---

## What Knapsack helps with

- repeated test output
- build logs
- search results
- large file reads
- repeated file reads
- long AI coding sessions
- keeping context cleaner
- seeing token savings

Knapsack reduces *repeated* context — it does not make every interaction cheaper. Savings depend on your workflow. Repeated test runs, logs, search output, and large or repeated file reads benefit most. When a shorter view wouldn't actually help, Knapsack passes the content through unchanged.

---

## Install

One command. Restart Claude Code. Done.

**Windows (PowerShell)**
```powershell
irm https://raw.githubusercontent.com/MerlijnW70/knapsack/main/install.ps1 | iex
```

**macOS / Linux**
```sh
curl -fsSL https://raw.githubusercontent.com/MerlijnW70/knapsack/main/install.sh | sh
```

The installer downloads a small binary, verifies its checksum, backs up your Claude Code config, wires Knapsack in, and runs a self-check.

After installing, **restart Claude Code**. That's it.

<details>
<summary>Prefer to build it yourself?</summary>

```sh
git clone https://github.com/MerlijnW70/knapsack
cd knapsack
cargo build --release
./target/release/knapsack install
```
</details>

---

## Check that it's working

```sh
knapsack doctor
```
Checks installation health: the binary, the Claude Code hook, recall storage, and a quick round-trip test.

```sh
knapsack status
```
Shows whether Knapsack is active and how much it has saved so far.

You can also type `/knapsack` inside Claude Code for the same summary.

---

## Normal usage

After install, you don't need to change your workflow. Run Claude Code normally — Knapsack works in the background.

Install once. Use Claude Code normally. See savings.

---

## Seeing savings

Savings appear after Claude Code reads files or runs commands. Right after install, savings show as zero — nothing has happened yet.

After a few file reads or repeated command runs, `knapsack status` will start showing tokens saved.

```sh
knapsack metrics
```
A more detailed savings scoreboard.

---

## Recall

If Knapsack shows a shorter view, the original is still available. Claude can ask for it back any time. You usually don't need to think about this.

Power users can recall content directly:

```sh
knapsack expand <handle>                  # full original bytes
knapsack expand <handle> --lines 10-40    # a slice
knapsack inspect <handle>                 # metadata + preview
```

---

## Troubleshooting

```sh
knapsack doctor
```
Checks that the hook, recall tools, and storage are healthy.

```sh
knapsack why-last
```
Shows the last few decisions Knapsack made on file reads — useful if you expected a specific file to be shortened but it wasn't.

```sh
knapsack install --repair
```
Re-points Claude Code at the installed Knapsack binary. Use this if you reinstalled, moved the binary, or `doctor` reports drift.

---

## Uninstall

```sh
knapsack uninstall
```
Removes Knapsack from Claude Code's config. Your local Knapsack data is kept in case you reinstall.

```sh
knapsack uninstall --purge
```
Also removes the local Knapsack data and cache.

---

## Who Knapsack is for

Knapsack is for developers using AI coding agents — especially when those agents run tests, searches, and file reads repeatedly during edit-test loops.

## Who it's not for

Knapsack is less useful if:

- you rarely repeat commands or file reads
- your AI sessions are very short
- you don't use Claude Code-compatible hooks/MCP

Knapsack is a **local developer tool**. It runs on your machine, alongside Claude Code. It is not a shared or team server.

---

## FAQ

**Will it lose any of my output?**
No. When Knapsack shows a shorter view, the original is still recoverable — same bytes, same characters.

**Will it mess up my Claude Code config?**
The installer backs up `settings.json` and `~/.claude.json` before changing anything. `knapsack uninstall` reverses the changes cleanly.

**Do I need to do anything after installing?**
Restart Claude Code so it picks up the new hook.

**What does it need to run?**
Nothing extra — Knapsack is a single small binary with no runtime dependencies.

---

## Technical details

Knapsack is a small Rust binary with zero runtime dependencies. It plugs into Claude Code through its hook and recall-tool surfaces. Local data lives under `~/.knapsack/`.

A deeper architecture write-up belongs in `ARCHITECTURE.md` — TODO.

---

## License

MIT — free to use, modify, and share.
