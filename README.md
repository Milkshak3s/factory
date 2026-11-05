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
   pre-selected, and an *Other* field for typing an alternative. The grill is on by default, and you
   can turn it off per project with the *Grill first* checkbox beside *Run pipeline*. A skipped run
   sends your prompt straight to the build agent, uses the prompt's first line as the commit title,
   and uses the bump you chose in the radio buttons.
2. **Branch:** a local `factory/<run-id>` branch, in its own git worktree, so several runs can go at
   once and none of them edit your checkout.
3. **Build:** a fresh agent gets the agreed brief and the Q&A behind it, or just your prompt if the
   grill was skipped. If tests fail, their output
   goes back to the same session, up to *Max attempts* rounds.
4. **Test:** `make test` is the gate. Nothing is committed unless it passes.
5. **Merge:** rebases onto the default branch, re-tests if it moved, and pushes. History stays linear.
6. **Release:** bumps the version, writes a bullet summary of everything since the last tag, and
   gets a draft GitHub Release (see below). The grill picks patch, minor or major, or you do with the
   radio buttons if the grill was skipped.
7. **Publish:** promotes the draft to published, once the release process has finished.

While a run is going, a line under the stages says what it's waiting on right now, such as
``Waiting for `make test` `` or `Waiting for the build agent (attempt 2 of 3)`, with a timer that
restarts for each new wait. During the release workflow it also links to the run on GitHub. When
a run needs your answers, its dot in the run list and its project's dot turn amber, even if a newer
run is going on the same project. Every other wait, CI included, stays blue. Hover over a dot to see
the wait.

*Cut release…* runs Test → Release → Publish on its own, for when there's no code to change.

Successful runs clean up their worktree. Failed ones keep it for inspection until you discard it.
A failure after Merge leaves the merged work on the default branch.

## Running Makefile targets

The row under *Run pipeline* runs a target from the project's Makefile in your checkout, and each
command appears in the run list with its own status, live log and *Stop* button. It involves no
agent, worktree, branch, commit or push, and it needs only `make`, plus `git` for *Pull first*. It
works when `claude`, `gh` or `origin` are missing. These runs don't change the project's dot, which
follows pipeline runs only.

- **Finding targets:** Factory reads `GNUmakefile`, `makefile` or `Makefile` from your working tree,
  in that order, on each refresh, without running `make`. Only rules written in that file are listed,
  so targets from `include` and generated rules are missing. The list leaves out names containing `%`
  or `$`, and names starting with `.` or `-`. The row is hidden if there's no Makefile.
- **Where it runs:** in your real checkout, on whatever is checked out, including uncommitted changes.
  A WSL project runs it inside the distro. It uses the checkout's normal build directory rather than
  the Cargo cache that pipeline runs share.
- **Pull first:** this checkbox is saved per project and off by default. It runs `git pull --ff-only`
  on the checked-out branch before the target. If that fails for any reason, such as a diverged
  branch, no upstream, a detached `HEAD`, a conflict with local edits or a network failure, the run
  fails and `make` doesn't run. It never merges, rebases or stashes.
- **No confirmation:** *Run* starts immediately, whatever the target is called. Only a run of the same
  target on that project disables it, so different targets can run at once and can collide if they
  share files. Commands get no input, so a recipe that prompts for some won't receive any.

