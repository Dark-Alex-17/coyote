# Roles
When customizing the behavior or LLMs, we use roles to "constrain" the responses or behavior of the LLM to whatever
purpose we desire. 

Think of them kind of like a baby: That baby can grow up to do anything! Be a resume builder, teacher, engineer, etc.

The only difference is that with roles, we're explicitly telling the LLM what we want it to be. Also: the LLM is already
grown up so we don't have to wait!

![Role demo](./images/roles/code.gif)

## Quick Links
<!--toc:start-->
- [Role Definition](#role-definition)
  - [Metadata Header](#metadata-header)
  - [Instructions](#instructions)
  - [Special Case: Metadata Header Only](#special-case-metadata-header-only)
- [Prompt Types](#prompt-types)
  - [Embedded Prompts](#embedded-prompts)
  - [System Prompts](#system-prompts)
  - [Few-Shot Prompt](#few-shot-prompt)
- [Built-In Roles](#built-in-roles)
<!--toc:end-->

---

## Role Definition
Roles in Loki are Markdown files that live in the `roles` directory of your Loki configuration. Loki configuration 
locations vary between systems, so you can use the following command to find the location of your roles configuration
directory:

```shell
loki --info | grep 'roles_dir' | awk '{print $2}'
```

All role configuration files have two parts: The metadata header, and the instructions.

**Example:** An expert resume builder role that specializes in helping users build and refine their resumes.
```markdown
---
# This is the metadata header
name: resume-builder
model: openai:gpt-4o
temperature: 0.2
top_p: 0
enabled_tools: fs_ls,fs_cat
enabled_mcp_servers: github
---
<!-- This is the instructions -->
You are an expert resume builder.
```

To see a full example configuration for a role, refer to the [example role configuration](../config.role.example.md) 
file in the root of the repo.

### Metadata Header
The metadata header in all role configuration files is completely optional. It lets you define role-specific settings
for each role that make the model work the way you want for your role. This includes things like forcing your role to
always use a specific model, set of tools, and tailoring the hyperparameters of the model for your role.

The header consists of a YAML-formatted list of settings that let you customize the model behavior for your role. These 
settings sit between `---` separators in your role configuration so Loki knows they're not part of the instructions you 
want to feed the model.

The following table lists the available configuration settings and their default values (if undefined):

| Setting               | Default                                                        | Description                                                                                                   |
|-----------------------|----------------------------------------------------------------|---------------------------------------------------------------------------------------------------------------|
| `name`                | The name of the role markdown file                             | The name of the role                                                                                          |
| `model`               | Default configured model or currently in-use model (REPL mode) | The preferred model to use with this role                                                                     |
| `temperature`         | Default `temperature` for the preferred model                  | Controls the creativity and randomness of the model's responses                                               |
| `top_p`               | Default `top_p` for the preferred model                        | Alternative way to control the model's output diversity, affecting the <br>probability distribution of tokens |
| `enabled_tools`       | Global setting for `enabled_tools`                             | The tools that this role utilizes                                                                             |
| `enabled_mcp_servers` | Global setting for `enabled_mcp_servers`                       | The MCP servers that this role utilizes                                                                       |
| `prompt`              | `null`                                                         | See [Prompt Types](#prompt-types) for detailed usage                                                          | 

### Instructions
The instructions for a role is what you use to tell the model how you want it to behave. This typically consists of one 
or two sentences, but can be more. To see some examples, look at the [built-in roles](../assets/roles) to see how they are defined.

**Pro-Tip:** The struggle to create good instructions for a role (or any other kind of instructions for your model) is 
so common, that Loki comes with a role to help you write instructions for roles! Simply invoke the role to start 
creating a role with the `create-prompt` role:

```shell
loki -r create-prompt
```

### Special Case: Metadata Header Only
When instructions are defined, the metadata header is optional. However sometimes we want a way to enable specific 
functions or MCP servers when prompting different models. In this situation, you need only specify the metadata header 
to just enable these settings as you like.

**Example: Role that enables all filesystem tools**
`roles/filesystem-functions.md`
```markdown
---
enabled_tools: fs_ls,fs_cat,fs_mkdir,fs_patch,fs_write
---
```

**Example: Role that enables the GitHub MCP server with the ollama:deepseek-r1 model**
`roles/github.md`
```markdown
---
model: ollama:deepseek-r1
enabled_mcp_servers: github
---
```

For more examples of this special use case of roles, you can look at the role configuration files for the following
built-in roles:

* [explain-shell](../assets/roles/explain-shell.md) - Explains cryptic shell commands in natural language
* [functions](../assets/roles/functions.md) - Enables all available functions (i.e. all globally `visible_functions`)
* [mcp-servers](../assets/roles/mcp-servers.md) - Enables all available MCP servers

## Special Variables
Loki has a set of built-in special variables that it will inject into your role's instructions if it finds them in the 
`{{variable_name}}` syntax. The available special variables are listed below:

| Name            | Description                                               | Example                    |
|-----------------|-----------------------------------------------------------|----------------------------|
| `__os__`        | Operating system name                                     | `linux`                    |
| `__os_family__` | Operating system family                                   | `unix`                     |
| `__arch__`      | System architecture                                       | `x86_64`                   |
| `__shell__`     | The current user's default shell                          | `bash`                     |
| `__locale__`    | The current user's preferred language and region settings | `en-US`                    |
| `__now__`       | Current timestamp in ISO 8601 format                      | `2025-11-07T10:15:44.268Z` |
| `__cwd__`       | The current working directory                             | `/tmp`                     |

## Prompt Types
In Loki, you can also create roles with pre-configured prompts so you can template prompts for your use cases. This is 
the purpose of the `prompt` field in the role's metadata header. 

There's three types of prompts you can create:

### Embedded Prompts
Embedded prompts let you create templated prompts for any input given to it. They are ideal for concise, input-driven
replies from the model. The input that users pass to Loki are injected into your prompt via a `__INPUT__` placeholder in 
your prompt.

**Example: Role to convert the given input to TOML**
`roles/convert-to-toml.md`
```markdown
---
prompt: convert __INPUT__ to TOML
---
Convert the given input to TOML format. Exclude any markdown formatting or code blocks and only output code.
```
Usage:
```shell
$ loki -r json-to-toml '{"test":"hi me"}'
test = "hi me"
```

Without the instructions (i.e. the prompt after the metadata header), this role would simply generate the following 
message for the model:

```json
[
  {"role":  "user", "content": "convert {\"test\":\"hi me\"} to TOML"}
]
```

### System Prompts
System prompts let you set the general context of the LLMs behavior. This is no different than other system prompts you
define in ChatGPT, Claude, Open WebUI, etc.

They are essentially Embedded Prompts without an `__INPUT__` placeholder.

**Example: Role to convert all input words to emoji**
`roles/emoji.md`
```markdown
---
prompt: convert my words to emojis
---
Convert all given input words into emojis
```
Usage:
```shell
$ loki -r emoji music joy
🎵 😊
```

Without the instructions (i.e. the prompt after the metadata header), this role would simply generate the following 
messages for the model:

```json
[
  {"role": "system", "content":  "convert my words to emojis"},
  {"role": "user", "content":  "music joy"}
]
```

### Few-Shot Prompt
[Few-Shot prompting](https://www.promptingguide.ai/techniques/fewshot) is a technique to enable in-context learning for LLMs by providing examples in the prompt to steer 
the model to better performance. In Loki, this is done as an extension of System Prompts.

**Example: Role to output code only**
`roles/code-generator.md`
~~~markdown
---
prompt: |-
    Output code only without comments or explanations.
    ### INPUT:
    async sleep in js
    ### OUTPUT:
    ```javascript
    async function timeout(ms) {
      return new Promise(resolve => setTimeout(resolve, ms));
    }
    ```
---
Output code only in response to the user's request
~~~
Usage:
~~~shell
$ loki -r code-generator python add two numbers
```python
# Function to add two numbers
def add_numbers(num1, num2):
    return num1 + num2

# Example usage
number1 = 5
number2 = 7

result = add_numbers(number1, number2)
print(f"The sum of {number1} and {number2} is {result}.")
```
~~~

Without the instructions (i.e. the prompt after the metadata header), this role would simply generate the following
messages for the model:

```json
[
  {"role": "system", "content": "Output code only without comments or explanations."},
  {"role": "user", "content": "async sleep in js"},
  {"role": "assistant", "content": "```javascript\nasync function timeout(ms) {\n  return new Promise(resolve => setTimeout(resolve, ms));\n}\n```"},
  {"role": "user", "content": "python add two numbers"}
]
```

## Built-In Roles
Loki comes packaged with some useful built-in roles. These are also good examples if you're looking for more examples on
how to make your own roles, so be sure to check out the [built-in role definitions](../assets/roles) if you're looking 
for more examples.

* `code`: Generates code (used by `loki -c`)
* `create-prompt`: Creates a prompt based on the user's input
* `create-title`: Creates 3-6 word titles based on the user's input
* `explain-shell`: Explains shell commands
* `functions`: Enable all globally-visible functions
* `github`: Interact with GitHub using natural language
* `mcp-servers`: Enables all MCP servers
* `repo-analyzer`: Ask questions about the code repository in the current working directory
* `shell`: Convert natural language into shell commands (used by `loki -e`)
* `slack`: Interact with Slack using natural language

## Temporary Roles
Loki also enables you to create temporary roles that will be discarded once you're finished with them. This is done via 
the `.prompt/--prompt` command:

![prompt role](./images/roles/prompt-role.gif)