# Request Patching in Loki
Loki provides two mechanisms for modifying API requests sent to LLM providers: **Model-Specific Patches** and 
**Client Configuration Patches**. These allow you to customize request parameters, headers, and URLs to work around 
provider quirks or add custom behavior.

## Quick Links
- [Model-Specific Patches](#model-specific-patches)
- [Client Configuration Patches](#client-configuration-patches)
- [Comparison](#comparison)
- [Common Use Cases](#common-use-cases)
- [Environment Variable Patches](#environment-variable-patches)
- [Tips](#tips)
- [Debugging Patches](#debugging-patches)

---

## Model-Specific Patches

### Overview
Model-specific patches are applied **unconditionally** to a single model. They are useful for handling model-specific 
quirks or requirements.

### When to Use
- A specific model requires certain parameters to be set or removed
- A model needs different default values than other models from the same provider
- You need to add special configuration for one model only

### Structure

```yaml
models:
  - name: model-name
    type: chat
    # ... other model properties ...
    patch:
      url: "https://custom-endpoint.com"   # Optional: override the API endpoint
      body:                                # Optional: modify request body
        <parameter>: <value>               # Add or modify parameters
        <parameter>: null                  # Remove parameters (set to null)
      headers:                             # Optional: modify request headers
        <header-name>: <value>             # Add or modify headers
        <header-name>: null                # Remove headers (set to null)
```

### Examples

#### Example 1: Removing Parameters
OpenAI's o1 models don't support `temperature`, `top_p`, or `max_tokens` parameters. The `patch` removes them:

```yaml
- name: o4-mini
  type: chat
  max_input_tokens: 200000
  max_output_tokens: 100000
  supports_function_calling: true
  patch:
    body:
      max_tokens: null      # Remove max_tokens from request
      temperature: null     # Remove temperature from request
      top_p: null           # Remove top_p from request
```

#### Example 2: Setting Required Parameters
Some models require specific parameters to be set:

```yaml
- name: o4-mini-high
  type: chat
  patch:
    body:
      reasoning_effort: high  # Always set reasoning_effort to "high"
      max_tokens: null
      temperature: null
```

#### Example 3: Custom Endpoint
If a model needs a different API endpoint:

```yaml
- name: custom-model
  type: chat
  patch:
    url: "https://special-endpoint.example.com/v1/chat"
```

#### Example 4: Adding Headers
Add authentication or custom headers:

```yaml
- name: special-model
  type: chat
  patch:
    headers:
      X-Custom-Header: "special-value"
      X-API-Version: "2024-01"
```

### How It Works
1. When you use a model, Loki loads its configuration
2. If the model has a `patch` field, it's **always applied** to every request
3. The patch modifies the request URL, body, or headers before sending to the API
4. Parameters set to `null` are **removed** from the request

---

## Client Configuration Patches

### Overview
Client configuration patches allow you to apply customizations to **multiple models** based on 
**regex pattern matching**. They're defined in your `config.yaml` file and can target specific API types (`chat`, 
`embeddings`, or `rerank`).

### When to Use
- You want to apply the same settings to multiple models from a provider
- You need different configurations for different groups of models
- You want to override the default client model settings
- You need environment-specific customizations

### Structure

```yaml
clients:
  - type: <client>                      # e.g., gemini, openai, claude
    # ... client configuration ...
    patch:
      chat_completions:                 # For chat models
        '<regex-pattern>':              # Regex to match model names
          url: "..."                    # Optional: override endpoint
          body:                         # Optional: modify request body
            <parameter>: <value>
          headers:                      # Optional: modify headers
            <header>: <value>
      embeddings:                       # For embedding models
        '<regex-pattern>':
          # ... same structure ...
      rerank:                           # For reranker models
        '<regex-pattern>':
          # ... same structure ...
```

### Pattern Matching
- Patterns are **regular expressions** that match against the model name
- Use `.*` to match all models
- Use specific patterns like `gpt-4.*` to match model families
- Use `model1|model2` to match multiple specific models

### Examples

#### Example 1: Disable Safety Filters for Gemini Models
Apply to all Gemini chat models:

```yaml
clients:
  - type: gemini
    api_key: "{{GEMINI_API_KEY}}"
    patch:
      chat_completions:
        '.*':  # Matches all Gemini models
          body:
            safetySettings:
              - category: HARM_CATEGORY_HARASSMENT
                threshold: BLOCK_NONE
              - category: HARM_CATEGORY_HATE_SPEECH
                threshold: BLOCK_NONE
              - category: HARM_CATEGORY_SEXUALLY_EXPLICIT
                threshold: BLOCK_NONE
              - category: HARM_CATEGORY_DANGEROUS_CONTENT
                threshold: BLOCK_NONE
```

#### Example 2: Apply Settings to Specific Model Family
Only apply to GPT-4 models (not GPT-3.5):

```yaml
clients:
  - type: openai
    api_key: "{{OPENAI_API_KEY}}"
    patch:
      chat_completions:
        'gpt-4.*':  # Matches gpt-4, gpt-4-turbo, gpt-4o, etc.
          body:
            frequency_penalty: 0.2
            presence_penalty: 0.1
```

#### Example 3: Different Settings for Different Models
Apply different patches based on model name:

```yaml
clients:
  - type: openai
    api_key: "{{OPENAI_API_KEY}}"
    patch:
      chat_completions:
        'gpt-4o':  # Specific model
          body:
            temperature: 0.7
        'gpt-3.5.*':  # Model family
          body:
            temperature: 0.9
            max_tokens: 2000
```

#### Example 4: Modify Embedding Requests
Apply to embedding models:

```yaml
clients:
  - type: openai
    api_key: "{{OPENAI_API_KEY}}"
    patch:
      embeddings:
        'text-embedding-.*':  # All text-embedding models
          body:
            dimensions: 1536
            encoding_format: "float"
```

#### Example 5: Custom Headers for Specific Models
Add headers only for certain models:

```yaml
clients:
  - type: openai-compatible
    api_base: "https://api.example.com/v1"
    patch:
      chat_completions:
        'custom-model-.*':
          headers:
            X-Custom-Auth: "bearer-token"
            X-Model-Version: "latest"
```

#### Example 6: Override Endpoint for Specific Models
Use different endpoints for different model groups:

```yaml
clients:
  - type: openai-compatible
    api_base: "https://default-endpoint.com/v1"
    patch:
      chat_completions:
        'premium-.*':  # Premium models use different endpoint
          url: "https://premium-endpoint.com/v1/chat/completions"
```

### How It Works
1. When making a request, Loki checks if the client has a `patch` configuration
2. It looks at the appropriate API type (`chat_completions`, `embeddings`, or `rerank`)
3. For each pattern in that section, it checks if the regex matches the model name
4. If a match is found, that patch is applied to the request
5. Only the **first matching pattern** is applied (patterns are processed in order)

---

## Comparison

| Feature               | Model-Specific Patch  | Client Configuration Patch          |
|-----------------------|-----------------------|-------------------------------------|
| **Scope**             | Single model only     | Multiple models via regex           |
| **Matching**          | Exact model name      | Regular expression pattern          |
| **Application**       | Always applied        | Only if pattern matches             |
| **API Type**          | All APIs              | Separate for chat/embeddings/rerank |
| **Override**          | Cannot be overridden  | Can override model patch            |
| **Use Case**          | Model-specific quirks | User preferences & customization    |
| **Application Order** | Applied first         | Applied second (can override)       |

### Patch Application Order
When both patches are present, they're applied in this order:

1. **Model-Specific Patch**
2. **Client Configuration Patch**

This means client configuration patches can override model-specific patches if they modify the same parameters.

## Common Use Cases

### Removing Unsupported Parameters
Some models don't support standard parameters like `temperature` or `max_tokens`:

**Model Patch**:
```yaml
patch:
  body:
    temperature: null
    max_tokens: null
```

### Adding Provider-Specific Parameters
Providers often have unique parameters:

**Client Patch**:
```yaml
patch:
  chat_completions:
    '.*':
      body:
        safetySettings: [...]        # Gemini
        thinking_budget: 10000       # DeepSeek
        response_format:             # OpenAI
          type: json_object
```

### Changing Endpoints
Use custom or regional endpoints:

**Client Patch**:
```yaml
patch:
  chat_completions:
    '.*':
      url: "https://eu-endpoint.example.com/v1/chat"
```

### Setting Default Values
Provide defaults for specific models or model families:

**Client Patch**:
```yaml
patch:
  chat_completions:
    'claude-3-.*':
      body:
        max_tokens: 4096
        temperature: 0.7
```

### Custom Authentication
Add special authentication headers:

**Client Patch**:
```yaml
patch:
  chat_completions:
    '.*':
      headers:
        Authorization: "Bearer {{custom_token}}"
        X-Organization-ID: "org-123"
```

## Environment Variable Patches
You can also apply patches via environment variables for temporary overrides:

```bash
export LLM_PATCH_OPENAI_CHAT_COMPLETIONS='{"gpt-4.*":{"body":{"temperature":0.5}}}'
```

This takes precedence over client configuration patches but not model-specific patches.

## Tips
1. **Use model patches** for permanent, model-specific requirements
2. **Use client patches** for personal preferences or environment-specific settings
3. **Test regex patterns** carefully
4. **Set to `null`** to remove parameters, don't just omit them
5. **Check each model provider's docs** for available parameters and their formats
6. **Be specific** with patterns to avoid unintended matches
7. **Remember order matters** - first matching pattern wins for client patches
8. **Patches merge** - both types can be applied, with client patches overriding model patches

## Debugging Patches
To see what request is actually being sent, enable debug logging:

```bash
export RUST_LOG=loki=debug
loki "your prompt here"
```

This will show the final request body after all patches are applied.
