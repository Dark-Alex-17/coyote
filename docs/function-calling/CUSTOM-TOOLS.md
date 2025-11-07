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
<!--toc:end-->

---

## Supported Languages
Loki supports custom tools written in the following programming languages:

* Python
* Bash

## Creating a Custom Tool
All tools are created as scripts in either Python or Bash. They should be placed in the `functions/tools` directory.
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
    array_optional: Optional[List[str]] = None,
):
    """Demonstrates how to create a tool using Python and how to use comments.
    Args:
        string: Define a required string property
        string_enum: Define a required string property with enum
        boolean: Define a required boolean property
        integer: Define a required integer property
        number: Define a required number property
        array: Define a required string array property
        string_optional: Define an optional string property
        array_optional: Define an optional string array property
    """
    output = f"""string: {string}
string_enum: {string_enum}
string_optional: {string_optional}
boolean: {boolean}
integer: {integer}
number: {number}
array: {array}
array_optional: {array_optional}"""

    for key, value in os.environ.items():
        if key.startswith("LLM_"):
            output = f"{output}\n{key}: {value}"

    return output
```
