# Tools
Loki supports function calling with various tools built-in to enhance LLM capabilities. All built-in tools for Loki
are located in the [`functions/tools`](../../assets/functions/tools) directory. These tools are also stored in your Loki `functions`
directory, which is also where you'd go to add more tools.

**Pro Tip:** The Loki functions directory can be found by running the following command:
```bash
loki --info | grep functions_dir | awk '{print $2}'
```

# Quick Links
<!--toc:start-->
- [Built-In Tools](#built-in-tools)
- [Configuration](#configuration)
  - [Global Configuration](#global-configuration)
  - [Enabling/Disabling Global Tools](#enablingdisabling-global-tools)
  - [Role Configuration](#role-configuration)
  - [Agent Configuration](#agent-configuration)
- [Tool Error Handling](#tool-error-handling)
  - [Native/Shell Tool Errors](#nativeshell-tool-errors)
  - [MCP Errors](#mcp-tool-errors)
  - [Why Tool Error Handling Is Important](#why-this-matters)
<!--toc:end-->

---

## Built-In Tools
The following tools are built-in to Loki by default, and their default enabled/disabled status is indicated. More about how tools can
be enabled/disabled can be found in the [Configuration](#configuration) section below.

| Tool                                                                                | Description                                                                                                                                                                                                                          | Enabled/Disabled |
|-------------------------------------------------------------------------------------|--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|------------------|
| [`demo_py.py`](../../assets/functions/tools/demo_py.py)                             | Demonstrates how to create a tool using Python and how to use comments.                                                                                                                                                              | 🔴               |
| [`demo_sh.sh`](../../assets/functions/tools/demo_sh.sh)                             | Demonstrate how to create a tool using Bash and how to use comment tags.                                                                                                                                                             | 🔴               |
| [`execute_command.sh`](../../assets/functions/tools/execute_command.sh)             | Execute the shell command.                                                                                                                                                                                                           | 🟢               |
| [`execute_py_code.py`](../../assets/functions/tools/execute_py_code.py)             | Execute the given Python code.                                                                                                                                                                                                       | 🔴               |
| [`execute_sql_code.sh`](../../assets/functions/tools/execute_sql_code.sh)           | Execute SQL code.                                                                                                                                                                                                                    | 🔴               |
| [`fetch_url_via_curl.sh`](../../assets/functions/tools/fetch_url_via_curl.sh)       | Extract the content from a given URL using cURL.                                                                                                                                                                                     | 🔴               |
| [`fetch_url_via_jina.sh`](../../assets/functions/tools/fetch_url_via_jina.sh)       | Extract the content from a given URL using Jina.                                                                                                                                                                                     | 🔴               |
| [`fs_cat.sh`](../../assets/functions/tools/fs_cat.sh)                               | Read the contents of a file at the specified path.                                                                                                                                                                                   | 🟢               |
| [`fs_read.sh`](../../assets/functions/tools/fs_read.sh)                             | Controlled reading of the contents of a file at the specified path  with line numbers, offset, and limit to read specific sections.                                                                                                  | 🟢               |
| [`fs_glob.sh`](../../assets/functions/tools/fs_glob.sh)                             | Find files by glob pattern. Returns matching file paths sorted by modification time.                                                                                                                                                 | 🟢               |
| [`fs_grep.sh`](../../assets/functions/tools/fs_grep.sh)                             | Search file contents using regular expressions. Returns matching file paths and lines.                                                                                                                                               | 🟢               |
| [`fs_ls.sh`](../../assets/functions/tools/fs_ls.sh)                                 | List all files and directories at the specified path.                                                                                                                                                                                | 🟢               |
| [`fs_mkdir.sh`](../../assets/functions/tools/fs_mkdir.sh)                           | Create a new directory at the specified path.                                                                                                                                                                                        | 🔴               |
| [`fs_patch.sh`](../../assets/functions/tools/fs_patch.sh)                           | Apply a patch to a file at the specified path. <br>This can be used to edit a file without having to rewrite the whole file.                                                                                                         | 🔴               |
| [`fs_rm.sh`](../../assets/functions/tools/fs_rm.sh)                                 | Remove a file or directory at the specified path.                                                                                                                                                                                    | 🔴               |
| [`fs_write.sh`](../../assets/functions/tools/fs_write.sh)                           | Write the full file contents to a file at the specified path.                                                                                                                                                                        | 🟢               |
| [`get_current_time.sh`](../../assets/functions/tools/get_current_time.sh)           | Get the current time.                                                                                                                                                                                                                | 🟢               |
| [`get_current_weather.py`](../../assets/functions/tools/get_current_weather.py)     | Get the current weather in a given location (Python implementation)                                                                                                                                                                  | 🔴               |
| [`get_current_weather.sh`](../../assets/functions/tools/get_current_weather.sh)     | Get the current weather in a given location.                                                                                                                                                                                         | 🟢               |
| [`query_jira_issues.sh`](../../assets/functions/tools/query_jira_issues.sh)         | Query for jira issues using a Jira Query Language (JQL) query.                                                                                                                                                                       | 🟢               |
| [`search_arxiv.sh`](../../assets/functions/tools/search_arxiv.sh)                   | Search arXiv using the given search query and return the top papers.                                                                                                                                                                 | 🔴               |
| [`search_wikipedia.sh`](../../assets/functions/tools/search_wikipedia.sh)           | Search Wikipedia using the given search query. <br>Use it to get detailed information about a public figure, interpretation of a <br>complex scientific concept or in-depth connectivity of a significant historical <br>event, etc. | 🔴               |
| [`search_wolframalpha.sh`](../../assets/functions/tools/search_wolframalpha.sh)     | Get an answer to a question using Wolfram Alpha. The input query should be <br>in English. Use it to answer user questions that require computation, detailed <br>facts, data analysis, or complex queries.                          | 🔴               |
| [`send_mail.sh`](../../assets/functions/tools/send_mail.sh)                         | Send an email.                                                                                                                                                                                                                       | 🔴               |
| [`send_twilio.sh`](../../assets/functions/tools/send_twilio.sh)                     | Send SMS or Twilio Messaging Channels messages using the Twilio API.                                                                                                                                                                 | 🔴               |
| [`web_search_loki.sh`](../../assets/functions/tools/web_search_loki.sh)             | Perform a web search to get up-to-date information or additional context. <br>Use this when you need current information or feel a search could provide <br>a better answer.                                                         | 🔴               |
| [`web_search_perplexity.sh`](../../assets/functions/tools/web_search_perplexity.sh) | Perform a web search using the Perplexity API to get up-to-date <br>information or additional context. Use this when you need current <br>information or feel a search could provide a better answer.                                | 🔴               |
| [`web_search_tavily.sh`](../../assets/functions/tools/web_search_tavily.sh)         | Perform a web search using the Tavily API to get up-to-date <br>information or additional context. Use this when you need current <br>information or feel a search could provide a better answer.                                    | 🔴               |

Details on what configuration, if any, is necessary for each tool can be found inside the tool file definition itself.

## Configuration
Tools can be used in a handful of contexts:
* Inside a session
* Inside a role
* Inside an agent
* Globally (i.e. outside a session, role, or agent)

Each of these has a different configuration and interaction with the global configuration.

**Note:** For each configuration property listed below, the functions that are mentioned *only*
correspond to the tool scripts located in your Loki `functions/tools` directory.

### Global Configuration
The global configuration is essentially what settings you want to have on by default when
you just invoke `loki`. (Don't worry about agents, roles, or sessions yet. We'll get to them in a bit).

The following settings are available in the global configuration for tools:

```yaml
function_calling_support: true   # Enables or disables function calling in any context
mapping_tools:                   # Alias for a tool or toolset
  fs: 'fs_cat,fs_ls,fs_mkdir,fs_rm,fs_write'
enabled_tools: null              # Which tools to use by default. (e.g. 'fs,web_search_loki')
visible_tools:                   # Which tools are visible to be compiled (and are thus able to be defined in 'enabled_tools')
  #  - demo_py.py
  - execute_command.sh
```

A special not about `enabled_tools`: a user can set this to `all` to enable all available tools listed in the 
`visible_tools` section of your Loki `config.yaml` file.
See the [Enabling/Disabling Global Tools](#enablingdisabling-global-tools) section below for more information on how tools
are globally enabled/disabled globally.

(See the [Configuration Example](../../config.example.yaml) file for an example global configuration with all options.)

When running in REPL-mode, the `function_calling_support` and `enabled_tools` settings can be overridden using the 
`.set` command:

![REPL set function calling](../images/tools/global-settings-overrides-repl.png)

You'll notice that mentioned above, some tools are disabled while others are enabled. How is that determined?

### Enabling/Disabling Global Tools
The configured tools are enabled/disabled by looking at the values in the `visible_tools` array in your `config.yaml` 
file. This file is located in the root of the Loki `config` directory. The location of the Loki config varies by system,
so your config file can be found using the following command:

```bash
loki --info | grep 'config_file' | awk '{print $2}'
```

Each line in the `visible_tools` array lists a tool.

If that line is commented out, then that tool is not included in the global tool set, and cannot be used in any context;
This means it will not be built, and even if enabled under `enabled_tools`, it still will not be available in any 
context.

### Role Configuration
When you create a role, you have the following global tool-related configuration options available to you:

```yaml
enabled_tools: query_jira_issues    # Which tools the role uses.
```

The values for `mapping_tools` are inherited from the [global configuration](#global-configuration).

For more information about roles, refer to the [Roles](../ROLES.md) documentation.

### Agent Configuration
When you create an agent, you have the following global tool-related configuration options available to you:

```yaml
global_tools:                 # Which global tools the agent uses
  - query_jira_issues.sh
  - fs_cat.sh
  - fs_ls.sh
```

The values for `mapping_tools` are inherited from the [global configuration](#global-configuration).

For more information about agents, refer to the [Agents](../AGENTS.md) documentation.

For a full example configuration for an agent, see the [Agent Configuration Example](../../config.agent.example.yaml) file.

---

## Tool Error Handling
When tools fail, Loki captures error information and passes it back to the model so it can diagnose issues and 
potentially retry or adjust its approach.

### Native/Shell Tool Errors
When a shell-based tool exits with a non-zero exit code, the model receives:

```json
{
  "tool_call_error": "Tool call 'my_tool' exited with code 1",
  "stderr": "Error: file not found: config.json"
}
```

The `stderr` field contains the actual error output from the tool, giving the model context about what went wrong.
If the tool produces no stderr output, only the `tool_call_error` field is included.

**Note:** Tool stdout streams to your terminal in real-time so you can see progress. Only stderr is captured for 
error reporting.

### MCP Tool Errors
When an MCP (Model Context Protocol) tool invocation fails due to connection issues, timeouts, or server errors,
the model receives:

```json
{
  "tool_call_error": "MCP tool invocation failed: connection refused"
}
```

This allows the model to understand that an external service failed and take appropriate action (retry, use an 
alternative approach, or inform the user).

### Why This Matters
Without proper error propagation, models would only know that "something went wrong" without understanding *what*
went wrong. By including stderr output and detailed error messages, models can:

- Diagnose the root cause of failures
- Suggest fixes (e.g., "the file doesn't exist, should I create it?")
- Retry with corrected parameters
- Fall back to alternative approaches when appropriate
