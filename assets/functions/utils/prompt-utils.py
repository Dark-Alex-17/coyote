# Python helper for prompting the user from a tool, the counterpart of
# prompt-utils.sh. Load it by path from the env var the shim exports:
#
#   import importlib.util, os
#   spec = importlib.util.spec_from_file_location("prompt_utils", os.environ["LLM_PROMPT_UTILS_PY"])
#   prompt_utils = importlib.util.module_from_spec(spec); spec.loader.exec_module(prompt_utils)
#
# The env var is exported by the global tool shims (run-tool.py / run-tool.ts)
# and by graph script nodes; the agent-scoped shims (run-agent.py /
# run-agent.ts) do not export it.
#
# Helpers: confirm, select, checkbox, input, password, guard_operation,
# guard_path. confirm returns a bool, select the index of the chosen option,
# checkbox the chosen indices in option order, input and password the text.
# Through the prompt channel a select may instead return the free text the
# user typed (a str) when they answered with one of their own.
#
# When Coyote offers a prompt channel (COYOTE_PROMPT_REPLY is set) a helper
# writes one request line to the tool's stdout and reads the answer line back
# from the channel. The request line looks like this, with the marker broken
# here on purpose because the host treats any line carrying the intact marker
# as a request:
#
#   {"coyote_ prompt": {"id": "p123-0f1e...", "action": "confirm", "question": "Go?", "options": []}}
#
# Return convention: every helper returns None when the prompt was cancelled,
# the channel closed, or the reply did not fit the action, so callers fail
# closed. The latter two also print a note to stderr.
#
# _COYOTE_PROMPT_OUT: a bash tool that sourced prompt-utils.sh saves its real
# stdout on a spare fd and exports its number here, so a Python tool it runs
# writes requests to that fd instead of its own (captured) stdout.
#
# The channel answers one prompt at a time: never run two helpers
# concurrently in the same tool, or they will swallow each other's replies.
#
# COYOTE_UI is "repl" or "tui" when a human can answer and "headless" when
# nobody can; tools may branch on it before asking. Under "tui" only the
# channel-backed path is safe: the /dev/tty fallback draws straight over the
# full-screen UI, so under "tui" every helper without a channel
# (COYOTE_PROMPT_REPLY unset) fails closed instead, exit 1 with one line on
# stderr; guard_operation and guard_path then abort the operation they guard.

import getpass
import io
import json
import os
import re
import sys

REPLY_ENV = "COYOTE_PROMPT_REPLY"
OUT_FD_ENV = "_COYOTE_PROMPT_OUT"
CONTROL_CHARS = re.compile(r"[\x00-\x1f\x7f\u0080-\u009f\u202a-\u202e\u2066-\u2069]")


def _show_error(message):
    print(message, file=sys.stderr, flush=True)


def _channel_enabled():
    reply = os.environ.get(REPLY_ENV, "")
    # The bash helper parks its stdout on fd 9, so a channel there is treated
    # as absent by all three helpers alike.
    return reply != "" and reply != "/dev/fd/9"


def _write_all(fd, data):
    while data:
        data = data[os.write(fd, data):]


def _send_request(line):
    out_fd = os.environ.get(OUT_FD_ENV, "")
    data = line.encode("utf-8")
    if out_fd.isascii() and out_fd.isdigit():
        _write_all(int(out_fd), data)
        return
    sys.stdout.flush()
    _write_all(1, data)


def _map_answer(action, answer, options):
    if action == "confirm":
        return answer if isinstance(answer, bool) else None
    if action == "select":
        if isinstance(answer, str):
            return options.index(answer) if answer in options else answer
        return None
    if action == "checkbox":
        if not isinstance(answer, list) or any(item not in options for item in answer):
            return None
        return [index for index, option in enumerate(options) if option in answer]
    if isinstance(answer, str):
        return answer
    return None


def _prompt_channel(action, question, options):
    prompt_id = f"p{os.getpid()}-{os.urandom(8).hex()}"
    request = {"id": prompt_id, "action": action, "question": question, "options": options}

    # The reply side is opened before the request goes out, so a channel that
    # cannot be read never receives a request nobody will answer. Unbuffered
    # so a stray reply behind ours stays in the pipe for the next helper, the
    # way bash's byte-wise read leaves it.
    try:
        replies = open(os.environ[REPLY_ENV], "rb", buffering=0)
    except OSError:
        _show_error("prompt request channel unavailable")
        return None
    with replies:
        try:
            _send_request(json.dumps({"coyote_prompt": request}) + "\n")
        except (OSError, OverflowError, ValueError):
            _show_error("prompt request channel unavailable")
            return None
        while True:
            try:
                raw = replies.readline()
            except OSError:
                _show_error("prompt reply channel closed")
                return None
            if raw == b"":
                _show_error("prompt reply channel closed")
                return None
            try:
                reply = json.loads(raw.decode("utf-8", errors="replace"))
            except (ValueError, RecursionError):
                continue
            if isinstance(reply, dict) and reply.get("id") == prompt_id:
                break

    answer = reply.get("answer")
    if answer is None:
        return None
    mapped = _map_answer(action, answer, options)
    if mapped is None:
        _show_error("prompt reply unusable")
    return mapped


