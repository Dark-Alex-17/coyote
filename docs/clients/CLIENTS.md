# Model Clients

Loki supports a large number of model providers (referred to as `clients` since Loki is a client of these providers). In 
order to use them, you must configure each one in the `clients` array in the global Loki configuration file.

The location of the global Loki configuration file varies between systems, so you can use the following command to 
locate your configuration file:

```shell
loki --info | grep 'config_file' | awk '{print $2}'
```

## Quick Links
<!--toc:start-->
- [Supported Clients](#supported-clients)
- [Client Configuration](#client-configuration)
- [Extra Settings](#extra-settings)
<!--toc:end-->

---

## Supported Clients
Loki supports the following model client types:

* Azure AI Foundry
* AWS Bedrock
* Anthropic Claude
* Cohere
* Google Gemini
* OpenAI
* OpenAI-Compatible
* GCP Vertex AI

In addition to the settings detailed below, each client may have additional settings specific to the provider. Check the
[example global configuration file](../../config.example.yaml) to verify that your client has all the necessary fields
defined.

## Client Configuration
Each client in Loki has the same configuration settings available to them, with only special authentication fields added
for specific clients as necessary. They are each placed under the `clients` array in your global configuration file:

```yaml
clients:
  - name: client1
    # ... client configuration ...
  - name: client2
    # ... client configuration ...
```

### Metadata
The client metadata uniquely identifies the client in Loki so you can reference it across your configurations. The 
available settings are listed below:

| Setting  | Description                                                                                   |
|----------|-----------------------------------------------------------------------------------------------|
| `name`   | The name of the client (e.g. `openai`, `gemini`, etc.)                                        |
| `models` | See the [model settings](#model-settings) documentation below                                 |
| `patch`  | See the [client patch configuration](./PATCHES.md#client-configuration-patches) documentation |
| `extra`  | See the [extra settings](#extra-settings) documentation below                                 |

Be sure to also check provider-specific configurations for any extra fields that are added for authentication purposes.

### Model Settings
The `models` array lists the available models from the model client. Each one has the following settings:

| Setting                     | Required | Model Type  | Description                                                                                                                                                                                                                                   |
|-----------------------------|----------|-------------|-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `name`                      | *        | `all`       | The name of the model                                                                                                                                                                                                                         |
| `real_name`                 |          | `all`       | You can define model aliases via the `name` field. However, Loki still needs to know the real name <br>of the model so it can query it. For example: If you have `name: gpt-alias`, then you must <br>also define `real_name: gpt-oss:latest` |
| `type`                      | *        | `all`       | The type of model. Loki supports only 3 types of models: <ul><li>`chat`</li><li>`embedding`</li><li>`reranker`</li></ul>                                                                                                                      |
| `input_price`               |          | `all`       | The cost in USD per 1M tokens for each input sequence; Loki will keep track of usage costs if this is defined                                                                                                                                 |
| `output_price`              |          | `all`       | The cost in USD per 1M tokens of the model output; Loki will keep track of usage costs if this is defined                                                                                                                                     |
| `patch`                     |          | `all`       | See the [model-specific patch configuration](./PATCHES.md#model-specific-patches) documentation                                                                                                                                               |
| `max_input_tokens`          |          | `all`       | The maximum number of input tokens for the model                                                                                                                                                                                              |
| `max_output_tokens`         |          | `chat`      | The maximum number of output tokens for the model                                                                                                                                                                                             |
| `require_max_tokens`        |          | `chat`      | Whether to enforce the `max_output_tokens` constraint.                                                                                                                                                                                        |
| `supports_vision`           |          | `chat`      | Indicates if the model supports multimodal queries that would require vision (i.e. image recognition)                                                                                                                                         |
| `supports_function_calling` |          | `chat`      | Indicates if the model supports function calling                                                                                                                                                                                              |
| `no_stream`                 |          | `chat`      | Enable or disable streaming API responses                                                                                                                                                                                                     |
| `no_system_message`         |          | `chat`      | Controls whether the model supports system messages                                                                                                                                                                                           |
| `system_prompt_prefix`      |          | `chat`      | An additional prefix prompt to add to all system prompts to ensure consistent behavior across all interactions                                                                                                                                |
| `max_tokens_per_chunk`      |          | `embedding` | The maximum chunk size supported by the embedding model                                                                                                                                                                                       |
| `default_chunk_size`        |          | `embedding` | The default chunk size to use with the given model                                                                                                                                                                                            |
| `max_batch_size`            |          | `embedding` | The maximum batch size that the given embedding model supports                                                                                                                                                                                |

## Extra Settings
Loki also lets you customize some extra settings for interacting with APIs:

| Setting           | Description                                           |
|-------------------|-------------------------------------------------------|
| `proxy`           | Set a proxy to use                                    |
| `connect_timeout` | Set the timeout in seconds for connections to the API |
