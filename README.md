# Factory

A desktop list of the projects you need to be able to change quickly, and a pipeline that takes a
prompt all the way to a published release. Agents write the brief and the code. Everything around
them is run by the factory, so those steps happen regardless of what an agent does or says.

```
Prepare → Grill → Branch → Build ⇄ Test → Commit → Merge → Release → Publish
```

0. **Prepare:** fetches `origin`, starts the run from the latest default branch, and fast-forwards
   your local copy of it too. If local commits or edits stop that fast-forward, it warns and carries
   on, because the run works from `origin` either way. Your unpushed work stays out of the run.
1. **Grill:** an agent reads the code and questions you, in rounds, until the request has narrowed
   to one specific solution. It can read files but not change anything. *Enough, build it* tells it
   to settle whatever is left. Questions come with clickable options, the agent's recommendation
   pre-selected, and an *Other* field for typing an alternative.
2. **Branch:** a local `factory/<run-id>` branch, in its own git worktree, so several runs can go at
   once and none of them edit your checkout.
3. **Build:** a fresh agent gets the agreed brief and the Q&A behind it. If tests fail, their output
   goes back to the same session, up to *Max attempts* rounds.
4. **Test:** `make test` is the gate. Nothing is committed unless it passes.
5. **Merge:** rebases onto the default branch, re-tests if it moved, and pushes. History stays linear.
6. **Release:** bumps the version, writes a bullet summary of everything since the last tag, and
   gets a draft GitHub Release (see below). The grill picks patch, minor or major.
7. **Publish:** promotes the draft to published, once the release process has finished.

*Cut release…* runs Test → Release → Publish on its own, for when there's no code to change.

Successful runs clean up their worktree. Failed ones keep it for inspection until you discard it.
A failure after Merge leaves the merged work on the default branch.

## How a release is produced

- **With a release workflow** (`.github/workflows/release.yml` triggered by the `release` branch):
  pushes the version bump to the default branch and `release`, waits for the workflow to pass,
  checks that it left a draft, and sets the notes.
- **Without one:** pushes the bump, creates the draft itself, and runs the optional *Release
  command* with the tag in `FACTORY_TAG`.

The version comes from `Cargo.toml` or `package.json`, otherwise from the latest tag.

Factory's own releases take the first route: its `.github/workflows/release.yml` builds
`factory-<version>-x64.msi` with WiX on `windows-latest` and attaches it to the draft.

## What keeps agents out of the pipeline's steps

- `git commit`, `push`, `tag` and similar are on the build agent's tool denylist.
- `git push` is pointed at a dead URL in every agent's environment.
- After the build agent stops, the run fails if `HEAD` moved, the branch changed, or the Makefile
  changed. Since `make test` is the gate, it can't be rewritten by the run it's judging.
- No agent can reach your claude.ai connectors, such as Gmail and Drive (`--strict-mcp-config`).

This is not a sandbox. A determined agent could still undo the push block, or edit test files so
they pass, and a passing run goes all the way to a public release with no further review.

The default `acceptEdits` mode still lets the build agent run some shell commands. In testing,
`whoami`, `git status` and `python --version` ran, while `cargo --version` and `curl --version` were
refused. Add what it needs under *Allowed tools*, such as `Bash(cargo check:*)`, so it can compile
its own work. `bypassPermissions` removes the limit entirely, so use it with care.

## Install

Download `factory-<version>-x64.msi` from
[Releases](https://github.com/Milkshak3s/factory/releases) and run it. It installs for your user
only, needs no admin rights, and adds a Factory shortcut to the Start Menu. The program goes in
`%LOCALAPPDATA%\Programs\Factory`. Uninstalling leaves your configuration and run history in place.

The MSI isn't code-signed, so a browser download may show a SmartScreen warning.

## Updates

The installed app checks this repo's latest published release at startup and every 30 minutes, or
when you click *Check for updates* at the bottom of the Projects panel. When a newer version has an
MSI, a bar appears at the top of the window. *Update now* downloads the MSI, warns you if runs are
in progress because they'll be stopped, then closes the app, installs the update and relaunches.

- Checks and downloads go through `gh`, so it has to be installed and logged in. A failed background
  check shows nothing and is retried at the next interval.
- If the install fails, the current version stays and is relaunched, and the bar comes back at the
  next check.
- Source builds such as `cargo run` don't check or update. Only a copy running from the install
  folder does.
- Versions from before the first MSI need one manual install.

## Setup

To run from source, you need GNU Make, the GitHub CLI and the Claude CLI, the last two logged in. Factory finds `claude`
on `PATH`, then in the desktop app's install, and that binary needs its own login.

```bash
winget install ezwinports.make
```

```bash
winget install GitHub.cli
```

```bash
gh auth login
```

```bash
claude auth login
```

Then:

```bash
cargo run --release
```

Use *Add…* to pick a repo, or a folder of repos. Each project needs an `origin` remote and a
Makefile with a `test` target.

If the Makefile is missing, *Create Makefile* has an agent write one from the project's tests and CI.
That agent can write files but can't run commands. The factory runs `make test`, sends failures back
to it, and pushes to the default branch only once the Makefile passes. If the agent reports that the
project has no tests, `make test` must fail instead, because a gate that passes with no tests would
wave every change through. Runs on that project can't land until it has tests.

Configuration is in `%APPDATA%\factory\config.toml`. Run history, worktrees and a per-project Cargo
cache are in `%LOCALAPPDATA%\factory`.

## Tests

```bash
cargo test
```

These use real git and `make` against a local bare remote, with a scripted agent and a fake GitHub.
To also run the pipeline with real Claude against a throwaway repo (this uses your account, and
GitHub is still faked):

```bash
cargo test -- --ignored
```
