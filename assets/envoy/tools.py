"""Read-only file tools for the built-in envoy agent.

No shebang on purpose: the shim runs this module with the interpreter Coyote
probed at startup, never with one named here.

Every path is confined to ENVOY_ROOT_DIR. Anything absolute, anything with a
`..` component, anything that resolves outside the root through a symlink,
anything that resolves into a directory named by ENVOY_DENY_DIRS (Coyote's
own config and cache dirs), and every `.git`, `.env*` or well-known
credential-store entry inside the root is refused. The name list is
best-effort and cannot be complete; the root and deny dirs are the real
boundary. Names are compared both as written (case-folded) and without
Windows trailing dots, spaces and `::$DATA` streams, so a case-insensitive
filesystem cannot be used to sidestep the deny list.
Refusals name only the path the caller supplied, never an absolute one.
"""

import fnmatch
import os
import re
import time
from typing import Callable, Optional

# The tool docstrings below and the Rust test module mirror these numbers.
MAX_READ_BYTES = 262_144
MAX_GREP_MATCHES = 200
MAX_GLOB_RESULTS = 500
MAX_LINE_CHARS = 2000
DEFAULT_READ_LINES = 2000
MAX_SCAN_ENTRIES = 20_000
BINARY_PROBE_BYTES = 8192
MAX_PATTERN_CHARS = 256
SEARCH_BUDGET_SECONDS = 5.0
BUDGET_CHECK_EVERY = 256

_NO_ROOT = {"error": "ENVOY_ROOT_DIR is not set to a directory; refusing"}
_TRUNCATED_MARK = " [truncated]"

_DENIED_EXACT = frozenset(
    {
        ".git",
        ".env",
        ".envrc",
        ".ssh",
        ".aws",
        ".gnupg",
        ".netrc",
        ".npmrc",
        ".pypirc",
        ".docker",
        ".coyote",
        ".coyote_password",
        ".mcp.json",
        ".git-credentials",
        ".kube",
        ".pgpass",
        ".password-store",
        ".terraform.d",
        ".bash_history",
        ".zsh_history",
    }
)
_DENIED_PREFIXES = (".env.", "id_rsa", "id_ed25519", "id_ecdsa", "id_dsa")
_DENIED_SUFFIXES = (
    ".pem",
    ".key",
    ".p12",
    ".pfx",
    ".tfstate",
    ".tfstate.backup",
    ".tfvars",
    ".env",
)


class _Refused(Exception):
    pass


class _BudgetExhausted(Exception):
    pass


def _root() -> Optional[str]:
    value = os.environ.get("ENVOY_ROOT_DIR", "")
    if not value or not os.path.isabs(value) or not os.path.isdir(value):
        return None
    return os.path.realpath(value)


def _deny_dirs():
    """Coyote sets the key even when there is nothing to deny; an absent key
    means the tool was not launched by Coyote, so `_resolve` must refuse."""
    if "ENVOY_DENY_DIRS" not in os.environ:
        return None
    dirs = []
    for entry in os.environ["ENVOY_DENY_DIRS"].split(os.pathsep):
        if entry:
            dirs.append(os.path.realpath(entry))
    return dirs


def _normal_name(name: str) -> str:
    return name.split(":", 1)[0].rstrip(". ").casefold()


def _denied_form(name: str) -> bool:
    return (
        name in _DENIED_EXACT
        or name.startswith(_DENIED_PREFIXES)
        or name.endswith(_DENIED_SUFFIXES)
    )


def _denied_name(name: str) -> bool:
    return _denied_form(name.casefold()) or _denied_form(_normal_name(name))


def _check_denied(parts, shown: str) -> None:
    for part in parts:
        if _denied_name(part):
            raise _Refused(f"refused: {shown}")


def _within(root: str, real: str) -> bool:
    root = os.path.normcase(root)
    real = os.path.normcase(real)
    try:
        return os.path.commonpath([root, real]) == root
    except ValueError:
        return False