Recipes can change your working files, or anything else your user can reach. The protections in
[What keeps agents out of the pipeline's steps](#what-keeps-agents-out-of-the-pipelines-steps) cover
agents, and don't apply to these commands.

## How a release is produced

- **With a release workflow** (`.github/workflows/release.yml` triggered by the `release` branch):
  pushes the version bump to the default branch and `release`, waits for the workflow to pass,
  checks that it left a draft, and sets the notes.
- **Without one:** pushes the bump, creates the draft itself, and runs the optional *Release
  command* with the tag in `FACTORY_TAG`.

The version comes from `Cargo.toml` or `package.json`, otherwise from the latest tag.

Factory's own releases take the first route. Its `.github/workflows/release.yml` runs the tests and
builds the packages on each OS, then attaches them all to one draft:

- `factory-<version>-x64.msi`, built with WiX on `windows-latest`
- `factory-<version>-arm64.dmg`, on `macos-latest`
- `factory-<version>-x64.AppImage` and `factory-<version>-x64.deb`, on `ubuntu-22.04`

A failure on any OS fails the whole run, so nothing is released until all of them pass. The packaging
scripts are in `packaging/`.

The app icon is drawn in code in `src/icon.rs`. The window renders it at startup. `build.rs` writes
it beside the binary for the packages, as `factory.ico` on Windows (and embeds it in the exe),
`factory.icns` on macOS and `factory.png` on Linux.

## What keeps agents out of the pipeline's steps

- `git commit`, `push`, `tag` and similar are on the build agent's tool denylist.
- `git push` is pointed at a dead URL in every agent's environment.
- After the build agent stops, the run fails if `HEAD` moved, the branch changed, or the Makefile
  changed. Since `make test` is the gate, it can't be rewritten by the run it's judging.
- No agent can reach your claude.ai connectors, such as Gmail and Drive (`--strict-mcp-config`).

This is not a sandbox. A determined agent could still undo the push block, or edit test files so
they pass, and a passing run goes all the way to a public release with no further review. Skipping
the grill also removes the only human input before publish, so a vague prompt can be built, merged
and published as it stands.

The default `acceptEdits` mode still lets the build agent run some shell commands. In testing,
`whoami`, `git status` and `python --version` ran, while `cargo --version` and `curl --version` were
refused. Add what it needs under *Allowed tools*, such as `Bash(cargo check:*)`, so it can compile
its own work. `bypassPermissions` removes the limit entirely, so use it with care.

## Install

Download your platform's package from
[Releases](https://github.com/Milkshak3s/factory/releases).

**Windows:** run `factory-<version>-x64.msi`. It installs for your user only, needs no admin rights,
and adds a Factory shortcut to the Start Menu. The program goes in
`%LOCALAPPDATA%\Programs\Factory`. Uninstalling leaves your configuration and run history in place.
The MSI isn't code-signed, so a browser download may show a SmartScreen warning.

**macOS (Apple silicon):** open `factory-<version>-arm64.dmg` and drag Factory to Applications. The
app is ad-hoc signed and not notarized, so Gatekeeper blocks a browser download. Either allow it in
System Settings > Privacy & Security, or run:

```bash
xattr -dr com.apple.quarantine /Applications/Factory.app
```

**Linux (x64), AppImage:** make `factory-<version>-x64.AppImage` executable with `chmod +x` and run
it. Some systems also need FUSE, usually the `libfuse2` package.

**Linux (x64), Debian or Ubuntu:** run `sudo apt install ./factory-<version>-x64.deb`.

Launched from Finder or a desktop launcher, Factory asks your login shell for its `PATH`, and also
looks in `/opt/homebrew/bin`, `/usr/local/bin`, `~/.local/bin`, `~/.cargo/bin` and `~/.claude/local`,
so it finds the same `claude`, `gh`, `git` and `make` your terminal does. Launched from a terminal,
it uses that terminal's `PATH` plus those directories.

## WSL projects

The Windows app can also work on repos on a WSL distro's Linux filesystem. Every tool for those
projects, including setup and release commands, runs inside the distro through `wsl.exe`.

**Adding one:** use *Add…* and pick the repo, or a folder of repos, under `\\wsl.localhost\<distro>\`
(*Linux* in Explorer's sidebar). Paths under `\\wsl$\` work too. The project shows a `WSL: <distro>`
label. Repos on a Windows drive (`/mnt/c/...`) aren't supported, even from inside a distro.

**What each distro needs:**

- `git`, `make`, `claude` and `gh` on your login shell's `PATH`, with `claude` and `gh` logged in
  there. Their Windows copies aren't used, and `claude_path` doesn't apply. A Windows copy reached
  through `/mnt/...` doesn't count.
- Git auth that works without prompting, such as `gh auth setup-git` or an SSH key with no passphrase
  prompt. Factory doesn't forward an SSH agent or Windows credentials.
- `setsid --wait` from util-linux, so that *Stop* can kill what a run started in the distro. Mainstream
  distros have it. On a BusyBox-based distro such as Alpine, you may need to install `util-linux`.

Factory checks for the tools, and for the `gh` login, on each refresh and again at the start of each
run. Anything missing shows as an amber message naming the distro, and *Run pipeline* stays disabled
until it's fixed. A logged-out `claude` isn't checked in advance, so it fails the run at its first
agent. Factory reads the `PATH` from your interactive login shell, once per distro, and reads it again
at the start of each run and on *Refresh*, so a newly installed tool is picked up without a restart.

**Where things live:** worktrees are in `~/.local/share/factory/worktrees/<project>/<run>` and the
Cargo cache is in `~/.local/share/factory/target/<project>`, under the distro's default user. Run
history and logs stay in the Windows data folder. *Open worktree* opens the run's worktree in
Explorer.

**Stopped distros:** the 30-second refresh, startup and a finished run never start a stopped distro,
so Factory being open doesn't keep its VM's memory in use. The project shows `<distro> is stopped`
with its last known status. Clicking *Refresh* or *Add…*, or starting a run, wakes it.

## Updates

The installed app checks this repo's latest published release at startup and every 30 minutes, or
when you click *Check for updates* at the bottom of the Projects panel. When a newer version has a
package for your install, a bar appears at the top of the window. *Update now* downloads it, warns
you if runs are in progress because they'll be stopped, then closes the app, installs the update and
relaunches.

- Checks and downloads go through `gh`, so it has to be installed and logged in. A failed background
  check shows nothing and is retried at the next interval.
- If the install fails, the current version stays and is relaunched, and the bar comes back at the
  next check.
- A `.deb` install asks for your password through polkit. Cancelling counts as a failed install.
- Source builds such as `cargo run` don't check or update. Only an installed copy does: on Windows
  one in the install folder, on macOS a `Factory.app` in `/Applications` or `~/Applications`, and on
  Linux an AppImage or `/usr/bin/factory`.
- Versions from before the first MSI need one manual install.

## Setup

To run from source, you need GNU Make, the GitHub CLI and the Claude CLI, the last two logged in.
Factory finds `claude` on `PATH`, then on Windows in the desktop app's install, and that binary needs
its own login. You can also set `claude_path` in the configuration file.

On Windows:

```bash
winget install ezwinports.make
```

```bash
winget install GitHub.cli
```

On macOS, the system `make` is GNU Make, so it's sufficient, and Homebrew's works too:

```bash
brew install make gh
```

On Debian or Ubuntu, where a from-source build also needs the windowing libraries:

```bash
sudo apt install make gh libxkbcommon-dev libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev libx11-dev libxcursor-dev libxi-dev libxrandr-dev libgl1-mesa-dev libegl1-mesa-dev libwayland-dev libfontconfig1-dev
```

Then, on every OS:

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
Makefile with a `test` target. If all your projects are in WSL, the Windows `claude` and `gh` aren't
needed for them, though the updater still uses `gh`. See [WSL projects](#wsl-projects).

If the Makefile is missing, *Create Makefile* has an agent write one from the project's tests and CI.
That agent can write files but can't run commands. The factory runs `make test`, sends failures back
to it, and pushes to the default branch only once the Makefile passes. If the agent reports that the
project has no tests, `make test` must fail instead, because a gate that passes with no tests would
wave every change through. Runs on that project can't land until it has tests.

Configuration is in `config.toml`, and run history, worktrees and a per-project Cargo cache are in
a data folder:

| OS | Configuration | Data |
| --- | --- | --- |
| Windows | `%APPDATA%\factory\config.toml` | `%LOCALAPPDATA%\factory` |
| macOS | `~/Library/Application Support/factory/config.toml` | `~/Library/Application Support/factory` |
| Linux | `~/.config/factory/config.toml` | `~/.local/share/factory` |

A WSL project's worktrees and Cargo cache are in `~/.local/share/factory` inside its distro. Its run
history stays in the Windows data folder.

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

The WSL tests run against a real distro only if you name one, on Windows, and do nothing otherwise.
`make test` doesn't cover the real distro path, so run these after changing how commands reach WSL.
They create and remove temp directories in that distro:

```bash
FACTORY_TEST_WSL_DISTRO=Ubuntu cargo test wsl::tests::real -- --ignored
```
