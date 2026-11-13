// TypeScript helper for prompting the user from a tool, the counterpart of
// prompt-utils.sh. Load it by path from the env var the shim exports, going
// through a file URL because a bare absolute path is not a valid import
// specifier everywhere (Node rejects `C:\...` on Windows):
//
//   import { pathToFileURL } from "node:url";
//   const prompt = await import(pathToFileURL(process.env.LLM_PROMPT_UTILS_TS!).href);
//
// The env var is exported by the global tool shims (run-tool.py / run-tool.ts)
// and by graph script nodes; the agent-scoped shims (run-agent.py /
// run-agent.ts) do not export it.
//
// Under plain `node`, a package.json without a "type" field above this file
// makes node print a MODULE_TYPELESS_PACKAGE_JSON warning on stderr the first
// time it loads.
//
// Helpers: confirm, select, checkbox, input, password, guardOperation,
// guardPath (also exported as guard_operation and guard_path). confirm
// returns a boolean, select the index of the chosen option, checkbox the
// chosen indices in option order, input and password the text. All of them
// are synchronous. Through the prompt channel a select may instead return
// the free text the user typed (a string) when they answered with one of
// their own.
//
// When Coyote offers a prompt channel (COYOTE_PROMPT_REPLY is set) a helper
// writes one request line to the tool's stdout and reads the answer line back
// from the channel. The request line looks like this, with the marker broken
// here on purpose because the host treats any line carrying the intact marker
// as a request:
//
//   {"coyote_ prompt": {"id": "p123-0f1e...", "action": "confirm", "question": "Go?", "options": []}}
//
// Return convention: every helper returns null when the prompt was cancelled,
// the channel closed, or the reply did not fit the action, so callers fail
// closed. The latter two also print a note to stderr.
//
// _COYOTE_PROMPT_OUT: a bash tool that sourced prompt-utils.sh saves its real
// stdout on a spare fd and exports its number here, so a TypeScript tool it
// runs writes requests to that fd instead of its own (captured) stdout.
//
// The request goes out through fs.writeSync: process.stdout.write is
// asynchronous on pipes on some platforms (macOS), so the blocking read that
// follows would wait for a reply to a request that never left the process.
// The reply is read one byte at a time with fs.readSync, never readFileSync:
// the host keeps the write end of the channel open, so waiting for EOF would
// wait forever.
//
// The channel answers one prompt at a time: never run two helpers
// concurrently in the same tool, or they will swallow each other's replies.
//
// Without a channel the helpers prompt on the controlling terminal
// (/dev/tty, else stderr for text and stdin for input); password turns echo
// off for its read when stty can reach that terminal.
//
// COYOTE_UI is "repl" or "tui" when a human can answer and "headless" when
// nobody can; tools may branch on it before asking. Under "tui" only the
// channel-backed path is safe: the /dev/tty fallback draws straight over the
// full-screen UI, so under "tui" every helper without a channel
// (COYOTE_PROMPT_REPLY unset) fails closed instead, exit 1 with one line on
// stderr; guard_operation and guard_path then abort the operation they guard.

import * as fs from "node:fs";
import * as path from "node:path";
import { execFileSync } from "node:child_process";
import { randomBytes } from "node:crypto";

const REPLY_ENV = "COYOTE_PROMPT_REPLY";
const OUT_FD_ENV = "_COYOTE_PROMPT_OUT";

type Action = "confirm" | "select" | "checkbox" | "input" | "password";
type Answer = boolean | number | number[] | string | null;

function channelEnabled(): boolean {
  const reply = process.env[REPLY_ENV] ?? "";
  // The bash helper parks its stdout on fd 9, so a channel there is treated
  // as absent by all three helpers alike.
  return reply !== "" && reply !== "/dev/fd/9";
}

function sleep(ms: number): void {
  Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, ms);
}

function writeAll(fd: number, text: string): void {
  let data = Buffer.from(text, "utf-8");
  while (data.length > 0) {
    try {
      data = data.subarray(fs.writeSync(fd, data));
    } catch (err) {
      if ((err as { code?: string }).code === "EAGAIN") {
        sleep(10);
        continue;
      }
      throw err;
    }
  }
}

function showError(message: string): void {
  writeAll(2, `${message}\n`);
}

