#!/usr/bin/env bash

# Bash helper utilities for prompting users.
# This is a modified version of the excellent Bash TUI toolkit
# (https://github.com/timo-reymann/bash-tui-toolkit)
#
# It includes the following functions for you to use in your
# bash tool commands:
#
# - password          Password prompt
# - checked           Checkbox
# - text              Text input with validation
# - list              Select an option from a given list
# - range             Prompt the user for a value within a range
# - confirm           Confirmation prompt
# - editor            Open the user's preferred editor for input
# - detect_os         Detect the current OS
# - get_opener        Get the file opener for the current OS
# - open_link         Open the given link in the default browser
# See https://github.com/timo-reymann/bash-tui-toolkit/blob/main/test.sh
# for examples on how to use these commands
#
# - guard_operation   Prompt for permission to run an operation
# - guard_path        Prompt for permission to perform path operations
# - patch_file        Patch a file
# - error             Log an error
# - warn              Log a warning
# - info              Log info
# - debug             Log a debug message
# - trace             Log a trace message
# - red               Output given text in red
# - green             Output given text in green
# - gold              Output given text in gold
# - blue              Output given text in blue
# - magenta           Output given text in magenta
# - cyan              Output given text in cyan
# - white             Output given text in white

# shellcheck disable=SC2034
red=$(tput setaf 1)
green=$(tput setaf 2)
gold=$(tput setaf 3)
blue=$(tput setaf 4)
magenta=$(tput setaf 5)
cyan=$(tput setaf 6)
white=$(tput setaf 7)

default=$(tput sgr0)
gray=$(tput setaf 243)

bold=$(tput bold)
underlined=$(tput smul)

error() {
  echo -e "${red}${bold}ERROR:${default}${red} $1${default}"
}

warn() {
  echo -e "${gold}${bold}WARN:${default}${gold} $1${default}"
}

info() {
  echo -e "${cyan}${bold}INFO:${default}${cyan} $1${default}"
}

debug() {
	echo -e "${blue}${bold}DEBUG:${default}${blue} $1${default}"
}

trace() {
	echo -e "${gray}${bold}TRACE:${default}${gray} $1${default}"
}

success() {
	echo -e "${green}${bold}SUCCESS:${default}${green} $1${default}"
}

red() {
  echo -e "${red}$1${default}"
}

green() {
  echo -e "${green}$1${default}"
}

gold() {
  echo -e "${gold}$1${default}"
}

blue() {
  echo -e "${blue}$1${default}"
}

magenta() {
  echo -e "${magenta}$1${default}"
}

cyan() {
  echo -e "${cyan}$1${default}"
}

white() {
  echo -e "${white}$1${default}"
}

_read_stdin() {
  read -r "$@" </dev/tty
}

_get_cursor_row() {
  declare IFS=';'
  _read_stdin -sdR -p $'\E[6n' ROW COL
  echo "${ROW#*[}"
}

_cursor_blink_on() {
  echo -en "\033[?25h" >&2
}
_cursor_blink_off() {
  echo -en "\033[?25l" >&2
}

_cursor_to() {
  echo -en "\033[$1;$2H" >&2
}

# shellcheck disable=SC2154
_key_input() {
  declare ESC=$'\033'
  declare IFS=''
  _read_stdin -rsn1 a
  if [[ "$ESC" == "$a" ]]; then
    _read_stdin -rsn2 b
  fi

  declare input="${a}${b}"
  case "$input" in
    "${ESC}[A" | "k") echo up ;;
    "${ESC}[B" | "j") echo down ;;
    "${ESC}[C" | "l") echo right ;;
    "${ESC}[D" | "h") echo left ;;
    '') echo enter ;;
    ' ') echo space ;;
  esac
}

_new_line_foreach_item() {
  count=0
  while [[ $count -lt $1  ]]; do
      echo "" >&2
      ((count++))
  done
}

_prompt_text() {
  echo -en "\033[32m?\033[0m\033[1m ${1}\033[0m " >&2
}

_decrement_selected() {
  declare selected=$1
  ((selected--))
  if [[ "${selected}" -lt 0 ]]; then
    selected=$(($2 - 1))
  fi

  echo -n $selected
}

_increment_selected() {
  declare selected=$1
  ((selected++))

  if [[ "${selected}" -ge "${opts_count}" ]]; then
    selected=0
  fi

  echo -n $selected
}

