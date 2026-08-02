# bsh

A small, fast shell written in Rust. It does the usual shell things,
pipelines, redirects, history, tab completion, and also adds other features.

`bsh` is a single Rust binary with a couple of small crate dependencies. It
talks to the terminal directly, so it is snappy on lean installs and machines
where a shell should just be a shell.

## What it does

Command and path completion(tab for full accept + right key for one character accept), history navigation with the up/down arrow keys, inline
autosuggestions, auto-closing quotes and brackets, piping, chaining
(`&&`, `||`), background jobs (`&`), and full redirect support (`>`, `>>`,
`<`, `<>`, `>&`, `<&`, `&>`, and numbered fd redirects like `2>file`).

It ships with the usual builtins (`cd`, `pwd`, `echo`, `export`, `alias`,
`source`, `rm`, `type`, ...) plus `shit`: a thefuck-style command corrector
that re-runs your last command and suggests a fix. And `bshctl`, a control
suite for tuning the shell to taste.

## Build and run

```
# Clone the repo and cd into it
git clone https://github.com/bulletic/bsh.git && cd bsh

# Compile it and add to your PATH
cargo install --path .
```

## Use it your way

```sh
# Launch the shell
bsh

# Run a command and exit (scripting)
bsh -c "echo hi | tr a-z A-Z"

# Start a fresh shell that ignores history and config
bsh --fresh
```

Configurable features:

```sh
# Toggle a feature on or off
bshctl enable  history
bshctl disable auto-suggestion

# Check the state of a feature
bshctl status shit
```

Run `bshctl --help` for the complete command list.

## Fixing a typo with shit

Typed something and it failed? Run `shit` and it will re-run the last command,
spot the problem, and offer a fix:

```
bsh: shit: cd some-dir/that/doesnt/exist -> mkdir -p some-dir/that/doesnt/exist ? [y/n]
```

It understands common typos, missing `sudo`, `cd` into missing directories,
`rm` on a directory, and git command mistakes, among others.

## Persistent tweaks

Settings live at `~/.config/bsh/config`. You do not need to edit it by hand:

```sh
# Add an alias (persists)
alias ll="ls -l"

# Add a directory to PATH (persists)
add_path ~/bin
```

The config file is plain shell-ish text:

```
# ~/.config/bsh/config
if-interactive {
    bfetch # or any other command
}

prompt {
    echo "\F{green}\u@\h \w"
    echo "\$ "
}
alias ll="ls -l"
path ~/bin
log off
shit on
history on
auto-suggestion on
```

Prompt escapes: `\w` current directory, `\W` basename, `\u` user, `\h` host,
`\t` time, `\$`/`\#` privilege, `\n` newline, `\F{color}` ANSI color,
`\[...]` raw escape. Command history is stored at
`~/.local/share/bsh/history.txt`.

## License

bsh is licensed under the MIT License.
See [LICENSE](LICENSE) for the full text.