function sendRequest(line: string): void {
  const outFd = process.env[OUT_FD_ENV] ?? "";
  writeAll(/^\d+$/.test(outFd) ? Number(outFd) : 1, line);
}

// One line from `fd`, without its newline; null at end of file.
function readLine(fd: number): string | null {
  const bytes: number[] = [];
  const byte = Buffer.alloc(1);
  for (;;) {
    let count: number;
    try {
      count = fs.readSync(fd, byte, 0, 1, null);
    } catch (err) {
      if ((err as { code?: string }).code === "EAGAIN") {
        sleep(10);
        continue;
      }
      throw err;
    }
    if (count === 0) {
      return bytes.length === 0 ? null : Buffer.from(bytes).toString("utf-8");
    }
    if (byte[0] === 0x0a) {
      return Buffer.from(bytes).toString("utf-8");
    }
    bytes.push(byte[0]);
  }
}

function mapAnswer(action: Action, answer: unknown, options: string[]): Answer {
  switch (action) {
    case "confirm":
      return typeof answer === "boolean" ? answer : null;
    case "select":
      if (typeof answer !== "string") {
        return null;
      }
      return options.includes(answer) ? options.indexOf(answer) : answer;
    case "checkbox": {
      if (!Array.isArray(answer) || !answer.every((item) => options.includes(item))) {
        return null;
      }
      const chosen: number[] = [];
      options.forEach((option, index) => {
        if (answer.includes(option)) {
          chosen.push(index);
        }
      });
      return chosen;
    }
    default:
      return typeof answer === "string" ? answer : null;
  }
}

function promptChannel(action: Action, question: string, options: string[]): Answer {
  const id = `p${process.pid}-${randomBytes(8).toString("hex")}`;
  const request = { id, action, question, options };

  // The reply side is opened before the request goes out, so a channel that
  // cannot be read never receives a request nobody will answer.
  let fd = -1;
  try {
    fd = fs.openSync(process.env[REPLY_ENV]!, "r");
    sendRequest(`${JSON.stringify({ coyote_prompt: request })}\n`);
  } catch {
    if (fd >= 0) {
      fs.closeSync(fd);
    }
    showError("prompt request channel unavailable");
    return null;
  }

  let reply: Record<string, unknown> | undefined;
  try {
    for (;;) {
      let line: string | null;
      try {
        line = readLine(fd);
      } catch {
        showError("prompt reply channel closed");
        return null;
      }
      if (line === null) {
        showError("prompt reply channel closed");
        return null;
      }
      let parsed: unknown;
      try {
        parsed = JSON.parse(line);
      } catch {
        continue;
      }
      if (parsed !== null && typeof parsed === "object" && (parsed as Record<string, unknown>).id === id) {
        reply = parsed as Record<string, unknown>;
        break;
      }
    }
  } finally {
    fs.closeSync(fd);
  }

  const answer = reply.answer;
  if (answer === null || answer === undefined) {
    return null;
  }
  const mapped = mapAnswer(action, answer, options);
  if (mapped === null) {
    showError("prompt reply unusable");
  }
  return mapped;
}

const TUI_NO_CHANNEL = "prompts are not available in the TUI without the reply channel";

// Under the full-screen UI the terminal belongs to the UI's own reader: a
// fallback prompt would be invisible and a typed answer (a password included)
// would land in its composer. Without the channel the helper fails closed.
function refuseTerminalUnderTui(): void {
  if (process.env.COYOTE_UI === "tui") {
    showError(TUI_NO_CHANNEL);
    process.exit(1);
  }
}

// Where terminal prompts are written and read: /dev/tty when it opens, else
// stderr and stdin.
interface Terminal {
  input: number;
  output: number;
  close(): void;
}

function openTerminal(): Terminal {
  try {
    const fd = fs.openSync("/dev/tty", "r+");
    return { input: fd, output: fd, close: () => fs.closeSync(fd) };
  } catch {
    return { input: 0, output: 2, close: () => {} };
  }
}

function stripCr(line: string | null): string | null {
  return line?.replace(/\r$/, "") ?? null;
}

