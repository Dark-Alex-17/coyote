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
- [Authentication](#authentication)
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

| Setting  | Description                                                                                                |
|----------|------------------------------------------------------------------------------------------------------------|
| `name`   | The name of the client (e.g. `openai`, `gemini`, etc.)                                                     |
| `auth`   | Authentication method: `oauth` for OAuth, or omit to use `api_key` (see [Authentication](#authentication)) |
| `models` | See the [model settings](#model-settings) documentation below                                              |
| `patch`  | See the [client patch configuration](./PATCHES.md#client-configuration-patches) documentation              |
| `extra`  | See the [extra settings](#extra-settings) documentation below                                              |

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

## Authentication

Loki clients support two authentication methods: **API keys** and **OAuth**. Each client entry in your configuration
must use one or the other.

### API Key Authentication

Most clients authenticate using an API key. Simply set the `api_key` field directly or inject it from the
[Loki vault](../VAULT.md):

```yaml
clients:
  - type: claude
    api_key: '{{ANTHROPIC_API_KEY}}'
```

API keys can also be provided via environment variables named `{CLIENT_NAME}_API_KEY` (e.g. `OPENAI_API_KEY`,
`GEMINI_API_KEY`). See the [environment variables documentation](../ENVIRONMENT-VARIABLES.md#client-related-variables)
for details.

### OAuth Authentication

For [providers that support OAuth](#providers-that-support-oauth), you can authenticate using your existing subscription instead of an API key. This uses
the OAuth 2.0 PKCE flow.

**Step 1: Configure the client**

Add a client entry with `auth: oauth` and no `api_key`:

```yaml
clients:
  - type: claude
    name: my-claude-oauth
    auth: oauth
```

**Step 2: Authenticate**

Run the `--authenticate` flag with the client name:

```sh
loki --authenticate my-claude-oauth
```

Or if you have only one OAuth-configured client, you can omit the name:

```sh
loki --authenticate
```

Alternatively, you can use the REPL command `.authenticate`.

This opens your browser for the OAuth authorization flow. Depending on the provider, Loki will either start a
temporary localhost server to capture the callback automatically (e.g. Gemini) or ask you to paste the authorization
code back into the terminal (e.g. Claude). Loki stores the tokens in `~/.cache/loki/oauth` and automatically refreshes
them when they expire.

#### Gemini OAuth Note
Loki uses the following scopes for OAuth with Gemini:
* https://www.googleapis.com/auth/generative-language.peruserquota 
* https://www.googleapis.com/auth/userinfo.email
* https://www.googleapis.com/auth/generative-language.retriever (Sensitive)

Since the `generative-language.retriever` scope is a sensitive scope, Google needs to verify Loki, which requires full
branding (logo, official website, privacy policy, terms of service, etc.). The Loki app is open-source and is designed 
to be used as a simple CLI. As such, there's no terms of service or privacy policy associated with it, and thus Google 
cannot verify Loki. 

So, when you kick off OAuth with Gemini, you may see a page similar to the following:
![](../images/clients/gemini-oauth-page.png)

Simply click the `Advanced` link and click `Go to Loki (unsafe)` to continue the OAuth flow.

![](../images/clients/gemini-oauth-unverified.png)
![](../images/clients/gemini-oauth-unverified-allow.png)

**Step 3: Use normally**

Once authenticated, the client works like any other. Loki uses the stored OAuth tokens automatically:

```sh
loki -m my-claude-oauth:claude-sonnet-4-20250514 "Hello!"
```

> **Note:** You can have multiple clients for the same provider. For example: you can have one with an API key and 
> another with OAuth. Use the `name` field to distinguish them.

### Providers That Support OAuth
* Claude
* Gemini

## Extra Settings
Loki also lets you customize some extra settings for interacting with APIs:

| Setting           | Description                                           |
|-------------------|-------------------------------------------------------|
| `proxy`           | Set a proxy to use                                    |
| `connect_timeout` | Set the timeout in seconds for connections to the API |
