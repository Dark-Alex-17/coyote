# Custom Tools
Loki is designed to be as flexible and as customizable as possible. One of the key
features that enables this flexibility is the ability to create and integrate custom tools
into your Loki setup. This document provides a guide on how to create and use custom tools within Loki.

## Quick Links
<!--toc:start-->
- [Supported Languages](#supported-languages)
- [Creating a Custom Tool](#creating-a-custom-tool)
  - [Environment Variables](#environment-variables)
  - [Custom Bash-Based Tools](#custom-bash-based-tools)
  - [Custom Python-Based Tools](#custom-python-based-tools)
  - [Custom TypeScript-Based Tools](#custom-typescript-based-tools)
- [Custom Runtime](#custom-runtime)
<!--toc:end-->

---

## Supported Languages
Loki supports custom tools written in the following programming languages:

* Python
* Bash
* TypeScript

## Creating a Custom Tool
All tools are created as scripts in either Python, Bash, or TypeScript. They should be placed in the `functions/tools` directory.
The location of the `functions` directory varies between systems, so you can use the following command to locate
your `functions` directory:

```shell
loki --info | grep functions_dir | awk '{print $2}'
```

Once you've created your custom tool, remember to add it to the `visible_tools` array in your global `config.yaml` file 
to enable it globally. See the [Tools](TOOLS.md#enablingdisabling-global-tools) documentation for more information on how Loki utilizes the 
`visible_tools` array.

### Environment Variables
All tools have access to the following environment variables that provide context about the current execution environment:

| Variable             | Description                                                                                                                                |
|----------------------|--------------------------------------------------------------------------------------------------------------------------------------------|
| `LLM_OUTPUT`         | Indicates where the output of the tool should go. <br>In certain situations, this may be set to a temporary file instead of `/dev/stdout`. |
| `LLM_ROOT_DIR`       | The root `config_dir` directory for Loki <br>(i.e. `dirname $(loki --info \| grep config_file \| awk '{print $2}')`)                       |
| `LLM_TOOL_NAME`      | The name of the tool being executed                                                                                                        |
| `LLM_TOOL_CACHE_DIR` | A directory specific to the tool for storing cache or temporary files                                                                      |

Loki also searches the tools directory on startup for a `.env` file. If found, all tools in `functions/tools/` will have
the environment variables defined in the `.env` file available to them.

### Custom Bash-Based Tools
To create a Bash-based tool, refer to the [custom bash tools documentation](CUSTOM-BASH-TOOLS.md).

### Custom Python-Based Tools
Loki supports tools written in Python.

Each Python-based tool must follow a specific structure in order for Loki to be able to properly compile and
execute it:

* The tool must be a Python script with a `.py` file extension.
* The tool must have a `def run` function that serves as the entry point for the tool.
* The `run` function must accept parameters that define the inputs for the tool.
  * Always use type hints to specify the data type of each parameter.
  * Use `Optional[...]` to indicate optional parameters
* The `run` function must return a `str`.
  * For Python, this is automatically written to the `LLM_OUTPUT` environment variable, so there's no need to explicitly
    write to the environment variable within the function.
* The function must also have a docstring that describes the tool and its parameters.
  * Each parameter in the `run` function should be documented in the docstring using the `Args:` section. They should use the following format:
    * `<parameter_name>: <description>` Where
      * `<parameter_name>`: The name of the parameter
      * `<description>`: The description of the parameter
  * These are *very* important because these descriptions are what's passed to the LLM as the description of the tool,
    letting the LLM know what the tool does and how to use it.

It's important to note that any functions prefixed with `_` are not sent to the LLM, so they will be invisible to the LLM
at runtime.

Below is the [`demo_py.py`](../../assets/functions/tools/demo_py.py) tool definition that comes pre-packaged with
Loki and demonstrates how to create a Python-based tool:

```python
import os
from typing import List, Literal, Optional


def run(
    string: str,
    string_enum: Literal["foo", "bar"],
    boolean: bool,
    integer: int,
    number: float,
    array: List[str],
    string_optional: Optional[str] = None,
    integer_with_default: int = 42,
    boolean_with_default: bool = True,
    number_with_default: float = 3.14,
    string_with_default: str = "hello",
    array_optional: Optional[List[str]] = None,
):
    """Demonstrates all supported Python parameter types and variations.
    Args:
        string: A required string property
        string_enum: A required string property constrained to specific values
        boolean: A required boolean property
        integer: A required integer property
        number: A required number (float) property
        array: A required string array property
        string_optional: An optional string property (Optional[str] with None default)
        integer_with_default: An optional integer with a non-None default value
        boolean_with_default: An optional boolean with a default value
        number_with_default: An optional number with a default value
        string_with_default: An optional string with a default value
        array_optional: An optional string array property
    """
    output = f"""string: {string}
string_enum: {string_enum}
boolean: {boolean}
integer: {integer}
number: {number}
array: {array}
string_optional: {string_optional}
integer_with_default: {integer_with_default}
boolean_with_default: {boolean_with_default}
number_with_default: {number_with_default}
string_with_default: {string_with_default}
array_optional: {array_optional}"""

    for key, value in os.environ.items():
        if key.startswith("LLM_"):
            output = f"{output}\n{key}: {value}"

    return output
```

### Custom TypeScript-Based Tools
Loki supports tools written in TypeScript. TypeScript tools require [Node.js](https://nodejs.org/) and
[tsx](https://tsx.is/) (`npx tsx` is used as the default runtime).

Each TypeScript-based tool must follow a specific structure in order for Loki to properly compile and execute it:

* The tool must be a TypeScript file with a `.ts` file extension.
* The tool must have an `export function run(...)` that serves as the entry point for the tool.
  * Non-exported functions are ignored by the compiler and can be used as private helpers.
* The `run` function must accept flat parameters that define the inputs for the tool.
  * Always use type annotations to specify the data type of each parameter.
  * Use `param?: type` or `type | null` to indicate optional parameters.
  * Use `param: type = value` for parameters with default values.
* The `run` function must return a `string` (or `Promise<string>` for async functions).
  * For TypeScript, the return value is automatically written to the `LLM_OUTPUT` environment variable, so there's
    no need to explicitly write to the environment variable within the function.
* The function must have a JSDoc comment that describes the tool and its parameters.
  * Each parameter should be documented using `@param name - description` tags.
  * These descriptions are passed to the LLM as the tool description, letting the LLM know what the tool does and
    how to use it.
* Async functions (`export async function run(...)`) are fully supported and handled transparently.

**Supported Parameter Types:**

| TypeScript Type   | JSON Schema                                      | Notes                       |
|-------------------|--------------------------------------------------|-----------------------------|
| `string`          | `{"type": "string"}`                             | Required string             |
| `number`          | `{"type": "number"}`                             | Required number             |
| `boolean`         | `{"type": "boolean"}`                            | Required boolean            |
| `string[]`        | `{"type": "array", "items": {"type": "string"}}` | Array (bracket syntax)      |
| `Array<string>`   | `{"type": "array", "items": {"type": "string"}}` | Array (generic syntax)      |
| `"foo" \| "bar"`  | `{"type": "string", "enum": ["foo", "bar"]}`     | String enum (literal union) |
| `param?: string`  | `{"type": "string"}` (not required)              | Optional via question mark  |
| `string \| null`  | `{"type": "string"}` (not required)              | Optional via null union     |
| `param = "value"` | `{"type": "string"}` (not required)              | Optional via default value  |

**Unsupported Patterns (will produce a compile error):**

* Rest parameters (`...args: string[]`)
* Destructured object parameters (`{ a, b }: { a: string, b: string }`)
* Arrow functions (`const run = (x: string) => ...`)
* Function expressions (`const run = function(x: string) { ... }`)

Only `export function` declarations are recognized. Non-exported functions are invisible to the compiler.

Below is the [`demo_ts.ts`](../../assets/functions/tools/demo_ts.ts) tool definition that comes pre-packaged with
Loki and demonstrates how to create a TypeScript-based tool:

```typescript
/**
 * Demonstrates all supported TypeScript parameter types and variations.
 *
 * @param string - A required string property
 * @param string_enum - A required string property constrained to specific values
 * @param boolean - A required boolean property
 * @param number - A required number property
 * @param array_bracket - A required string array using bracket syntax
 * @param array_generic - A required string array using generic syntax
 * @param string_optional - An optional string using the question mark syntax
 * @param string_nullable - An optional string using the union-with-null syntax
 * @param number_with_default - An optional number with a default value
 * @param boolean_with_default - An optional boolean with a default value
 * @param string_with_default - An optional string with a default value
 * @param array_optional - An optional string array using the question mark syntax
 */
export function run(
  string: string,
  string_enum: "foo" | "bar",
  boolean: boolean,
  number: number,
  array_bracket: string[],
  array_generic: Array<string>,
  string_optional?: string,
  string_nullable: string | null = null,
  number_with_default: number = 42,
  boolean_with_default: boolean = true,
  string_with_default: string = "hello",
  array_optional?: string[],
): string {
  const parts = [
    `string: ${string}`,
    `string_enum: ${string_enum}`,
    `boolean: ${boolean}`,
    `number: ${number}`,
    `array_bracket: ${JSON.stringify(array_bracket)}`,
    `array_generic: ${JSON.stringify(array_generic)}`,
    `string_optional: ${string_optional}`,
    `string_nullable: ${string_nullable}`,
    `number_with_default: ${number_with_default}`,
    `boolean_with_default: ${boolean_with_default}`,
    `string_with_default: ${string_with_default}`,
    `array_optional: ${JSON.stringify(array_optional)}`,
  ];

  for (const [key, value] of Object.entries(process.env)) {
    if (key.startsWith("LLM_")) {
      parts.push(`${key}: ${value}`);
    }
  }

  return parts.join("\n");
}
```

## Custom Runtime
By default, Loki uses the following runtimes to execute tools:

| Language   | Default Runtime | Requirement                    |
|------------|-----------------|--------------------------------|
| Python     | `python`        | Python 3 on `$PATH`            |
| TypeScript | `npx tsx`       | Node.js + tsx (`npm i -g tsx`) |
| Bash       | `bash`          | Bash on `$PATH`                |

You can override the runtime for Python and TypeScript tools using a **shebang line** (`#!`) at the top of your
script. Loki reads the first line of each tool file; if it starts with `#!`, the specified interpreter is used instead
of the default.

**Examples:**

```python
#!/usr/bin/env python3.11
# This Python tool will be executed with python3.11 instead of the default `python`

def run(name: str):
    """Greet someone.
    Args:
        name: The name to greet
    """
    return f"Hello, {name}!"
```

```typescript
#!/usr/bin/env bun
// This TypeScript tool will be executed with Bun instead of the default `npx tsx`

/**
 * Greet someone.
 * @param name - The name to greet
 */
export function run(name: string): string {
  return `Hello, ${name}!`;
}
```

This is useful for pinning a specific Python version, using an alternative TypeScript runtime like
[Bun](https://bun.sh/) or [Deno](https://deno.com/), or working with virtual environments.