def _in_deny_dirs(deny_dirs, real: str) -> bool:
    # Case-folded on purpose: on a case-insensitive filesystem realpath keeps
    # the caller's spelling, and refusing a same-spelled sibling on a
    # case-sensitive one is the safe side of that trade.
    folded = real.casefold()
    return any(_within(deny.casefold(), folded) for deny in deny_dirs)


def _rel(root: str, real: str) -> str:
    rel = os.path.relpath(real, root)
    if rel == ".":
        return "."
    return "/".join(rel.split(os.sep))


def _denied_rel(rel: str) -> bool:
    return rel != "." and any(_denied_name(part) for part in rel.split("/"))


def _resolve(root: str, deny_dirs, user_path: Optional[str]):
    if deny_dirs is None:
        raise _Refused("ENVOY_DENY_DIRS is not set; refusing")
    if user_path is None or user_path == "":
        user_path = "."
    if not isinstance(user_path, str):
        raise _Refused("path must be a string")
    if (
        os.path.isabs(user_path)
        or re.match(r"^[A-Za-z]:", user_path)
        or user_path.startswith("\\\\")
        or user_path.startswith("/")
    ):
        raise _Refused("absolute paths are refused; give a path relative to the root")
    parts = [part for part in re.split(r"[\\/]+", user_path) if part not in ("", ".")]
    shown = "/".join(parts) or "."
    if ".." in parts:
        raise _Refused(f"path escapes the root: {shown}")
    _check_denied(parts, shown)
    real = os.path.realpath(os.path.join(root, *parts)) if parts else root
    if not _within(root, real):
        raise _Refused(f"path escapes the root: {shown}")
    if _in_deny_dirs(deny_dirs, real) or _denied_rel(_rel(root, real)):
        raise _Refused(f"refused: {shown}")
    if not os.path.exists(real):
        raise _Refused(f"not found: {shown}")
    return real, shown


def _read_capped(real: str):
    with open(real, "rb") as handle:
        data = handle.read(MAX_READ_BYTES + 1)
    if b"\x00" in data[:BINARY_PROBE_BYTES]:
        return None
    truncated = len(data) > MAX_READ_BYTES
    return data[:MAX_READ_BYTES].decode("utf-8", errors="replace"), truncated


def _clip(line: str) -> str:
    if len(line) > MAX_LINE_CHARS:
        return line[:MAX_LINE_CHARS] + _TRUNCATED_MARK
    return line


_LINE_BREAK = re.compile(r"\r\n|\r|\n")


def _lines(text: str):
    """Lines as editors and `grep -n` count them: `str.splitlines` would also
    break on form feeds, vertical tabs and Unicode separators."""
    lines = _LINE_BREAK.split(text)
    if lines and lines[-1] == "":
        lines.pop()
    return lines


def _scan(root: str, deny_dirs, start: str):
    """Regular files under `start`, as (real, root-relative) pairs, in sorted
    walk order. Never descends into a symlinked, denied or deny-dir
    directory; skips denied names, files whose target is denied, and
    symlinks that resolve outside the root."""
    files = []
    seen = 0
    for dirpath, dirnames, filenames in os.walk(start, followlinks=False):
        kept = []
        for name in sorted(dirnames):
            seen += 1
            if seen > MAX_SCAN_ENTRIES:
                return files, True
            full = os.path.join(dirpath, name)
            if _denied_name(name) or os.path.islink(full):
                continue
            # A Windows junction is not a link to `islink`, but it resolves
            # elsewhere, so every deny check runs on the resolved dir.
            real_dir = os.path.realpath(full)
            if not _within(root, real_dir):
                continue
            if _in_deny_dirs(deny_dirs, real_dir) or _denied_rel(_rel(root, real_dir)):
                continue
            kept.append(name)
        dirnames[:] = kept
        for name in sorted(filenames):
            seen += 1
            if seen > MAX_SCAN_ENTRIES:
                return files, True
            if _denied_name(name):
                continue
            full = os.path.join(dirpath, name)
            real = os.path.realpath(full)
            if not _within(root, real) or not os.path.isfile(real):
                continue
            if _in_deny_dirs(deny_dirs, real) or _denied_rel(_rel(root, real)):
                continue
            # The resolved-target check above is the one that matters; the
            # link's own name was already checked and its parents were pruned.
            rel = _rel(root, full)
            files.append((real, rel))
    return files, False


