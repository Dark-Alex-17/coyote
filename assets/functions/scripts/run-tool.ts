#!/usr/bin/env tsx

// Usage: ./{function_name}.ts <tool-data>

import { readFileSync, writeFileSync, existsSync, statSync } from "fs";
import { join, basename } from "path";
import { pathToFileURL } from "url";

async function main(): Promise<void> {
  const rawData = parseArgv();
  const toolData = parseRawData(rawData);

  const rootDir = "{root_dir}";
  setupEnv(rootDir);

  const toolPath = "{tool_path}.ts";
  await run(toolPath, "run", toolData);
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

function parseArgv(): string {
  const toolDataFile = process.env["LLM_TOOL_DATA_FILE"];
  if (toolDataFile && existsSync(toolDataFile)) {
    return readFileSync(toolDataFile, "utf-8");
  }

  const toolData = process.argv[2];

  if (!toolData) {
    process.stderr.write("Usage: ./{function_name}.ts <tool-data>\n");
    process.exit(1);
  }

  return toolData;
}

function setupEnv(rootDir: string): void {
  loadEnv(join(rootDir, ".env"));
  process.env["LLM_ROOT_DIR"] = rootDir;
  process.env["LLM_TOOL_NAME"] = "{function_name}";
  process.env["LLM_TOOL_CACHE_DIR"] = join(rootDir, "cache", "{function_name}");
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

async function run(
  toolPath: string,
  toolFunc: string,
  toolData: Record<string, unknown>,
): Promise<void> {
  const mod = await import(pathToFileURL(toolPath).href);

  if (typeof mod[toolFunc] !== "function") {
    throw new Error(`No module function '${toolFunc}' at '${toolPath}'`);
  }

  const value = await mod[toolFunc](toolData);
  returnToLlm(value);
  dumpResult("{function_name}");
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
