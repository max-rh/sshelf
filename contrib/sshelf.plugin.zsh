# sshelf shell integration for zsh.
#
# Binds a key that opens sshelf's host picker in the rows under your prompt and drops what you
# choose onto the command line, so you can reach a host without leaving the line you are typing.
# The prompt is where you left it when the list closes.
#
#   Alt-s            pick a host; on an empty line you get `sshelf <host>`, ready to run
#   Alt-s mid-line   insert the host name where the cursor is, e.g. after `scp file `
#
# Mark several rows in the picker with ^s and every marked name is inserted, space separated,
# which is what you want in front of a loop.
#
# Install by sourcing this file from ~/.zshrc, or with any plugin manager:
#
#   Oh My Zsh   copy contrib/ to $ZSH_CUSTOM/plugins/sshelf, then plugins=(... sshelf)
#   zinit       zinit ice as'plugin' pick'contrib/sshelf.plugin.zsh'; zinit load max-rh/sshelf
#   antidote    max-rh/sshelf path:contrib
#   manual      source /path/to/contrib/sshelf.plugin.zsh
#
# Settings, set before sourcing:
#
#   SSHELF_BIN        the binary to call (default: sshelf)
#   SSHELF_PICK_KEY   the binding (default: ^[s, i.e. Alt-s). Set it to an empty string to
#                     bind nothing yourself and call `bindkey '<key>' sshelf-pick` later.
#
# ^s is free in zsh only after `stty -ixon`, since the terminal claims it for flow control by
# default. Alt-s needs Option-as-Meta turned on in Terminal.app; iTerm2 and most Linux
# terminals send it already.

: ${SSHELF_BIN:=sshelf}
: ${SSHELF_PICK_KEY:='^[s'}

# The picker draws on stderr (the terminal) and prints the chosen name(s) on stdout, so the
# command substitution below captures names and nothing else. A non-zero status means the user
# backed out, and then the line is left exactly as it was.
sshelf-pick() {
  local out
  if ! out=$("${SSHELF_BIN}" pick 2>/dev/tty); then
    zle reset-prompt
    return 0
  fi
  local -a picked
  picked=("${(@f)out}")
  if [[ -z ${picked[1]} ]]; then
    zle reset-prompt
    return 0
  fi

  if [[ -z $BUFFER && ${#picked} -eq 1 ]]; then
    # An empty line is a connect waiting to happen: write the whole command.
    LBUFFER="${SSHELF_BIN} ${picked[1]}"
  else
    # Mid-line, the names are an argument to whatever is already being typed.
    [[ -n $LBUFFER && $LBUFFER != *[[:space:]] ]] && LBUFFER+=' '
    LBUFFER+="${(j: :)picked}"
  fi
  zle reset-prompt
}

if [[ -o interactive ]]; then
  zle -N sshelf-pick
  [[ -n $SSHELF_PICK_KEY ]] && bindkey "$SSHELF_PICK_KEY" sshelf-pick
fi