# shellcheck disable=SC2154
input() {
  _prompt_text "$1"
  echo -en "\033[36m\c" >&2
  _read_stdin -r text
  echo -n "${text}"
}

confirm() {
  trap "stty echo; exit" EXIT
  _prompt_text "$1 (y/N)"
  echo -en "\033[36m\c " >&2

  declare first_row
  first_row=$(_get_cursor_row)
  declare current_row
  current_row=$((first_row - 1))
  declare result=""
  echo -n " " >&2

  while true; do
    echo -e "\033[1D\c " >&2
    _read_stdin -n1 result

    case "$result" in
      y | Y)
        echo -n 1
        break
        ;;
      n | N | *)
        echo -n 0
        break
        ;;
    esac
  done

  echo -en "\033[0m" >&2
  echo "" >&2
}

list() {
  _prompt_text "$1 "
  declare opts=("${@:2}")
  declare opts_count=$(($# - 1))

  _new_line_foreach_item "${#opts[@]}"

  declare last_row
  last_row=$(_get_cursor_row)
  declare first_row
  first_row=$((last_row - opts_count + 1))

  trap "_cursor_blink_on; stty echo; exit" 2

  _cursor_blink_off

  declare selected=0
  while true; do
    declare idx=0
    for opt in "${opts[@]}"; do
      _cursor_to $((first_row + idx))

      if [[ $idx -eq "$selected" ]]; then
        printf "\033[0m\033[36m❯\033[0m \033[36m%s\033[0m" "$opt" >&2
      else
        printf "  %s" "$opt" >&2
      fi

      ((idx++))
    done

    case $(_key_input) in
      enter) break  ;;
      up) selected=$(_decrement_selected "${selected}" "${opts_count}")  ;;
      down) selected=$(_increment_selected "${selected}" "${opts_count}")  ;;
    esac
  done

  echo -en "\n" >&2
  _cursor_to "${last_row}"
  _cursor_blink_on
  echo -n "${selected}"
}

checkbox() {
  _prompt_text "$1"
  declare opts
  opts=("${@:2}")
  declare opts_count
  opts_count=$(($# - 1))

  _new_line_foreach_item "${#opts[@]}"

  declare last_row
  last_row=$(_get_cursor_row)
  declare first_row
  first_row=$((last_row - opts_count + 1))

  trap "_cursor_blink_on; stty echo; exit" 2

  _cursor_blink_off

  declare selected=0
  declare checked=()
  while true; do
    declare idx=0
    for opt in "${opts[@]}"; do
      _cursor_to $((first_row + idx))
      declare icon="◯"

      for item in "${checked[@]}"; do
        if [[ "$item" == "$idx" ]]; then
          icon="◉"
          break
        fi
      done

      if [[ $idx -eq "$selected" ]]; then
        printf "%s \e[0m\e[36m❯\e[0m \e[36m%-50s\e[0m" "$icon" "$opt" >&2
      else
        printf "%s   %-50s" "$icon" "$opt" >&2
      fi

      ((idx++))
    done

    case $(_key_input) in
      enter) break ;;
      space)
        declare found=0
        for item in "${checked[@]}"; do
          if [[ "$item" == "$selected" ]]; then
            found=1
            break
          fi
        done

        if [ $found -eq 1 ]; then
          checked=("${checked[@]/$selected/}")
        else
          checked+=("${selected}")
        fi
        ;;
      up) selected=$(_decrement_selected "${selected}" "${opts_count}")  ;;
      down) selected=$(_increment_selected "${selected}" "${opts_count}")  ;;
    esac
  done

  _cursor_to "${last_row}"
  _cursor_blink_on
  IFS="" echo -n "${checked[@]}"
}

password() {
  _prompt_text "$1"
  echo -en "\033[36m" >&2
  declare password=''
  declare IFS=

  while _read_stdin -r -s -n1 char; do
    [[ -z "${char}" ]] && {
      printf '\n' >&2
      break
    }

    if [[ "${char}" == $'\x7f' ]]; then
      if [[ "${#password}" -gt 0 ]]; then
        password="${password%?}"
        echo -en '\b \b' >&2
      fi
    else
      password+=$char
      echo -en '*' >&2
    fi
  done

  echo -en "\e[0m" >&2
  echo -n "${password}"
}

