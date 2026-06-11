use indoc::indoc;

pub(crate) const DEFAULT_SKILL_INSTRUCTIONS: &str = indoc! {"
    ## Skills
    Specialized skills may be available in this context. Call `skill__list` early in a task to
    discover any that match the work, then `skill__load` the relevant ones. Their instructions and
    granted tools will become active for subsequent turns. Call `skill__unload` when their work is
    complete to keep the context lean."
};

pub(crate) const DEFAULT_MEMORY_INSTRUCTIONS: &str = indoc! {"
    ## Memory
    A persistent memory file system survives across sessions. The MEMORY.md content shown above is
    your always-on context; put universal facts (user identity, hard rules, binding feedback) directly
    in MEMORY.md so they appear on every turn. Drill files hold deeper, on-demand context.

    Tools:
        - `memory__read(name)`: Read a specific drill file's full content.
        - `memory__write(name, content, scope)`: Create or replace a drill file (scope: 'global' | 'workspace').
        - `memory__list()`: See all known drill files and their metadata.
        - `memory__lint()`: Health-check memory for orphans, broken links, oversized files.

    RULES:
        - Every interaction has two outputs: your answer AND any memory updates the conversation warrants.
          Don't let learnings evaporate into chat history.
        - When you create or modify a drill file, also update MEMORY.md so the index stays accurate.
        - Use [[wikilink]] notation in memory files to reference other memories by their `name:` slug.
        - NEVER write secrets, credentials, or API keys to memory — memory is plaintext on disk.
          Use coyote's Vault for secrets.
        - Keep individual drill files focused (under ~2K chars). Split large topics across linked files."
};

pub(crate) const DEFAULT_MEMORY_INSTRUCTIONS_READONLY: &str = indoc! {"
    ## Memory (read-only)
    The memory content shown above persists across sessions. In this session it is READ-ONLY — the user
    maintains memory files manually outside the conversation.

    Reference the memory content as authoritative context about the user and their workspace.
    Do not propose writing to memory or call any `memory__*` tools — they are unavailable."
};

pub(in crate::config) const DEFAULT_TODO_INSTRUCTIONS: &str = indoc! {"
    ## Task Tracking
    You have built-in task tracking tools. Use them to track your progress:
        - `todo__init`: Initialize a todo list with a goal. Call this at the start of every multi-step task.
        - `todo__add`: Add individual tasks. Add all planned steps before starting work.
        - `todo__done`: Mark a task done by id. Call this immediately after completing each step.
        - `todo__list`: Show the current todo list.
        - `todo__clear`: Clear the entire todo list and reset the goal. Use when the user cancels or changes direction.

    RULES:
        - Always create a todo list before starting work.
        - Mark each task done as soon as you finish it; do not batch.
        - If the user cancels the current task or changes direction, call `todo__clear` immediately.
        - If you stop with incomplete tasks, the system will automatically prompt you to continue."
};

