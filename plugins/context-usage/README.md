# herdr-context-usage

Collects AI agent context-window and reset usage for [Herdr](https://herdr.dev)
panes, and reports it so Herdr's top monitor strip can show per-pane usage
(`ctx p1 claude 63% ▰▰▰▰▰▰▱▱ 2h14m`).

This is a standalone crate that lives in the Herdr fork but is **not** part of
the `herdr` binary build. It is built and tested on its own:

```bash
cd plugins/context-usage
cargo test
cargo build --release
```

## What it does

Each collector runs inside a Herdr pane, reads the agent's local telemetry,
and writes a small per-pane record to
`~/.cache/herdr-context-usage/panes/<pane>.json`. When a recent enough Herdr is
running, it also calls `herdr pane report-usage` so the top strip can render
from server state instead of reading cache files.

## Compatibility

| CLI | Context % | Reset timer | Source | How |
| --- | --- | --- | --- | --- |
| Claude Code | yes (Pro/Max sessions) | yes when present | statusLine `rate_limits` | push (statusLine hook) |
| Codex | yes (official) | no (none in local state) | rollout JSONL `token_count` / `model_context_window` | pull (`poll`) |
| OpenCode | yes (estimated) | no | SQLite `session` + last message `tokens.input`, sized by a model registry | pull (`poll`) |
| Antigravity | harvester ready, unverified | when present | statusLine JSON (defensive parse) | push (statusLine hook) |
| Hermes | prefer-native (its own bar) | - | n/a by default | Herdr defers to Hermes |

"Context %" and "reset timer" are independent: a session can report one without
the other. Reset times are only ever shown when the provider supplies a
machine-readable value; they are never synthesized. `official` = a provider
percentage; `estimated` = token count sized against a model context-window
registry.

**Hermes** renders its own context bar, so Herdr defers to it by default
(`[ui.context_usage.native] hermes = "prefer-native"`). Set `hermes =
"prefer-herdr"` to have Herdr draw the segment too. The same per-agent key
exists for every agent (`prefer-herdr` / `prefer-native` / `both` / `hidden`).

**Push vs. pull.** Claude Code renders a statusLine on every turn, so its
collector is invoked there and reports immediately. Codex has no such hook, so
`poll` asks Herdr for the pane list, maps each Codex pane to its session by
working directory, reads the latest `token_count`, and reports. Run
`herdr-context-usage poll --watch` as a background daemon to keep Codex panes
current.

## Install

```bash
herdr-context-usage install         # wires the Claude Code statusLine collector
herdr-context-usage poll            # report pull-based agents (Codex) once
herdr-context-usage poll --watch    # keep Codex panes current (run as a daemon)
herdr-context-usage doctor          # diagnose the setup
herdr-context-usage show --all      # print cached usage
herdr-context-usage uninstall       # restore prior config
```

Install preserves any existing Claude `statusLine.command` by recording it and
chaining to it, and backs up `settings.json` before the first change.

## Privacy

Records hold only counts, model names, pane/tab ids, and timestamps. No prompt
or response text, file paths, or transcript contents are read or stored.
Collectors make no network calls. Cache files are written `0600` under a `0700`
directory where the platform supports it.

See the full plan at
`_arcwright-output/specs/herdr-context-window-usage-plan.md` (in the Arcwright
workshop) for the phased roadmap.