TUI_NO_CHANNEL = "prompts are not available in the TUI without the reply channel"


# Under the full-screen UI the terminal belongs to the UI's own reader: a
# fallback prompt would be invisible and a typed answer (a password included)
# would land in its composer. Without the channel the helper fails closed.
def _refuse_terminal_under_tui():
    if os.environ.get("COYOTE_UI", "") == "tui":
        _show_error(TUI_NO_CHANNEL)
        sys.exit(1)


def _open_tty():
    # Built the way getpass does it: open("/dev/tty", "r+") wraps the fd in a
    # BufferedRandom, whose seek test fails on a Linux tty (ESPIPE), so the
    # open would raise and the fallback would silently take over.
    fd = os.open("/dev/tty", os.O_RDWR | getattr(os, "O_NOCTTY", 0))
    return io.TextIOWrapper(io.FileIO(fd, "w+"), write_through=True)


# Question and option text reaches the terminal unescaped in fallback mode,
# so control and bidi-override characters (which could repaint or reorder
# the prompt) are dropped from each piece before it is formatted.
def _clean(text):
    return CONTROL_CHARS.sub("", str(text))


def _tty_line(prompt, option_lines=()):
    _refuse_terminal_under_tui()
    text = "".join(option_lines) + prompt
    try:
        tty = _open_tty()
    except OSError:
        sys.stderr.write(text)
        sys.stderr.flush()
        line = sys.stdin.readline()
    else:
        with tty:
            tty.write(text)
            line = tty.readline()
    if line == "":
        return None
    return line.rstrip("\r\n")


def _tty_list(question, options):
    option_lines = [f"  {index}) {_clean(option)}\n" for index, option in enumerate(options, start=1)]
    return _tty_line(f"? {_clean(question)} ", option_lines)


def _parse_index(text, options):
    if not (text.isascii() and text.isdigit()):
        return None
    index = int(text) - 1
    if 0 <= index < len(options):
        return index
    return None


def confirm(question):
    if _channel_enabled():
        return _prompt_channel("confirm", question, [])
    line = _tty_line(f"? {_clean(question)} (y/N) ")
    if line is None:
        return None
    return line.strip().lower().startswith("y")


def select(question, options):
    options = [str(option) for option in options]
    if _channel_enabled():
        return _prompt_channel("select", question, options)
    line = _tty_list(question, options)
    if line is None:
        return None
    return _parse_index(line.strip(), options)


def checkbox(question, options):
    options = [str(option) for option in options]
    if _channel_enabled():
        return _prompt_channel("checkbox", question, options)
    line = _tty_list(question, options)
    if line is None:
        return None
    chosen = set()
    for word in line.split():
        index = _parse_index(word, options)
        if index is None:
            return None
        chosen.add(index)
    return sorted(chosen)


def input(question):
    if _channel_enabled():
        return _prompt_channel("input", question, [])
    return _tty_line(f"? {_clean(question)} ")


def password(question):
    if _channel_enabled():
        return _prompt_channel("password", question, [])
    _refuse_terminal_under_tui()
    try:
        return getpass.getpass(f"? {_clean(question)} ")
    except EOFError:
        return None


def _auto_confirmed():
    return bool(os.environ.get("AUTO_CONFIRM") or os.environ.get("LLM_AGENT_VAR_AUTO_CONFIRM"))


def _abort_unless_confirmed(question):
    if confirm(question) is not True:
        _show_error("ERROR: Operation aborted!")
        sys.exit(1)


def guard_operation(question="Are you sure you want to continue?"):
    if _auto_confirmed():
        return
    _abort_unless_confirmed(question)


def guard_path(path, question):
    if _auto_confirmed():
        return
    target = os.path.realpath(path)
    cwd = os.path.realpath(os.getcwd())
    if target == cwd or target.startswith(cwd.rstrip(os.sep) + os.sep):
        return
    _abort_unless_confirmed(question)
