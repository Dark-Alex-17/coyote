# Jira AI Agent

## Overview

The Jira AI Agent is designed to assist with managing tasks within Jira projects, providing capabilities such as creating, searching, updating, assigning, linking, and commenting on issues. Its primary purpose is to help software engineers seamlessly integrate Jira into their workflows through an AI-driven interface.

## Configuration

### Variables

This agent accepts the following variables:

- **config**: Specifies the configuration file for the Jira CLI. This configuration should be located at `~/.config/.jira/<config_name>.yml`. Example: `work`.
- **project**: The Jira project key where operations are executed. Example: `PAN`.

### Customization

#### For a User's Specific Jira Instance

1. **Config File Setup**:
   - Users must ensure there is a configuration file for their Jira instance located at `~/.config/.jira/`. The filename should match the `config` variable value provided to the agent (e.g., for `config` set to `work`, ensure a `work.yml` exists).

2. **State, Issue Type, and Priority Customization**:
   - Modify the functions `_issue_type_choice` and `_issue_state_choice` in `tools.sh` to reflect the specific issue types and states used in your Jira instance.
   - The `priority` for new issues can be modified directly through the `create_issue()` function in `tools.sh` with options set to the values available in your Jira instance (e.g., Medium, Highest, etc.).

## How the Agent Works

The agent works by utilizing provided variables to interact with Jira CLI commands through `tools.sh`. The `config` variable links directly to a `.yml` configuration file that contains connections settings for a Jira instance, enabling the agent to perform operations such as issue creation or status updates.

- **Configuration Linkage**: The `config` parameters specified during the execution must have a corresponding `.yml` configuration file at `~/.config/.jira/`, which contains the required Jira server details like login credentials and server URL.
- **Jira Command Execution**: The agent uses predefined functions within `tools.sh` to execute Jira operations. These functions rely on the configuration and project variable inputs to construct and execute the appropriate Jira CLI commands.
