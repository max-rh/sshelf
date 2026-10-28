# sshelf shell integration for bash.
#
# Binds a key that opens sshelf's host picker in the rows under your prompt and drops what you
# choose onto the command line. The zsh version is contrib/sshelf.plugin.zsh.
#
#   Alt-s            pick a host; on an empty line you get `sshelf <host>`, ready to run
#   Alt-s mid-line   insert the host name where the cursor is, e.g. after `scp file `
#
# Mark several rows in the picker with ^s and every marked name is inserted, space separated.
#
# Install by sourcing this file from ~/.bashrc:
#
#   source /path/to/contrib/sshelf.bash
#
# Settings, set before sourcing:
#
#   SSHELF_BIN        the binary to call (default: sshelf)
#   SSHELF_PICK_KEY   the binding in readline syntax (default: \es, i.e. Alt-s)
#
# The key binding needs bash 4.0 or newer, because editing the line from a `bind -x` command is
# what READLINE_LINE is for and bash 3.2 (the one Apple still ships) cannot do it. On 3.2 this
# file defines `sshelf-pick` as a plain command instead: run it, and it connects to what you
# chose.

: "${SSHELF_BIN:=sshelf}"
: "${SSHELF_PICK_KEY:=\\es}"

# The picker draws on stderr (the terminal) and prints the chosen name(s) on stdout, so the
# command substitution captures names and nothing else. Backing out leaves the line untouched.
__sshelf_pick() {
  local out
  if ! out=$("${SSHELF_BIN}" pick 2>/dev/tty); then
    return 0
  fi
  [[ -z $out ]] && return 0
  local names left right=''
  names=$(printf '%s' "$out" | tr '\n' ' ')
  names=${names% }

  if [[ -z $READLINE_LINE ]] && [[ $names != *" "* ]]; then
    READLINE_LINE="${SSHELF_BIN} ${names}"
  else
    left=${READLINE_LINE:0:$READLINE_POINT}
    right=${READLINE_LINE:$READLINE_POINT}
    [[ -n $left && $left != *[[:space:]] ]] && left+=' '
    READLINE_LINE="${left}${names}${right}"
  fi
  READLINE_POINT=$((${#READLINE_LINE} - ${#right}))
}

# Pick a host and connect to it, for shells that cannot rewrite the command line.
sshelf-pick() {
  local host
  host=$("${SSHELF_BIN}" pick 2>/dev/tty) || return 1
  [[ -z $host ]] && return 1
  "${SSHELF_BIN}" ${host}
}

if [[ $- == *i* ]]; then
  if ((BASH_VERSINFO[0] >= 4)); then
    bind -x "\"${SSHELF_PICK_KEY}\": __sshelf_pick"
  fi
fi
