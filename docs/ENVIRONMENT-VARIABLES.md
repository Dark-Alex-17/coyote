# Environment Variables

Loki is designed to be highly dynamic and customizable. As a result, Loki utilizes a number of environment variables
that can be used to modify its behavior at runtime without needing to modify the existing configuration files.

Loki also supports defining environment variables via a `.env` file in the Loki configuration directory. This directory
varies between systems, so you can find the location of your configuration directory using the following command:

```shell
loki --info | grep 'config_dir' | awk '{print $2}'
```

## Quick Links
<!--toc:start-->
- [Global Configuration Related Variables](#global-configuration-related-variables)
- [Client Related Variables](#client-related-variables)
- [Files and Directory Related Variables](#files-and-directory-related-variables)
- [Agent Related Variables](#agent-related-variables)
- [Logging Related Variables](#logging-related-variables)
- [Miscellaneous Variables](#miscellaneous-variables)
<!--toc:end-->

---

## Global Configuration Related Variables
All configuration items in the global config file have environment variables that can be overridden at runtime. To see
all configuration options and more thorough descriptions, refer to the [example config file](../config.example.yaml).

Below are the most commonly used configuration settings and their corresponding environment variables:

| Setting                    | Environment Variable            |
|----------------------------|---------------------------------|
| `model`                    | `LOKI_MODEL`                    |
| `temperature`              | `LOKI_TEMPERATURE`              |
| `top_p`                    | `LOKI_TOP_P`                    |
| `stream`                   | `LOKI_STREAM`                   |
| `save`                     | `LOKI_SAVE`                     |
| `editor`                   | `LOKI_EDITOR`                   |
| `wrap`                     | `LOKI_WRAP`                     |
| `wrap_code`                | `LOKI_WRAP_CODE`                |
| `save_session`             | `LOKI_SAVE_SESSION`             |
| `compression_threshold`    | `LOKI_COMPRESSION_THRESHOLD`    |
| `function_calling_support` | `LOKI_FUNCTION_CALLING_SUPPORT` |
| `enabled_tools`            | `LOKI_ENABLED_TOOLS`            |
| `mcp_server_support`       | `LOKI_MCP_SERVER_SUPPORT`       |
| `enabled_mcp_servers`      | `LOKI_ENABLED_MCP_SERVERS`      |
| `rag_embedding_model`      | `LOKI_RAG_EMBEDDING_MODEL`      |
| `rag_reranker_model`       | `LOKI_RAG_RERANKER_MODEL`       |
| `rag_top_k`                | `LOKI_RAG_TOP_K`                |
| `rag_chunk_size`           | `LOKI_RAG_CHUNK_SIZE`           |
| `rag_chunk_overlap`        | `LOKI_RAG_CHUNK_OVERLAP`        |
| `highlight`                | `LOKI_HIGHLIGHT`                |
| `theme`                    | `LOKI_THEME`                    |
| `serve_addr`               | `LOKI_SERVE_ADDR`               |
| `user_agent`               | `LOKI_USER_AGENT`               |
| `save_shell_history`       | `LOKI_SAVE_SHELL_HISTORY`       |
| `sync_models_url`          | `LOKI_SYNC_MODELS_URL`          |


## Client Related Variables
The following environment variables are available for clients in Loki:

| Environment Variable                   | Description                                                                                                                                                                                                                                             |
|----------------------------------------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `{client}_API_KEY`                     | For clients that require an API key, you can define the keys either through environment variables or <br>using the [vault](./VAULT.md). The variables are named after the client to which they apply; <br>e.g. `OPENAI_API_KEY`, `GEMINI_API_KEY`, etc. |
| `LOKI_PLATFORM`                        | Combine with `{client}_API_KEY` to run Loki without a configuration file. <br>This variable is ignored if a configuration file exists.                                                                                                                  |
| `LOKI_PATCH_{client}_CHAT_COMPLETIONS` | Patch chat completion requests to models on the corresponding client; Can modify the URL, body, <br>or headers.                                                                                                                                         | 
| `LOKI_SHELL`                           | Specify the shell that Loki should be using when executing commands                                                                                                                                                                                     |

## Files and Directory Related Variables
You can also customize the files and directories that Loki loads its configuration files from:

| Environment Variable | Description                                                            | Default Value                   |
|----------------------|------------------------------------------------------------------------|---------------------------------|
| `LOKI_CONFIG_DIR`    | Customize the location of the Loki configuration directory.            | `<user-config-dir>/loki`        |
| `LOKI_ENV_FILE`      | Customize the location of the `.env` file to load at startup.          | `<loki-config-dir>/.env`        |
| `LOKI_CONFIG_FILE`   | Customize the location of the global `config.yaml` configuration file. | `<loki-config-dir>/config.yaml` |
| `LOKI_ROLES_DIR`     | Customize the location of the `roles` directory.                       | `<loki-config-dir>/roles`       |
| `LOKI_SESSIONS_DIR`  | Customize the location of the `sessions` directory.                    | `<loki-config-dir>/sessions`    |
| `LOKI_RAGS_DIR`      | Customize the location of the `rags` directory.                        | `<loki-config-dir>/rags`        |
| `LOKI_FUNCTIONS_DIR` | Customize the location of the `functions` directory.                   | `<loki-config-dir>/functions`   |

## Agent Related Variables
You can also customize the location of full agent configurations using the following environment variables:

| Environment Variable         | Description                                                                                                                         |
|------------------------------|-------------------------------------------------------------------------------------------------------------------------------------|
| `<AGENT_NAME>_CONFIG_FILE`   | Customize the location of the agent's configuration file; e.g. `SQL_CONFIG_FILE`                                                    |
| `<AGENT_NAME>_MODEL`         | Customize the `model` used for the agent; e.g `SQL_MODEL`                                                                           |
| `<AGENT_NAME>_TEMPERATURE`   | Customize the `temperature` used for the agent; e.g. `SQL_TEMPERATURE`                                                              |
| `<AGENT_NAME>_TOP_P`         | Customize the `top_p` used for the agent; e.g. `SQL_TOP_P`                                                                          |
| `<AGENT_NAME>_GLOBAL_TOOLS`  | Customize the `global_tools` that are enabled for the agent (a JSON string array); e.g. `SQL_GLOBAL_TOOLS`                          |
| `<AGENT_NAME>_MCP_SERVERS`   | Customize the `mcp_servers` that are enabled for the agent (a JSON string array); e.g. `SQL_MCP_SERVERS`                            |
| `<AGENT_NAME>_AGENT_SESSION` | Customize the `agent_session` used with the agent; e.g. `SQL_SESSION`                                                               |
| `<AGENT_NAME>_INSTRUCTIONS`  | Customize the `instructions` for the agent; e.g. `SQL_INSTRUCTIONS`                                                                 |
| `<AGENT_NAME>_VARIABLES`     | Customize the `variables` used for the agent (in JSON format of `[{"key1": "value1", "key2": "value2"}]`); <br>e.g. `SQL_VARIABLES` |

## Logging Related Variables
The following variables can be used to change the log level of Loki or the location of the log file:

| Environment Variable | Description                                 | Default Value                    |
|----------------------|---------------------------------------------|----------------------------------|
| `LOKI_LOG_LEVEL`     | Customize the log level of Loki             | `INFO`                           |
| `LOKI_LOG_FILE`      | Customize the location of the Loki log file | `<user-cache-dir>/loki/loki.log` |

**Pro-Tip:** You can always tail the Loki logs using the `--tail-logs` flag. If you need to disable color output, you
can also pass the `--disable-log-colors` flag as well.

## Miscellaneous Variables
| Environment Variable | Description                                                                                      | Default Value |
|----------------------|--------------------------------------------------------------------------------------------------|---------------|
| `AUTO_CONFIRM`       | Bypass all `guard_*` checks in the bash prompt helpers; useful for agent composition and routing |               |