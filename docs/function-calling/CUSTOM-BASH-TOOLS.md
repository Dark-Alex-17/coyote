# Custom Bash-Based Tools
Loki supports tools written in Bash. However, they must be written in a special format with special annotations in order 
for Loki to be able to properly parse and utilize them. This formatting ensures that each Bash script is 
self-describing, and formatted in such a way that Loki can anticipate how to execute it and what parameters to pass to 
it. This standardization also lets Loki compile the script into a JSON schema that can be used to inform the LLM about 
how to use the tool.

Each Bash-based tool must follow a specific structure in order for Loki to be able to properly compile and execute it:

* The tool must be a Bash script with a `.sh` file extension.
* The script must have the following comments:
    * `# @describe ...` comment at the top that describes the tool.
    * `# @env LLM_OUTPUT=/dev/stdout The output path` comment to describe the `LLM_OUTPUT` environment variable. This 
      syntax in particular assigns `/dev/stdout` as the default value for `LLM_OUTPUT`, so that if it's not set by Loki, 
      the script will still function properly.
    * `# @option --option <value>  An example option` comments to define each option that the tool accepts.
        * Use `--flag` syntax for boolean flags.
        * Use `--option <value>` syntax for options that accept a value.
        * Use `--option <value1,value2>` syntax for options that accept multiple values (i.e. arrays).
* The script must have a `main` function
* The `main` function must redirect the return value to the `>> "$LLM_OUTPUT"` environment variable.
    * This is necessary because Loki relies on the `$LLM_OUTPUT` environment variable to capture the output of the tool.

Essentially, you can think of the Bash-based tool script as just a normal Bash script that uses special comments to 
define a CLI.
* The `# @env LLM_OUTPUT=/dev/stdout` comment to define the `$LLM_OUTPUT` environment variable (good practice)
* A `# @describe`
* And a `main` function that writes to `$LLM_OUTPUT`

The following section explains how you can add parameters to your bash functions and how to test out your scripts.

