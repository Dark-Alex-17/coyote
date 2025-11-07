# AIChat to Loki Migration Guide
Loki originally started as a fork of AIChat but has since evolved into its own separate project with separate goals.

As a result, there's some changes you'll need to make to your AIChat configuration to be able to use Loki.

Be sure you've followed the [first-time setup steps](../README.md#first-time-setup) so that the Loki configuration 
directory and subdirectories exist and is populated with the built-in defaults.

## Global Configuration File
You should be able to copy/paste your AIChat configuration file into your Loki configuration directory. Since the 
location of the Loki configuration directory varies between systems, you can use the following command to locate your
config directory:

```shell
loki --info | grep 'config_dir' | awk '{print $2}'
```

Then, you'll need to make the following changes:

* `function_calling` -> `function_calling_support`
* `use_tools` -> `enabled_tools`
* `agent_prelude` -> `agent_session`
* `compress_threshold` -> `compression_threshold`
* `summarize_prompt` -> `summarization_prompt`
* `summary_prompt` -> `summary_context_prompt`

## Roles
Locate your `roles` directory using the following command:

```shell
loki --info | grep 'roles_dir' | awk '{print $2}'
```

Update any roles that have `use_tools` to `enabled_tools`.

## Sessions
Locate your `sessions` directory using the following command:

```shell
loki --info | grep 'sessions_dir' | awk '{print $2}'
```

Update the following settings:
* `use_tools` -> `enabled_tools`
* `compress_threshold` -> `compression_threshold`
* `summarize_prompt` -> `summarization_prompt`
* `summary_prompt` -> `summary_context_prompt`

---

# LLM Functions Changes
Probably the most significant difference between AIChat and Loki is how tools are handled. So if you cloned the 
[llm-functions](https://github.com/sigoden/llm-functions) repo, you'll need to make the following changes.

**Note: JavaScript functions are not supported in Loki.**

The following guide assumes you're using the `llm-functions` repository as your base for custom functions, and thus
follows that directory structure.

## Agents
Agents are now all handled in one place: the `agents` directory (`<loki-config-dir>/agents`):

```shell
loki --info | grep 'agents_dir' | awk '{print $2}'
```

And instead of separate `index.yaml` and `config.yaml` files, they're now both in a single `config.yaml` file.

So now for all of your agents, copy all the contents of those directories to the corresponding directory in the Loki 
`agents` directory. Then make the following changes:

* Copy the contents of your `<aichat-config-dir>/functions/agents` directory into `<loki-config-dir/agents`
* Merge `index.yaml` into `config.yaml`
  * If you never created a custom `config.yaml` file, then simply rename `index.yaml` to `config.yaml`
  * If you've defined an `agent_prelude`, rename that field to `agent_session`
* Convert all JavaScript tools to either Python or Bash
* For Bash `tools.sh`: Remove the following line:
  ```bash
  eval "$(argc --argc-eval "$0" "$@")"
  ```
* Any `tools.txt` files you have that define what global functions the agent uses is now replaced by the `global_tools`
  field in the agent's `config.yaml`. So for example: If your `tools.txt` looks like this:
  ```text
  fs_mkdir.sh
  fs_ls.sh
  fs_patch.sh
  fs_cat.sh
  ```
  then you need to add the following to your agent's `config.yaml`:
  ```yaml
  global_tools:
    - fs_mkdir.sh
    - fs_ls.sh
    - fs_patch.sh
    - fs_cat.sh
  ```
* If you have any bash `tools.sh` that depend on the utility scripts in the `llm-functions` repository, they've been 
  replaced by built-in utility scripts. So use the following to replace any matching lines in your `tools.sh` files:
  ```bash
  ##################
  ## Scripts file ##
  ##################
  ROOT_DIR="${LLM_ROOT_DIR:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
  # replace with
  source "$LLM_PROMPT_UTILS_FILE"
  
  #######################
  ## guard_path script ##
  #######################
  "$ROOT_DIR/utils/guard_path.sh"
  # replace with
  guard_path
  
  ############################
  ## guard_operation script ##
  ############################
  "$ROOT_DIR/utils/guard_operation.sh"
  # replace with
  guard_operation
  
  ######################
  ## patch.awk script ##
  ######################
  awk -f "$ROOT_DIR/utils/patch.awk"
  # replace with
  patch_file
  ```
  
When you're done with this migration, you should have the following:

* No more `functions/agents` directory
* No `functions/agents.txt` file (Loki assumes that if the agent directory exists, it is loadable)
* No `<loki-config-dir>/agents/<agent-name>/tools.txt`
* No `<loki-config-dir>/agents/<agent-name>/index.yaml`

## Functions
Loki consolidates much of the `llm-functions` repo functionality into one binary. So this means

* There's no need to have `argc` installed anymore
* No separate repository to manage
* No `tools.txt`
* No `functions.json`
* No `functions/mcp` directory at all
* No `functions/scripts`

Here's how to migrate your functions over to Loki from the `llm-functions` repository.

* Copy your AIChat `<aichat-config-dir>/functions` directory into your Loki config directory
* Delete the following files and directories from your `<loki-config-dir>/functions` directory:
  * `scripts/`
  * `agents.txt`
  * `functions.json`
  * `Argcfile.sh`
  * `README.md` (irrelevant now)
  * `LICENSE` (irrelevant now)
  * `utils/guard_operation.sh`
  * `utils/guard_path.sh`
  * `utils/patch.awk`
* Everything in `tools.txt` now lives in the global config file under the `visible_tools` setting:
  ```text
  get_current_weather.sh
  execute_command.sh
  web_search.sh
  #execute_py_code.py
  query_jira_issues.sh
  ```
  becomes the following in your `<loki-config-dir>/config.yaml`
  ```yaml
  visible_tools:
    - get_current_weather.sh
    - execute_command.sh
    - web_search.sh
    # - web_search.sh
    - query_jira_issues.sh
  ```
* If you've defined a `functions/mcp.json` file, you can leave it alone.
* Similarly to agents, if you have any bash `tools.sh` that depend on the utility scripts in the `llm-functions` 
  repository, they've been replaced by built-in utility scripts. So use the following to replace any matching lines in 
  your `tools.sh` files:
  ```bash
  ##################
  ## Scripts file ##
  ##################
  ROOT_DIR="${LLM_ROOT_DIR:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
  # replace with
  source "$LLM_PROMPT_UTILS_FILE"
  
  #######################
  ## guard_path script ##
  #######################
  "$ROOT_DIR/utils/guard_path.sh"
  # replace with
  guard_path
  
  ############################
  ## guard_operation script ##
  ############################
  "$ROOT_DIR/utils/guard_operation.sh"
  # replace with
  guard_operation
  
  ######################
  ## patch.awk script ##
  ######################
  awk -f "$ROOT_DIR/utils/patch.awk"
  # replace with
  patch_file
  ```
  
Refer to the [custom bash tools docs](./function-calling/CUSTOM-BASH-TOOLS.md) to learn how to compile and test bash 
tools in Loki without needing to use `argc`.