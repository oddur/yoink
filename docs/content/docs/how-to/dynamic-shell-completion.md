---
title: Add dynamic shell completion
description: Connect the hidden __complete helper to Bash or Zsh so that TAB completes service names, host addresses, and config files.
---

`yoink completions <shell>` completes subcommand names, flag names, and value enums. This page adds completion for service names, host addresses, and config file paths. The [CLI reference](/docs/reference/cli#dynamic-shell-completion) describes the `__complete` helper that supplies these values.

## Bash

1. Add this function to `~/.bashrc`, alongside `source <(yoink completions bash)`:

   ```bash
   # ~/.bashrc (alongside `source <(yoink completions bash)`)
   _yoink_dynamic() {
       local cur="${COMP_WORDS[COMP_CWORD]}"
       local prev="${COMP_WORDS[COMP_CWORD-1]}"
       # Forward any -c / --config that's already on the line so that
       # `yoink -c staging.yoink.yaml shell <TAB>` lists *staging's*
       # services, not the default config's.
       local fwd=()
       for ((i=1; i<COMP_CWORD; i++)); do
           case "${COMP_WORDS[i]}" in
               -c|--config)
                   fwd=(-c "${COMP_WORDS[i+1]}")
                   ;;
           esac
       done
       case "$prev" in
           -c|--config)
               COMPREPLY=( $(compgen -W "$(yoink __complete configs 2>/dev/null)" -- "$cur") )
               return 0
               ;;
           --service|-s|shell|ssh|exec|logs|restart|kill|debug|version|history|rollback|pull|diff)
               COMPREPLY=( $(compgen -W "$(yoink "${fwd[@]}" __complete services 2>/dev/null)" -- "$cur") )
               return 0
               ;;
           --host)
               COMPREPLY=( $(compgen -W "$(yoink "${fwd[@]}" __complete hosts 2>/dev/null)" -- "$cur") )
               return 0
               ;;
       esac
   }
   complete -F _yoink_dynamic -o default yoink
   ```

2. Source `~/.bashrc` again.

## Zsh

1. Put the `compdef` snippet from `yoink completions zsh` into your `fpath`.
2. Add a wrapper that calls `yoink __complete <kind>` for the values that you want.

The helper prints paths or names, separated by `\n`.

## Make sure that completion works

Press `<TAB>` in each of these positions:

```sh
yoink -c <TAB>                    # all yoink configs in this repo
yoink shell <TAB>                 # services from the default config
yoink -c staging.yoink.yaml shell <TAB>   # services from staging
```

## See also

- [CLI reference](/docs/reference/cli#dynamic-shell-completion): the `__complete` subcommands and their sources.
- [Install yoink](/docs/how-to/install): the installation of the binary.