editor() {
  tmpfile=$(mktemp)
  _prompt_text "$1"
  echo "" >&2
  "${EDITOR:-vi}" "${tmpfile}" >/dev/tty
  echo -en "\033[36m" >&2
  sed -e 's/^/  /' "${tmpfile}" >&2
  echo -en "\033[0m" >&2
  cat "${tmpfile}"
}

with_validate() {
  while true; do
    declare val
    val="$(eval "$1")"

    if ($2 "$val" >/dev/null); then
      echo "$val"
      break
    else
      show_error "$($2 "$val")"
    fi
  done
}

range() {
  declare min="$2"
  declare current="$3"
  declare max="$4"
  declare selected="${current}"
  declare max_len_current
  max_len_current=0

  if [[ "${#min}" -gt "${#max}" ]]; then
    max_len_current="${#min}"
  else
    max_len_current="${#max}"
  fi

  declare padding
  padding="$(printf "%-${max_len_current}s" "")"
  declare first_row
  first_row=$(_get_cursor_row)
  declare current_row
  current_row=$((first_row - 1))

  trap "_cursor_blink_on; stty echo; exit" 2

  _cursor_blink_off

  _check_range() {
    val=$1

    if [[ "$val" -gt "$max" ]]; then
      val=$min
    elif [[ "$val" -lt "$min" ]]; then
      val=$max
    fi

    echo "$val"
  }

  while true; do
    _prompt_text "$1"
    printf "\033[37m%s\033[0m \033[1;90m❮\033[0m \033[36m%s%s\033[0m \033[1;90m❯\033[0m \033[37m%s\033[0m\n" "$min" "${padding:${#selected}}" "$selected" "$max" >&2

    case $(_key_input) in
      enter)
        break
        ;;
      left)
        selected="$(_check_range $((selected - 1)))"
        ;;
      right)
        selected="$(_check_range $((selected + 1)))"
        ;;
    esac

    _cursor_to "$current_row"
  done

  echo "$selected"
}

validate_present() {
  if [ "$1" != "" ]; then
    return 0
  else
    error "Please specify the value"
    return 1
  fi
}

show_error() {
  echo -e "\033[91;1m✘ $1\033[0m" >&2
}

show_success() {
  echo -e "\033[92;1m✔ $1\033[0m" >&2
}

detect_os() {
  case "$OSTYPE" in
    solaris*) echo "solaris"  ;;
    darwin*)  echo "macos"  ;;
    linux*)   echo "linux"  ;;
    bsd*)     echo "bsd"  ;;
    msys*)    echo "windows"  ;;
    cygwin*)  echo "windows"  ;;
    *)        echo "unknown"  ;;
  esac
}

get_opener() {
  declare cmd

  case "$(detect_os)" in
    macos)  cmd="open"  ;;
    linux)   cmd="xdg-open"  ;;
    windows) cmd="start"  ;;
    *)       cmd=""  ;;
  esac

  echo "$cmd"
}

open_link() {
  cmd="$(get_opener)"

  if [[ "$cmd" == "" ]]; then
    error "Your platform is not supported for opening links."
    red "Please open the following URL in your preferred browser:"
    red " ${1}"
    return 1
  fi

  $cmd "$1"

  if [[ $? -eq 1 ]]; then
    error "Failed to open your browser."
    red "Please open the following URL in your browser:"
    red "${1}"
    return 1
  fi

  return 0
}

guard_operation() {
  if [[ -t 1 ]]; then
  	if [[ -z "$AUTO_CONFIRM" ]]; then
			ans="$(confirm "${1:-Are you sure you want to continue?}")"

			if [[ "$ans" == 0 ]]; then
				error "Operation aborted!" 2>&1
				exit 1
			fi
		fi
  fi
}