// Question and option text reaches the terminal unescaped in fallback mode,
// so control and bidi-override characters (which could repaint or reorder
// the prompt) are dropped from each piece before it is formatted.
function stripControl(text: string): string {
  return text.replace(/[\x00-\x1f\x7f\u0080-\u009f\u202a-\u202e\u2066-\u2069]/g, "");
}

function stty(tty: Terminal, mode: string): boolean {
  try {
    execFileSync("stty", [mode], { stdio: [tty.input, "ignore", "ignore"] });
    return true;
  } catch {
    return false;
  }
}

function snapshotTty(tty: Terminal): string | null {
  try {
    return execFileSync("stty", ["-g"], { stdio: [tty.input, "pipe", "ignore"], encoding: "utf8" }).trim();
  } catch {
    return null;
  }
}

const ECHO_UNAVAILABLE = Symbol("echo unavailable");

// The hidden read belongs to a short-lived sh child, because a blocking
// fs.readSync never lets a SIGINT handler run, so Ctrl-C would be swallowed
// with echo still off. The child installs its trap before any mode change,
// then turns echo off and prints the prompt itself, so there is no window in
// which a keystroke can be echoed or a signal can leave the terminal
// unrestored. The parent only holds the snapshot taken beforehand and puts it
// back once the child returns; the child's trap restores the same snapshot
// when a signal kills it first, since SIGINT reaches both and node dies
// inside execFileSync without running its finally. The prompt travels on the
// child's stderr, which is bound to the terminal output. A failed stty exits
// 2 so the caller can fall back to an echoed read with a warning; end of
// input before a line exits 1 and surfaces as null, the same cancel every
// other helper reports.
function readHidden(tty: Terminal, saved: string, prompt: string): string | null | typeof ECHO_UNAVAILABLE {
  try {
    const line = execFileSync(
      "sh",
      [
        "-c",
        "trap 'stty \"$COYOTE_TTY_SAVED\"' EXIT INT TERM; stty -echo || exit 2; " +
          'printf %s "$COYOTE_TTY_PROMPT" >&2; IFS= read -r l || exit 1; printf %s "$l"',
      ],
      {
        stdio: [tty.input, "pipe", tty.output],
        encoding: "utf8",
        env: { ...process.env, COYOTE_TTY_SAVED: saved, COYOTE_TTY_PROMPT: prompt },
      },
    );
    return stripCr(line);
  } catch (err) {
    return (err as { status?: number | null }).status === 2 ? ECHO_UNAVAILABLE : null;
  }
}

function ttyLine(prompt: string, optionLines: string[] = [], hidden = false): string | null {
  refuseTerminalUnderTui();
  const tty = openTerminal();
  try {
    if (!hidden) {
      writeAll(tty.output, optionLines.join("") + prompt);
      return stripCr(readLine(tty.input));
    }
    const saved = snapshotTty(tty);
    if (saved === null) {
      writeAll(tty.output, "Warning: Password input may be echoed.\n" + prompt);
      return stripCr(readLine(tty.input));
    }
    let hiddenReadRan = false;
    try {
      const result = readHidden(tty, saved, prompt);
      if (result === ECHO_UNAVAILABLE) {
        writeAll(tty.output, "Warning: Password input may be echoed.\n" + prompt);
        return stripCr(readLine(tty.input));
      }
      hiddenReadRan = true;
      return result;
    } finally {
      stty(tty, saved);
      if (hiddenReadRan) {
        writeAll(tty.output, "\n");
      }
    }
  } finally {
    tty.close();
  }
}

function ttyList(question: string, options: string[]): string | null {
  const optionLines = options.map((option, index) => `  ${index + 1}) ${stripControl(option)}\n`);
  return ttyLine(`? ${stripControl(question)} `, optionLines);
}

function parseIndex(text: string, options: string[]): number | null {
  if (!/^\d+$/.test(text)) {
    return null;
  }
  const index = Number(text) - 1;
  return index >= 0 && index < options.length ? index : null;
}

export function confirm(question: string): boolean | null {
  if (channelEnabled()) {
    return promptChannel("confirm", question, []) as boolean | null;
  }
  const line = ttyLine(`? ${stripControl(question)} (y/N) `);
  if (line === null) {
    return null;
  }
  return line.trim().toLowerCase().startsWith("y");
}

