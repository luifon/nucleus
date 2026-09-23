# claude-wrapper.zsh — load the operator-private skill tree in manual sessions.
#
# Operator-private skills live in <main checkout>/.nucleus/.claude/skills/
# (gitignored, ADR-032). Claude Code loads a `.claude/skills/` tree only from
# the project dir or from a dir passed with `--add-dir`, so a plain `claude`
# started inside Nucleus would not see them. The bots and reminder fires pass
# the flag in code (core `build_claude_args`, the WhatsApp `claude_session.ts`).
# This file defines a zsh function `claude` that does the same for sessions
# the operator starts by hand.
#
# Behavior: when $PWD is inside a Nucleus checkout (main checkout or a linked
# worktree, both resolved to the main checkout through the common git dir) and
# <main>/.nucleus/.claude/skills exists, it runs
#   command claude --add-dir=<main>/.nucleus "$@"
# unless the arguments already contain `--add-dir <main>/.nucleus` (the
# WhatsApp bot spawns bare `claude` through tmux's zsh and already passes it).
# Anywhere else it runs `command claude "$@"` unchanged.
# The `=` form is required: `--add-dir` takes every following non-flag
# argument, so `--add-dir X mcp list` would read `mcp` and `list` as
# directories; `--add-dir=X` takes exactly one value.
#
# Install: source this file from ~/.zshenv, so interactive, login and
# `zsh -c` shells (tmux windows included) all define the function:
#   source <path to this checkout>/tools/claude-wrapper.zsh
#
# Known gaps: shells other than zsh (bash, sh) do not get the function, and
# launches that call the binary by absolute path (for example `claude-cli://`
# deep links) bypass it.

claude() {
  emulate -L zsh
  local top common main nucleus_dir
  top="$(git rev-parse --show-toplevel 2>/dev/null)" || top=""
  if [[ -n "$top" ]]; then
    common="$(git -C "$top" rev-parse --path-format=absolute --git-common-dir 2>/dev/null)" || common=""
    main="${common%/.git}"
    [[ -n "$main" && -d "$main" ]] || main="$top"
    nucleus_dir="$main/.nucleus"
    if [[ -f "$main/core/src/claude_session.rs" && -d "$nucleus_dir/.claude/skills" ]]; then
      # `--add-dir` takes one or more values (`--add-dir a b`), or `=a`.
      local arg in_add=0
      for arg in "$@"; do
        case "$arg" in
          --add-dir) in_add=1; continue ;;
          --add-dir=*) arg="${arg#--add-dir=}"; in_add=0 ;;
          -*) in_add=0; continue ;;
          *) (( in_add )) || continue ;;
        esac
        if [[ "${arg:A}" == "${nucleus_dir:A}" ]]; then
          command claude "$@"
          return
        fi
      done
      command claude "--add-dir=$nucleus_dir" "$@"
      return
    fi
  fi
  command claude "$@"
}
