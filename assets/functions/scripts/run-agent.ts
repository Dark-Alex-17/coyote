#!/usr/bin/env tsx

// Usage: ./{agent_name}.ts <agent-func> <agent-data>

import { readFileSync, writeFileSync, existsSync } from "fs";
import { join } from "path";
import { pathToFileURL } from "url";

async function main(): Promise<void> {
  const { agentFunc, rawData } = parseArgv();
  const agentData = parseRawData(rawData);

  const configDir = "{config_dir}";
  setupEnv(configDir, agentFunc);

  const agentToolsPath = join(configDir, "agents", "{agent_name}", "tools.ts");
  await run(agentToolsPath, agentFunc, agentData);
}

function parseRawData(data: string): Record<string, unknown> {
  if (!data) {
    throw new Error("No JSON data");
  }

  try {
    return JSON.parse(data);
  } catch {
    throw new Error("Invalid JSON data");
  }
}

function parseArgv(): { agentFunc: string; rawData: string } {
  const agentFunc = process.argv[2];

  const toolDataFile = process.env["LLM_TOOL_DATA_FILE"];
  let agentData: string;
  if (toolDataFile && existsSync(toolDataFile)) {
    agentData = readFileSync(toolDataFile, "utf-8");
  } else {
    agentData = process.argv[3];
  }

  if (!agentFunc || !agentData) {
    process.stderr.write("Usage: ./{agent_name}.ts <agent-func> <agent-data>\n");
    process.exit(1);
  }

  return { agentFunc, rawData: agentData };
}

function setupEnv(configDir: string, agentFunc: string): void {
  loadEnv(join(configDir, ".env"));
  process.env["LLM_ROOT_DIR"] = configDir;
  process.env["LLM_AGENT_NAME"] = "{agent_name}";
  process.env["LLM_AGENT_FUNC"] = agentFunc;
  process.env["LLM_AGENT_ROOT_DIR"] = join(configDir, "agents", "{agent_name}");
  process.env["LLM_AGENT_CACHE_DIR"] = join(configDir, "cache", "{agent_name}");
}

function loadEnv(filePath: string): void {
  let lines: string[];
  try {
    lines = readFileSync(filePath, "utf-8").split("\n");
  } catch {
    return;
  }

  for (const raw of lines) {
    const line = raw.trim();
    if (line.startsWith("#") || !line) {
      continue;
    }

    const eqIdx = line.indexOf("=");
    if (eqIdx === -1) {
      continue;
    }

    const key = line.slice(0, eqIdx).trim();
    if (key in process.env) {
      continue;
    }

    let value = line.slice(eqIdx + 1).trim();
    if (
      (value.startsWith('"') && value.endsWith('"')) ||
      (value.startsWith("'") && value.endsWith("'"))
    ) {
      value = value.slice(1, -1);
    }
    process.env[key] = value;
  }
}

function extractParamNames(fn: Function): string[] {
  const src = fn.toString();
  const match = src.match(/^(?:async\s+)?function\s*\w*\s*\(([^)]*)\)/);
  if (!match) {
    return [];
  }
  return match[1]
    .split(",")
    .map((p) => p.trim().replace(/[:=?].*/s, "").trim())
    .filter(Boolean);
}

function spreadArgs(
  fn: Function,
  data: Record<string, unknown>,
): unknown[] {
  const names = extractParamNames(fn);
  if (names.length === 0) {
    return [];
  }
  return names.map((name) => data[name]);
}

async function run(
  agentPath: string,
  agentFunc: string,
  agentData: Record<string, unknown>,
): Promise<void> {
  const mod = await import(pathToFileURL(agentPath).href);

  if (typeof mod[agentFunc] !== "function") {
    throw new Error(`No module function '${agentFunc}' at '${agentPath}'`);
  }

  const fn = mod[agentFunc] as Function;
  const args = spreadArgs(fn, agentData);
  const value = await fn(...args);
  returnToLlm(value);
  dumpResult(`{agent_name}:${agentFunc}`);
}

function returnToLlm(value: unknown): void {
  if (value === null || value === undefined) {
    return;
  }

  const output = process.env["LLM_OUTPUT"];
  const write = (s: string) => {
    if (output) {
      writeFileSync(output, s, "utf-8");
    } else {
      process.stdout.write(s);
    }
  };

  if (typeof value === "string" || typeof value === "number" || typeof value === "boolean") {
    write(String(value));
  } else if (typeof value === "object") {
    write(JSON.stringify(value, null, 2));
  }
}

function dumpResult(name: string): void {
  const dumpResults = process.env["LLM_DUMP_RESULTS"];
  const llmOutput = process.env["LLM_OUTPUT"];

  if (!dumpResults || !llmOutput || !process.stdout.isTTY) {
    return;
  }

  try {
    const pattern = new RegExp(`\\b(${dumpResults})\\b`);
    if (!pattern.test(name)) {
      return;
    }
  } catch {
    return;
  }

  let data: string;
  try {
    data = readFileSync(llmOutput, "utf-8");
  } catch {
    return;
  }

  process.stdout.write(
    `\x1b[2m----------------------\n${data}\n----------------------\x1b[0m\n`,
  );
}

main().catch((err) => {
  process.stderr.write(`${err}\n`);
  process.exit(1);
});