def _candidates(root: str, deny_dirs, path: Optional[str]):
    real, rel = _resolve(root, deny_dirs, path)
    if os.path.isdir(real):
        return _scan(root, deny_dirs, real)
    if not os.path.isfile(real):
        raise _Refused(f"not a regular file: {rel}")
    return [(real, rel)], False


def _is_int(value) -> bool:
    return isinstance(value, int) and not isinstance(value, bool)


def _budget(now: Callable[[], float] = time.monotonic) -> Callable[[], None]:
    """A check that raises `_BudgetExhausted` once `SEARCH_BUDGET_SECONDS`
    have passed since it was created. Best-effort only: it runs between
    lines, and a single `re.search` cannot be interrupted, so Coyote kills
    the tool process after its (capped) tool timeout as the real bound."""
    deadline = now() + SEARCH_BUDGET_SECONDS

    def check() -> None:
        if now() > deadline:
            raise _BudgetExhausted()

    return check


def _internal_error(err: Exception) -> dict:
    return {"error": f"internal error: {type(err).__name__}"}


def fs_read(path: str, offset: Optional[int] = None, limit: Optional[int] = None) -> dict:
    """Read a UTF-8 text file under the root and return numbered lines.

    Reads at most 262144 bytes of the file; lines longer than 2000 characters
    are cut and marked, and `lines_clipped` counts them. Binary files are
    refused.

    Args:
        path: File path relative to the root.
        offset: 1-indexed number of the first line to return (default 1).
        limit: Maximum number of lines to return (default 2000).
    """
    try:
        return _fs_read(path, offset, limit)
    except _Refused as err:
        return {"error": str(err)}
    except Exception as err:
        return _internal_error(err)


def _fs_read(path, offset, limit) -> dict:
    root = _root()
    if root is None:
        return _NO_ROOT
    offset = 1 if offset is None else offset
    limit = DEFAULT_READ_LINES if limit is None else limit
    if not _is_int(offset) or not _is_int(limit):
        return {"error": "offset and limit must be integers"}
    if offset < 1 or limit < 1:
        return {"error": "offset and limit must be positive"}
    real, rel = _resolve(root, _deny_dirs(), path)
    if os.path.isdir(real):
        return {"error": f"is a directory: {rel}"}
    if not os.path.isfile(real):
        return {"error": f"not a regular file: {rel}"}
    try:
        read = _read_capped(real)
    except OSError:
        return {"error": f"unreadable: {rel}"}
    if read is None:
        return {"error": f"binary file refused: {rel}"}
    text, cut = read
    lines = _lines(text)
    window = lines[offset - 1 : offset - 1 + limit]
    content = "\n".join(
        f"{number}: {_clip(line)}" for number, line in enumerate(window, start=offset)
    )
    return {
        "path": rel,
        "offset": offset,
        "lines": len(window),
        "total_lines_read": len(lines),
        "truncated": cut or offset - 1 + len(window) < len(lines),
        "lines_clipped": sum(1 for line in window if len(line) > MAX_LINE_CHARS),
        "content": content,
    }


def fs_grep(pattern: str, path: Optional[str] = None, include: Optional[str] = None) -> dict:
    """Search text files under the root for a Python regular expression.

    Returns at most 200 matches; `.git`, `.env*`, credential stores, binary
    files and symlinks leaving the root are never searched. Each file is read
    up to 262144 bytes and each line is matched against its first 2000
    characters; the pattern and include glob are at most 256 characters.
    A walk over more than 20000 directory entries stops early with
    `scan_truncated` set. The search stops between lines after about 5
    seconds with `budget_exhausted` set, and Coyote kills it outright at its
    tool timeout.

    Args:
        pattern: Python `re` pattern matched against each line.
        path: File or directory relative to the root to search (default: the root).
        include: fnmatch glob matched against file basenames, e.g. `*.rs`; braces are literal, so `*.{rs,txt}` matches nothing.
    """
    try:
        return _fs_grep(pattern, path, include)
    except _Refused as err:
        return {"error": str(err)}
    except Exception as err:
        return _internal_error(err)


