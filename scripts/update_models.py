import requests
import sys
import re
import json

# Provider mapping from models.yaml to OpenRouter prefixes
PROVIDER_MAPPING = {
    "openai": "openai",
    "claude": "anthropic",
    "gemini": "google",
    "mistral": "mistralai",
    "cohere": "cohere",
    "perplexity": "perplexity",
    "xai": "x-ai",
    "openrouter": "openrouter",
    "ai21": "ai21",
    "deepseek": "deepseek",
    "moonshot": "moonshotai",
    "qianwen": "qwen",
    "zhipuai": "zhipuai",
    "minimax": "minimax",
    "vertexai": "google",
    "groq": "groq",
    "bedrock": "amazon",
    "hunyuan": "tencent",
    "ernie": "baidu",
    "github": "github",
}

def fetch_openrouter_models():
    print("Fetching models from OpenRouter...")
    try:
        response = requests.get("https://openrouter.ai/api/v1/models")
        response.raise_for_status()
        data = response.json()["data"]
        print(f"Fetched {len(data)} models.")
        return data
    except Exception as e:
        print(f"Error fetching models: {e}")
        sys.exit(1)

def get_openrouter_model(models_data, provider_prefix, model_name, is_openrouter_provider=False):
    if is_openrouter_provider:
        # For openrouter provider, the model_name in yaml is usually the full ID
        for model in models_data:
            if model["id"] == model_name:
                return model
        return None

    expected_id = f"{provider_prefix}/{model_name}"
    
    # 1. Try exact match on ID
    for model in models_data:
        if model["id"] == expected_id:
            return model
            
    # 2. Try match by suffix
    for model in models_data:
        if model["id"].split("/")[-1] == model_name:
            if model["id"].startswith(f"{provider_prefix}/"):
                return model

    return None

def format_price(price_per_token):
    if price_per_token is None:
        return None
    try:
        price_per_1m = float(price_per_token) * 1_000_000
        if price_per_1m.is_integer():
            return str(int(price_per_1m))
        else:
            return str(round(price_per_1m, 4))
    except:
        return None

def get_indentation(line):
    return len(line) - len(line.lstrip())

def process_model_block(block_lines, current_provider, or_models):
    if not block_lines:
        return []

    # 1. Identify model name and indentation
    name_line = block_lines[0]
    name_match = re.match(r"^(\s*)-\s*name:\s*(.+)$", name_line)
    if not name_match:
        return block_lines 

    name_indent_str = name_match.group(1)
    model_name = name_match.group(2).strip()
    
    # 2. Find OpenRouter model
    or_prefix = PROVIDER_MAPPING.get(current_provider)
    is_openrouter_provider = (current_provider == "openrouter")
    
    if not or_prefix and not is_openrouter_provider:
        return block_lines
        
    or_model = get_openrouter_model(or_models, or_prefix, model_name, is_openrouter_provider)
    if not or_model:
        return block_lines

    print(f"  Updating {model_name}...")

    # 3. Prepare updates
    updates = {}
    
    # Pricing
    pricing = or_model.get("pricing", {})
    p_in = format_price(pricing.get("prompt"))
    p_out = format_price(pricing.get("completion"))
    if p_in: updates["input_price"] = p_in
    if p_out: updates["output_price"] = p_out
    
    # Context
    ctx = or_model.get("context_length")
    if ctx: updates["max_input_tokens"] = str(ctx)
    
    max_out = None
    if "top_provider" in or_model and or_model["top_provider"]:
        max_out = or_model["top_provider"].get("max_completion_tokens")
    if max_out: updates["max_output_tokens"] = str(max_out)
    
    # Capabilities
    arch = or_model.get("architecture", {})
    modality = arch.get("modality", "")
    if "image" in modality:
        updates["supports_vision"] = "true"

    # 4. Detect field indentation
    field_indent_str = None
    existing_fields = {} # key -> line_index
    
    for i, line in enumerate(block_lines):
        if i == 0: continue # Skip name line
        
        # Skip comments
        if line.strip().startswith("#"):
            continue
            
        # Look for "key: value"
        m = re.match(r"^(\s*)([\w_-]+):", line)
        if m:
            indent = m.group(1)
            key = m.group(2)
            # Must be deeper than name line
            if len(indent) > len(name_indent_str):
                if field_indent_str is None:
                    field_indent_str = indent
                existing_fields[key] = i

    if field_indent_str is None:
        field_indent_str = name_indent_str + "  "

    # 5. Apply updates
    new_block = list(block_lines)
    
    # Update existing fields
    for key, value in updates.items():
        if key in existing_fields:
            idx = existing_fields[key]
            # Preserve original key indentation exactly
            original_line = new_block[idx]
            m = re.match(r"^(\s*)([\w_-]+):", original_line)
            if m:
                current_indent = m.group(1)
                new_block[idx] = f"{current_indent}{key}: {value}\n"
    
    # Insert missing fields
    # Insert after the name line
    insertion_idx = 1
    
    for key, value in updates.items():
        if key not in existing_fields:
            new_line = f"{field_indent_str}{key}: {value}\n"
            new_block.insert(insertion_idx, new_line)
            insertion_idx += 1
            
    return new_block

def main():
    or_models = fetch_openrouter_models()
    
    print("Reading models.yaml...")
    with open("models.yaml", "r") as f:
        lines = f.readlines()
        
    new_lines = []
    current_provider = None
    
    i = 0
    while i < len(lines):
        line = lines[i]
        
        # Check for provider
        # - provider: name
        p_match = re.match(r"^\s*-?\s*provider:\s*(.+)$", line)
        if p_match:
            current_provider = p_match.group(1).strip()
            new_lines.append(line)
            i += 1
            continue
            
        # Check for model start
        # - name: ...
        m_match = re.match(r"^(\s*)-\s*name:\s*.+$", line)
        if m_match:
            # Start of a model block
            start_indent = len(m_match.group(1))
            
            # Collect block lines
            block_lines = [line]
            j = i + 1
            while j < len(lines):
                next_line = lines[j]
                stripped = next_line.strip()
                
                # If empty or comment, include it
                if not stripped or stripped.startswith("#"):
                    block_lines.append(next_line)
                    j += 1
                    continue
                
                # Check indentation
                next_indent = get_indentation(next_line)
                
                # If indentation is greater, it's part of the block (property)
                if next_indent > start_indent:
                    block_lines.append(next_line)
                    j += 1
                    continue
                
                # If indentation is equal or less, it's the end of the block
                break
            
            # Process the block
            processed_block = process_model_block(block_lines, current_provider, or_models)
            new_lines.extend(processed_block)
            
            # Advance i
            i = j
            continue
            
        # Otherwise, just a regular line
        new_lines.append(line)
        i += 1
        
    print("Saving models.yaml...")
    with open("models.yaml", "w") as f:
        f.writelines(new_lines)
    print("Done.")

if __name__ == "__main__":
    main()