# Here is an example of a patch block that can be applied to modify the file to request the user's name:
# --- a/hello.py
# +++ b/hello.py
# \@@ ... @@
#  def hello():
# -    print("Hello World")
# +    name = input("What is your name? ")
# +    print(f"Hello {name}")
patch_file() {
  awk '
    FNR == NR {
      lines[FNR] = $0
      next;
    }

    {
      patchLines[FNR] = $0
    }

    END {
      totalPatchLines=length(patchLines)
      totalLines = length(lines)
      patchLineIndex = 1

      mode = "none"

      while (patchLineIndex <= totalPatchLines) {
        line = patchLines[patchLineIndex]

        if (line ~ /^--- / || line ~ /^\+\+\+ /) {
          patchLineIndex++
          continue
        }

        if (line ~ /^@@ /) {
          mode = "hunk"
          hunkIndex++
          patchLineIndex++
          continue
        }

        if (mode == "hunk") {
          while (patchLineIndex <= totalPatchLines && line ~ /^[-+ ]|^\s*$/ && line !~ /^--- /) {
            sanitizedLine = substr(line, 2)

            if (line !~ /^\+/) {
              hunkTotalOriginalLines[hunkIndex]++;
              hunkOriginalLines[hunkIndex,hunkTotalOriginalLines[hunkIndex]] = sanitizedLine
            }

            if (line !~ /^-/) {
              hunkTotalUpdatedLines[hunkIndex]++;
              hunkUpdatedLines[hunkIndex,hunkTotalUpdatedLines[hunkIndex]] = sanitizedLine
            }

            patchLineIndex++
            line = patchLines[patchLineIndex]
          }

          mode = "none"
        } else {
          patchLineIndex++
        }
      }

      if (hunkIndex == 0) {
        print "error: no patch" > "/dev/stderr"
        exit 1
      }

      totalHunks = hunkIndex
      hunkIndex = 1

      for (lineIndex = 1; lineIndex <= totalLines; lineIndex++) {
        line = lines[lineIndex]
        nextLineIndex = 0

        if (hunkIndex <= totalHunks && line == hunkOriginalLines[hunkIndex,1]) {
          nextLineIndex = lineIndex + 1

          for (i = 2; i <= hunkTotalOriginalLines[hunkIndex]; i++) {
            if (lines[nextLineIndex] != hunkOriginalLines[hunkIndex,i]) {
              nextLineIndex = 0
              break
            }

            nextLineIndex++
          }
        }

        if (nextLineIndex > 0) {
          for (i = 1; i <= hunkTotalUpdatedLines[hunkIndex]; i++) {
            print hunkUpdatedLines[hunkIndex,i]
          }

          hunkIndex++
          lineIndex = nextLineIndex - 1;
        } else {
          print line
          }
        }

        if (hunkIndex != totalHunks + 1) {
          print "error: unable to apply patch" > "/dev/stderr"
          exit 1
        }
    }

    function inspectHunks() {
      print "/* Begin inspecting hunks"

      for (i = 1; i <= totalHunks; i++) {
        print ">>>>>> Original"

        for (j = 1; j <= hunkTotalOriginalLines[i]; j++) {
          print hunkOriginalLines[i,j]
        }

        print "======"

        for (j = 1; j <= hunkTotalUpdatedLines[i]; j++) {
          print hunkUpdatedLines[i,j]
        }

        print "<<<<<< Updated"
      }

      print "End inspecting hunks */\n"
    }' "$1" "$2"
}

guard_path() {
  if [[ "$#" -ne 2 ]]; then
    echo "Usage: guard_path <path> <confirmation_prompt>" >&2
    exit 1
  fi

  if [[ -t 1 ]]; then
    path="$(_to_real_path "$1")"
    confirmation_prompt="$2"

    if [[ ! "$path" == "$(pwd)"* && -z "$AUTO_CONFIRM" ]]; then
			ans="$(confirm "$confirmation_prompt")"

			if [[ "$ans" == 0 ]]; then
				error "Operation aborted!" >&2
				exit 1
			fi
    fi
  fi
}

_to_real_path() {
  path="$1"

  if [[ $OS == "Windows_NT" ]]; then
    path="$(cygpath -u "$path")"
  fi

  awk -v path="$path" -v pwd="$PWD" '
    BEGIN {
      if (path !~ /^\//) {
        path = pwd "/" path
      }

      if (path ~ /\/\.{1,2}?$/) {
        isDir = 1
      }

      split(path, parts, "/")
      newPartsLength = 0

      for (i = 1; i <= length(parts); i++) {
        part = parts[i]
        if (part == "..") {
          if (newPartsLength > 0) {
            delete newParts[newPartsLength--]
          }
        } else if (part != "." && part != "") {
          newParts[++newPartsLength] = part
        }
      }

      if (isDir == 1 || newPartsLength == 0) {
        newParts[++newPartsLength] = ""
      }

      printf "/"

      for (i = 1; i <= newPartsLength; i++) {
        newPart = newParts[i]
        printf newPart
        if (i < newPartsLength) {
          printf "/"
        }
      }
    }'
}