def _fs_grep(pattern, path, include) -> dict:
    root = _root()
    if root is None:
        return _NO_ROOT
    if not isinstance(pattern, str):
        return {"error": "pattern must be a string"}
    if include is not None and not isinstance(include, str):
        return {"error": "include must be a string"}
    if len(pattern) > MAX_PATTERN_CHARS:
        return {"error": f"pattern longer than {MAX_PATTERN_CHARS} characters"}
    if include is not None and len(include) > MAX_PATTERN_CHARS:
        return {"error": f"include longer than {MAX_PATTERN_CHARS} characters"}
    try:
        regex = re.compile(pattern)
    except re.error as err:
        return {"error": f"invalid pattern: {err}"}
    candidates, scan_truncated = _candidates(root, _deny_dirs(), path)
    check_budget = _budget()
    matches = []
    truncated = False
    budget_exhausted = False
    try:
        for real, rel in candidates:
            check_budget()
            if include and not fnmatch.fnmatchcase(os.path.basename(rel), include):
                continue
            try:
                read = _read_capped(real)
            except OSError:
                continue
            if read is None:
                continue
            for number, line in enumerate(_lines(read[0]), start=1):
                if number % BUDGET_CHECK_EVERY == 0:
                    check_budget()
                if not regex.search(line[:MAX_LINE_CHARS]):
                    continue
                if len(matches) >= MAX_GREP_MATCHES:
                    truncated = True
                    break
                matches.append({"path": rel, "line": number, "text": _clip(line)})
            if truncated:
                break
    except _BudgetExhausted:
        budget_exhausted = True
        truncated = True
    return {
        "matches": matches,
        "truncated": truncated,
        "scan_truncated": scan_truncated,
        "budget_exhausted": budget_exhausted,
    }


def fs_glob(pattern: str, path: Optional[str] = None) -> dict:
    """List files under the root whose root-relative path matches a glob.

    This is `fnmatch`, not gitignore: `*` crosses `/`, so `*.rs` matches at
    any depth and `src/*.rs` lists every `.rs` under `src` at any depth,
    while `src/**/*.rs` does NOT match `src/main.rs`. Returns at most 500
    sorted paths; `.git`, `.env*`, credential stores and symlinked
    directories are never listed. A leading `./` on the pattern is ignored.
    A walk over more than 20000 directory entries stops early with
    `scan_truncated` set; matching stops after about 5 seconds with
    `budget_exhausted` set.

    Args:
        pattern: Glob matched against each file's root-relative POSIX path.
        path: Directory relative to the root to list (default: the root).
    """
    try:
        return _fs_glob(pattern, path)
    except _Refused as err:
        return {"error": str(err)}
    except Exception as err:
        return _internal_error(err)


def _fs_glob(pattern, path) -> dict:
    root = _root()
    if root is None:
        return _NO_ROOT
    if not isinstance(pattern, str):
        return {"error": "pattern must be a string"}
    if len(pattern) > MAX_PATTERN_CHARS:
        return {"error": f"pattern longer than {MAX_PATTERN_CHARS} characters"}
    pattern = re.sub(r"^(?:\./)+", "", pattern)
    candidates, scan_truncated = _candidates(root, _deny_dirs(), path)
    check_budget = _budget()
    paths = []
    truncated = False
    budget_exhausted = False
    try:
        for index, (_, rel) in enumerate(candidates, start=1):
            if index % BUDGET_CHECK_EVERY == 0:
                check_budget()
            if not fnmatch.fnmatchcase(rel, pattern):
                continue
            if len(paths) >= MAX_GLOB_RESULTS:
                truncated = True
                break
            paths.append(rel)
    except _BudgetExhausted:
        budget_exhausted = True
        truncated = True
    paths.sort()
    return {
        "paths": paths,
        "truncated": truncated,
        "scan_truncated": scan_truncated,
        "budget_exhausted": budget_exhausted,
    }
