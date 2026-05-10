# winbash

A deliberately small zsh-like shell for Windows.

The goal is not POSIX or zsh compatibility. The goal is a fast interactive layer
that makes common Unix-style commands usable on Windows while delegating command
behavior to `uutils`.

## What Works

- Interactive prompt with history in `$HOME/.winbash_history`.
- Tab completion for commands, paths, `$VARS`, and `cd` directory arguments.
- Windows paths are treated as first-class input, so `C:\Users\name` is not
  parsed as escape sequences, but visible shell output prefers `/`.
- Simple quotes and escaped spaces:
  - `ls "folder with spaces"`
  - `ls folder\ with\ spaces`
- Simple command-position aliases:
  - `alias ll='ls -la'`
  - `unalias ll`
- Startup files:
  - `$HOME/.zshrc` imports simple `alias NAME=VALUE` lines.
  - `$HOME/.winbashrc` executes supported winbash commands.
  - `WINBASH_ZSHRC` and `WINBASH_RC` can override those paths.
- Prompt status:
  - Shows the git branch when inside a repository.
  - Adds `*` when the git worktree is dirty.
  - Adds `!STATUS` after a failed command.
  - Git dirty checks are cached and refreshed in the background.
- Ctrl+C attempts to terminate active foreground child processes.
- Builtins:
  - `cd`
  - `pwd`
  - `alias`
  - `unalias`
  - `export`
  - `unset`
  - `source` / `.`
  - `help`
  - `exit`
- Linux-style variable expansion:
  - `echo $HOME`
  - `echo ${HOME}`
  - `echo "$HOME"`
  - `echo '$HOME'` prints `$HOME`
  - `%HOME%` is literal text, not variable syntax
- Linux-style shell assignments:
  - `FOO=bar`
  - `export FOO`
  - `export FOO=bar`
  - `unset FOO`
  - `FOO=bar command`
- Glob expansion before launching external commands:
  - `ls *.rs`
  - `ls src/*.rs`
- Pipelines:
  - `ls src | rg main`
  - `cat file.txt | wc -l`
  - `pwd | cat`
- Basic redirection:
  - `cat input.txt > output.txt`
  - `echo more >> output.txt`
  - `rg needle < input.txt`
  - `rg needle missing.txt 2> errors.txt`
  - `pwd > cwd.txt`
- Control operators:
  - `cmd; next`
  - `cmd && next`
  - `cmd || fallback`
- Command substitution:
  - `echo "branch: $(git branch --show-current)"`
  - `FOO=$(echo bar)`

## uutils Discovery

`winbash` tries these in order for coreutils commands such as `ls`, `cat`, and
`mkdir`:

1. `WINBASH_UUTILS_DIR`, a directory containing separate tools such as
   `ls.exe`.
2. `WINBASH_COREUTILS`, a path to a multi-call `coreutils.exe` binary.
3. A matching command already on `PATH`, for example `ls.exe`.
4. `coreutils.exe` on `PATH`, called as `coreutils ls ...`.

## Run

```powershell
cargo run
```

Command mode is useful for smoke tests:

```powershell
cargo run -- -c "pwd"
cargo run -- -c "help"
cargo run -- -c "ls src"
cargo run -- -c "ls src | rg main"
cargo run -- -c "echo hello > target/winbash-smoke.txt"
cargo run -- -c "export FOO=bar"
cargo run -- -c "FOO=bar env | rg '^FOO='"
cargo run -- -c "echo before $(echo inner) after"
cargo run -- -c "cargo build && cargo test"
```

## Test

Fast parser, state, and property tests:

```powershell
cargo test --bin winbash
```

End-to-end CLI tests that launch the compiled shell:

```powershell
cargo test --test cli
```

Everything:

```powershell
cargo test
```

The CLI tests set `WINBASH_NO_RC=1` so local startup files do not affect
assertions.

## Current Limits

- Shell vars are minimal: no arrays, arithmetic, parameter modifiers, or `set`.
- Unquoted variable values are not word-split yet.
- Alias parsing intentionally supports only simple `alias NAME=VALUE` forms.
- Pipelines with builtins are materialized when needed, not streamed end-to-end.
- Git prompt dirty status is cached and refreshed in the background.
