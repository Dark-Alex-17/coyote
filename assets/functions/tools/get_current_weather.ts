#!/usr/bin/env tsx

import { appendFileSync, mkdirSync } from "fs";
import { dirname } from "path";

/**
 * Get the current weather in a given location
 * @param location - The city and optionally the state or country (e.g., "London", "San Francisco, CA").
 */
export async function run(location: string): string {
  const encoded = encodeURIComponent(location);
  const url = `https://wttr.in/${encoded}?format=4`;

  const resp = await fetch(url);
  const data = await resp.text();

  const dest = process.env["LLM_OUTPUT"] ?? "/dev/stdout";
  if (dest !== "-" && dest !== "/dev/stdout") {
    mkdirSync(dirname(dest), { recursive: true });
    appendFileSync(dest, data, "utf-8");
  }

  return data;
}