## Quick Links:
<!--toc:start-->
- [Loki Bash Tools Syntax](#loki-bash-tools-syntax)
  - [Metadata](#metadata)
  - [Environment Variables](#environment-variables)
  - [Arguments](#arguments)
  - [Flags](#flags)
  - [Options](#options)
  - [Subcommands (Agents only)](#subcommands-agents-only)
- [Execute and Test Your Bash Tools](#execute-and-test-your-bash-tools)
  - [Example](#example)
- [Prompt Helpers](#prompt-helpers)
<!--toc:end-->

---

## Loki Bash Tools Syntax
Loki Bash tools work via `@___` annotations that describe specific functionality of a script. The following reference
explains the general syntax of these annotations and how to use them to create a CLI that Loki can recognize.

Refer to the [Execute and Test Your Bash Tools](#execute-and-test-your-bash-tools) section to learn how to test out your Bash tools
without needing to go through Loki itself.

It's important to note that any functions prefixed with `_` are not sent to the LLM, so they will be invisible to the 
LLM at runtime.

### Metadata:
You can define different metadata about your script to help Loki understand its dependencies and purpose.

```bash
# Use the `@meta require-tools` annotation to specify any external tools that your script depends on.
# @meta require-tools jq,yq

# Use the `@describe` annotation to describe the purpose of the script.
# @describe A tool to interact with things
```

### Environment Variables:
```bash
###########################
## Environment Variables ##
###########################

# Use `@env` to define environment variables that the script uses.
# @env LLM_OUTPUT=/dev/stdout                          The output path, with a default value of '/dev/stdout' if not set.
# @env OPTIONAL                                        An optional environment variable
# @env REQUIRED!                                       A required environment variable
# @env DEFAULT_VALUE=default                           An environment variable with a default value if unset.
# @env DEFAULT_FROM_FN=`_default_env_fn`               An environment variable with a default value calculated from a function if unset.
# @env CHOICE[even|odd]                                An environment variable that, if set, must be set to either `even` or `odd`
# @env CHOICE_WITH_DEFAULT[=even|odd]                  An environment variable that, if set, must be set to either `even` or `odd`, and defaults to `even` when unset
# @env CHOICE_FROM_FN[`_choice_env_fn`]                An environment variable that, if set, must be set to one of the values returned by the `_choice_fn` function.

# Example variable usage:
export CHOICE=even
# ./script.sh
main() {
  [[ $CHOICE == "even" ]] || { echo "The value of the 'CHOICE' env var is not 'even'" >> "$LLM_OUTPUT" && exit 1 }
}

# Loki does not pass functions prefixed with `_` to the LLM, so these are essentially `private` functions
_default_env_fn() {
  echo "calculated default env value"
}

# Loki does not pass functions prefixed with `_` to the LLM, so these are essentially `private` functions
_choice_env_fn() {
  echo even
  echo odd
}
```

### Arguments:
When referencing an argument defined via the `@arg` annotation, you can access its value using the `argc_<argument_name>` variable that
is created at runtime.

```bash
###############
## Arguments ##
###############

# Use `@arg` To define positional arguments for your script.
# To reference an argument within your script, use the `argc_<argument_name>` variable.
# @arg optional                                             Optional argument
# @arg required!                                            Required argument
# @arg multi_value*                                         An argument that accepts multiple values (e.g. './script.sh one two three')
# @arg multi_value_required+                                An argument that is required and accepts multiple values
# @arg value_notated <VALUE>                                An argument that explicitly specifies the name for documentation (e.g. Usage: ./script.sh [VALUE])
# @arg default=default                                      An argument with a default value if unset
# @arg default_from_fn=`_default_arg_fn`                    An argument with a default value calculated from a function if unset
# @arg choice[even|odd]                                     An argument that, if set, must be set to either `even` or `odd`
# @arg required_choice+[even|odd]                           An required argument that must be set to either `even` or `odd`
# @arg default_choice[=even|odd]                            An argument that if unset defaults to 'even', but if set must be either `even` or `odd`
# @arg multi_value_choice*[even|odd]                        An argument that, if set, must be set to either `even` or `odd`, and accepts multiple values
# @arg choice_fn[`_choice_arg_fn`]                          An argument that, if set, must be set to one of the values returned by the `_choice_arg_fn` function.
# @arg choice_fn_no_valid[?`_choice_arg_fn`]                An argument that, if set, can be set to one of the values returned by the `_choice_arg_fn` function,
#                                                           but does not validate the value.
# @arg multi_choice_fn*[`_choice_arg_fn`]                   An argument that, if set, must be set to one of the values returned by the `_choice_arg_fn` function,
#                                                           and accepts multiple values.
# @arg multi_choice_comma_fn*,[`_choice_arg_fn`]            An argument that, if set, must be set to one of the values returned by the `_choice_arg_fn` function,
#                                                           and accepts multiple values in the form of a comma-separated list
# @arg capture_arg~                                         An argument that captures all remaining args passed to the script

# Example usage 1: ./script.sh something_required
main() {
  [[ $argc_required == "something_required" ]] || { echo "The value of the 'required' arg is not 'something_required'" >> "$LLM_OUTPUT" && exit 1 }
}

# Example usage 2: ./script.sh this is a test
main() {
  [[ "${argc_multi_value[*]}" == "this is a test" ]] || { echo "The value of the 'multi_value' arg is not 'this is a test'" >> "$LLM_OUTPUT" && exit 1 }
}


# Loki does not pass functions prefixed with `_` to the LLM, so these are essentially `private` functions
_default_arg_fn() {
  echo "default arg value"
}

# Loki does not pass functions prefixed with `_` to the LLM, so these are essentially `private` functions
_choice_arg_fn() {
  echo even
  echo odd
}
```

### Flags:
To access the value of a flag defined via the `@flag` annotation, you can check the value of the `argc_<flag_name>` variable.

```bash
###########
## Flags ##
###########

# Use `@flag` to define boolean flags for your script
# To reference a flag within your script, use the `argc_<argument_name>` variable.
# @flag    --bool                              A boolean flag with only a long option
# @flag -b --bool                              A boolean flag with a short and long option
# @flag -b                                     A boolean flag with only a short option
# @flag    --multi*                            A boolean flag that can be used multiple times (e.g. '--multi --multi' will return '2')

# Example usage 1: ./script.sh --bool
main() {
  [[ $argc_bool == "1" ]] || { echo "The value of the 'bool' flag is not '1'" >> "$LLM_OUTPUT" && exit 1 }
}

# Example usage 2: ./script.sh --multi --multi
main() {
  [[ $argc_multi == "2" ]] || { echo "The value of the 'multi' flag is not 2" >> "$LLM_OUTPUT" && exit 1 }
}
```

### Options:
To access the value of an option defined via the `@option` annotation, you can check the value of the `argc_<option_name>` variable.

```bash
#############
## Options ##
#############

# Use `@option` to define flags that accept values
# To reference an option within your script, use the `argc_<argument_name>` variable.
# @option    --option                                     An option that accepts a value with only a long flag
# @option -o --option                                     An option that accepts a value with both a short and long flag
# @option -o                                              An option that accepts a value with only a short flag
# @option    --required                                   A required option that accepts a value
# @option    --multi*                                     An option that accepts multiple values                                      
# @option    --required-multi+                            An option that accepts multiple values and is required
# @option    --multi-comma*,                              An option that accepts multiple values in the form of a comma-separated list
# @option    --value <VALUE>                              An option that explicitly specifies the name for documentation (e.g. Usage: ./script.sh --value [VALUE])
# @option    --two-args <SRC> <DEST>                      An option that accepts two arguments and explicitly names them for documentation
#                                                         (e.g. Usage: ./script.sh --two-args [SRC] [DEST])
# @option    --unlimited-args <SRC> <DEST+>               An option that accepts an unlimited number of arguments and explicitly names them for documentation
#                                                         (e.g. Usage: ./script.sh --unlimited-args [SRC] [DEST ...])
# @option    --default=default                            An option that has a default value if unset
# @option    --default-from-fn=`_default_opt_fn`          An option that has a default value calculated from a function if unset
# @option    --choice[even|odd]                           An option that, if set, must be set to either `even` or `odd`
# @option    --choice-default[=even|odd]                  An option that, if unset, defaults to `even`, but if set must be either `even` or `odd`
# @option    --choice-multi*[even|odd]                    An option that, if set, must be set to either `even` or `odd`, and can be specified multiple times
#                                                         (e.g. ./script.sh --choice-multi even --choice-multi odd)
# @option    --required-choice-multi+[even|odd]           A required option that, must be set to either `even` or `odd`, and can be specified multiple times
# @option    --choice-fn[`_choice_opt_fn`]                An option that, if set, must be set to one of the values returned by the `_choice_opt_fn` function.`
# @option    --choice-fn-no-valid[?`_choice_opt_fn`]      An option that, if set, can be set to one of the values returned by the `_choice_opt_fn` function, with no validation
# @option    --choice-multi-fn*[`_choice_opt_fn`]         An option that, if set, must be set to one of the values returned by the `_choice_opt_fn` function,
#                                                         and can be specified multiple times
# @option    --choice-multi-comma*,[`_choice_opt_fn`]     An option that, if set, must be set to one of the values returned by the `_choice_opt_fn` function,
#                                                         and is specified as a comma-separated list
# @option    --capture~                                   An option that captures all remaining arguments passed to the script

# Example usage 1: ./script.sh --option some_value
main() {
  [[ $argc_option == "some_value" ]] || { echo "The value of the 'option' option is not 'some_value'" >> "$LLM_OUTPUT" && exit 1 }
}

# Example usage 2: ./script.sh --multi value1 --multi value2
main() {
  [[ "${argc_multi[*]}" == "value1 value2" ]] || { echo "The value of the 'multi' option is not 'value1 value2'" >> "$LLM_OUTPUT" && exit 1 }
}


# Loki does not pass functions prefixed with `_` to the LLM, so these are essentially `private` functions
_default_opt_fn() {
  echo "calculated default option value"
}

# Loki does not pass functions prefixed with `_` to the LLM, so these are essentially `private` functions
_choice_opt_fn() {
  echo even
  echo odd
}
```

### Subcommands (Agents only):
By default, if no `@cmd` annotations are defined, the script's `main` function is treated as the default command.
However, for agents, there can be many functions defined in one file, and thus it is useful to create subcommands
to organize your agent's tools.

```bash
#################
## Subcommands ##
#################

# Use the `@cmd` annotation to define subcommands for your script.
# @cmd List all files
list() {
  ls -la >> "$LLM_OUTPUT"
}

# @cmd Output the contents of the specified file
# @arg file! The file to output
cat() {
  cat "$argc_file" >> "$LLM_OUTPUT"
}

# Example usage 1: ./script.sh cat myfile.txt
```

## Execute and Test Your Bash Tools
Your bash tools are just normal bash scripts stored in the `functions/tools` directory. So you can execute and test them
directly by first having Loki compile them so all this syntactic sugar means something.

This is achieved via the `loki --build-tools` command.

### Example
Suppose we want to execute the `functions/tools/get_current_time.sh` script for testing.

We'd first make sure the script is visible in all contexts by ensuring it's in the `visible_tools` array in your global 
`config.yaml` file. This ensures Loki builds the tool so it's ready to use in any context.

You can find the location of your global `config.yaml` file with the following command:

```shell
loki --info | grep 'config_file' | awk '{print $2}'
```

Then, we can instruct Loki to build the script so we can test it out:

```shell
loki --build-tools
```

This will add additional boilerplate to the top of the script so that it can be executed directly.

Finally, we can now execute the script:

```bash
$ ./get_current_time.sh
Fri Oct 24 05:55:04 PM MDT 2025
```

## Prompt Helpers
It's often useful to create interactive prompts for our bash tools so that our tools can get input from
users.

To accommodate this, Loki provides a set of prompt helper functions that can be referenced and used within your Bash
tools.

For more information, refer to the [Bash Prompt Helpers documentation](BASH-PROMPT-HELPERS.md).
