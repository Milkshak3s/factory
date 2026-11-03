# Factory

A desktop list of the projects you need to be able to change quickly, and a pipeline that runs
Claude Code on them. The agent writes code. Everything around it is run by the factory, so those
steps happen regardless of what the agent does or says.

```
Build:    Prepare → Setup → Agent ⇄ Test → Commit → Push
Release:  Prepare → Setup → Test → Tag → Push → Release command
```

- **Prepare** fetches and makes a fresh git worktree from `origin/<default branch>`. Your checkout
  is never touched, and several runs can go at once.
- **Agent** runs `claude -p` in the worktree. If tests fail, their output goes back to the same
  session, up to *Max attempts* rounds.
- **Test** is the gate: nothing is committed or tagged unless it passes. A project with no test
  command can't start a run.
- **Commit** and **Push** run only after the gate. Push goes to a new `factory/<run-id>` branch, or
  to the default branch if the project is set to. Pushing to the default branch rebases first and
  re-tests if it moved.
- **Release** is started by hand from *Cut release…*. It tests `origin/<default branch>`, pushes an
  annotated tag, then runs the optional release command with the tag in `FACTORY_TAG`.

Successful runs clean up their worktree. Failed ones keep it for inspection until you discard it.

## What keeps the agent out of the pipeline's steps

- The project's configuration lives outside its repo, so the agent can't edit the gate.
- `git commit`, `push`, `tag` and similar are on the agent's tool denylist.
- `git push` is pointed at a dead URL in the agent's environment.
- After the agent stops, the run fails if `HEAD` moved or the branch changed.

This is not a sandbox. A determined agent could still undo the push block, or edit test files so
they pass.

The default `acceptEdits` mode still lets the agent run some shell commands. In testing, `whoami`,
`git status` and `python --version` ran, while `cargo --version` and `curl --version` were refused.
Add what it needs under *Allowed tools*, such as `Bash(cargo check:*)`, so it can compile its own
work. `bypassPermissions` removes the limit entirely, so use it with care.

## Setup

Factory finds `claude` on `PATH`, then in the desktop app's install. That binary needs its own
login:

```bash
claude auth login
```

Then:

```bash
cargo run --release
```

Use *Add…* to pick a repo, or a folder of repos. Each project needs an `origin` remote and a test
command (detected for Rust, Node, Go and Python).

Configuration is in `%APPDATA%\factory\config.toml`. Run history, worktrees and a per-project Cargo
cache are in `%LOCALAPPDATA%\factory`.

## Tests

```bash
cargo test
```

These use real git against a local bare remote, with a scripted agent. To also run the pipeline
with real Claude against a throwaway repo (this uses your account):

```bash
cargo test -- --ignored
```