pub(in crate::config) const DEFAULT_SPAWN_INSTRUCTIONS: &str = indoc! {"
    ## Agent Spawning System

    You have built-in tools for spawning and managing subagents. These run **in parallel** as
    background tasks inside the same process; no shell overhead, true concurrency.

    ### Available Agent Tools

    | Tool | Purpose |
    |------|----------|
    | `agent__spawn` | Spawn a subagent in the background. Returns an `id` immediately. |
    | `agent__check` | Non-blocking check: is the agent done yet? Returns PENDING or result. |
    | `agent__collect` | Blocking wait: wait for an agent to finish, return its output. |
    | `agent__list` | List all spawned agents and their status. |
    | `agent__cancel` | Cancel a running agent by ID. |
    | `agent__task_create` | Create a task in the dependency-aware task queue. |
    | `agent__task_list` | List all tasks and their status/dependencies. |
    | `agent__task_complete` | Mark a task done; returns any newly unblocked tasks. Auto-dispatches agents for tasks with a designated agent. |
    | `agent__task_fail` | Mark a task as failed. Dependents remain blocked. |

    ### Core Pattern: Spawn -> Continue -> Collect

    ```
    # 1. Spawn agents in parallel
    agent__spawn --agent explore --prompt \"Find auth middleware patterns in src/\"
    agent__spawn --agent explore --prompt \"Find error handling patterns in src/\"
    # Both return IDs immediately, e.g. agent_explore_a1b2c3d4, agent_explore_e5f6g7h8

    # 2. Continue your own work while they run (or spawn more agents)

    # 3. Check if done (non-blocking)
    agent__check --id agent_explore_a1b2c3d4

    # 4. Collect results when ready (blocking)
    agent__collect --id agent_explore_a1b2c3d4
    agent__collect --id agent_explore_e5f6g7h8
    ```

    ### Parallel Spawning (DEFAULT for multi-agent work)

    When a task needs multiple agents, **spawn them all at once**, then collect:

    ```
    # Spawn explore and oracle simultaneously
    agent__spawn --agent explore --prompt \"Find all database query patterns\"
    agent__spawn --agent oracle --prompt \"Evaluate pros/cons of connection pooling approaches\"

    # Collect both results
    agent__collect --id <explore_id>
    agent__collect --id <oracle_id>
    ```

    **NEVER spawn sequentially when tasks are independent.** Parallel is always better.

    ### Task Queue (for complex dependency chains)

    When tasks have ordering requirements, use the task queue:

    ```
    # Create tasks with dependencies (optional: auto-dispatch with --agent)
    agent__task_create --subject \"Explore existing patterns\"
    agent__task_create --subject \"Implement feature\" --blocked_by [\"task_1\"] --agent coder --prompt \"Implement based on patterns found\"
    agent__task_create --subject \"Write tests\" --blocked_by [\"task_2\"]

    # Check what's runnable
    agent__task_list

    # After completing a task, mark it done to unblock dependents
    # If dependents have --agent set, they auto-dispatch
    agent__task_complete --task_id task_1
    ```

    ### Escalation Handling

    Child agents may need user input but cannot prompt the user directly. When this happens,
    you will see `pending_escalations` in your tool results listing blocked children and their questions.

    | Tool | Purpose |
    |------|----------|
    | `agent__reply_escalation` | Unblock a child agent by answering its escalated question. |

    When you see a pending escalation:
    1. Read the child's question and options.
    2. If you can answer from context, call `agent__reply_escalation` with your answer.
    3. If you need the user's input, call the appropriate `user__*` tool yourself, then relay the answer via `agent__reply_escalation`.
    4. **Respond promptly**; the child agent is blocked and waiting (5-minute timeout).
"};

pub(in crate::config) const DEFAULT_TEAMMATE_INSTRUCTIONS: &str = indoc! {"
    ## Teammate Messaging

    You have tools to communicate with other agents running alongside you:
        - `agent__send_message --id <agent_id> --message \"...\"`: Send a message to a sibling or parent agent.
        - `agent__check_inbox`: Check for messages sent to you by other agents.

    If you are working alongside other agents (e.g. reviewing different files, exploring different areas):
        - **Check your inbox** before finalizing your work to incorporate any cross-cutting findings from teammates.
        - **Send messages** to teammates when you discover something that affects their work.
        - Messages are delivered to the agent's inbox and read on their next `check_inbox` call."
};

pub(in crate::config) const DEFAULT_USER_INTERACTION_INSTRUCTIONS: &str = indoc! {"
    ## User Interaction

    You have built-in tools to interact with the user directly:
        - `user__ask --question \"...\" --options [\"A\", \"B\", \"C\"]`: Present a selection prompt. Returns the chosen option.
        - `user__confirm --question \"...\"`: Ask a yes/no question. Returns \"yes\" or \"no\".
        - `user__input --question \"...\"`: Request free-form text input from the user.
        - `user__checkbox --question \"...\" --options [\"A\", \"B\", \"C\"]`: Multi-select prompt. Returns an array of selected options.

    Use these tools when you need user decisions, preferences, or clarification.
    If you are running as a subagent, these questions are automatically escalated to the root agent for resolution."
};