export function select(question: string, options: string[]): number | string | null {
  options = options.map(String);
  if (channelEnabled()) {
    return promptChannel("select", question, options) as number | string | null;
  }
  const line = ttyList(question, options);
  return line === null ? null : parseIndex(line.trim(), options);
}

export function checkbox(question: string, options: string[]): number[] | null {
  options = options.map(String);
  if (channelEnabled()) {
    return promptChannel("checkbox", question, options) as number[] | null;
  }
  const line = ttyList(question, options);
  if (line === null) {
    return null;
  }
  const chosen = new Set<number>();
  for (const word of line.split(/\s+/).filter(Boolean)) {
    const index = parseIndex(word, options);
    if (index === null) {
      return null;
    }
    chosen.add(index);
  }
  return [...chosen].sort((a, b) => a - b);
}

export function input(question: string): string | null {
  if (channelEnabled()) {
    return promptChannel("input", question, []) as string | null;
  }
  return ttyLine(`? ${stripControl(question)} `);
}

export function password(question: string): string | null {
  if (channelEnabled()) {
    return promptChannel("password", question, []) as string | null;
  }
  return ttyLine(`? ${stripControl(question)} `, [], true);
}

function autoConfirmed(): boolean {
  return Boolean(process.env["AUTO_CONFIRM"] || process.env["LLM_AGENT_VAR_AUTO_CONFIRM"]);
}

function abortUnlessConfirmed(question: string): void {
  if (confirm(question) !== true) {
    showError("ERROR: Operation aborted!");
    process.exit(1);
  }
}

export function guardOperation(question = "Are you sure you want to continue?"): void {
  if (autoConfirmed()) {
    return;
  }
  abortUnlessConfirmed(question);
}

export function guardPath(target: string, question: string): void {
  if (autoConfirmed()) {
    return;
  }
  const cwd = realpathIfExists(process.cwd());
  const resolved = resolveComponents(target, cwd);
  if (resolved === cwd || resolved.startsWith(cwd.replace(/[\\/]+$/, "") + path.sep)) {
    return;
  }
  abortUnlessConfirmed(question);
}

function realpathIfExists(target: string): string {
  try {
    return fs.realpathSync(target);
  } catch {
    return target;
  }
}

function readLinkTarget(candidate: string): string | null {
  try {
    return fs.lstatSync(candidate).isSymbolicLink() ? fs.readlinkSync(candidate) : null;
  } catch {
    return null;
  }
}

// Past this many hops the remaining link is kept as a plain path; the kernel
// stops following at the same depth (MAXSYMLINKS), so open(2) fails there too.
const MAX_SYMLINK_HOPS = 40;

// Like os.path.realpath: symlinks are followed component by component, so
// `..` steps out of the directory a link points at, not the link's parent
// (path.resolve would collapse `link/../x` lexically to a path inside the
// working directory while the kernel writes outside it). Components that
// do not exist yet are appended unchanged, so a new file under a link that
// points outside still resolves to where it will land. A dangling link is
// followed by its text for the same reason: open(2) creates the file where
// the link points, not where the link sits.
function resolveComponents(target: string, cwd: string): string {
  const separators = process.platform === "win32" ? /[\\/]+/ : /\/+/;
  const root = path.parse(target).root;
  let head = root === "" ? cwd : realpathIfExists(path.resolve(root));
  const pending = target.slice(root.length).split(separators);
  let hops = 0;
  while (pending.length > 0) {
    const segment = pending.shift() as string;
    if (segment === "" || segment === ".") {
      continue;
    }
    if (segment === "..") {
      head = path.dirname(head);
      continue;
    }
    const candidate = path.join(head, segment);
    const real = realpathIfExists(candidate);
    const link = real === candidate ? readLinkTarget(candidate) : null;
    if (link === null || hops >= MAX_SYMLINK_HOPS) {
      head = real;
      continue;
    }
    hops += 1;
    const linkRoot = path.parse(link).root;
    head = linkRoot === "" ? path.dirname(candidate) : realpathIfExists(path.resolve(linkRoot));
    pending.unshift(...link.slice(linkRoot.length).split(separators));
  }
  return head;
}

export { guardOperation as guard_operation, guardPath as guard_path };
