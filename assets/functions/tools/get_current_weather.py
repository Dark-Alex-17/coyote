import os
from pathlib import Path
from typing import Optional
from urllib.parse import quote_plus
from urllib.request import urlopen


def run(
    location: str,
    llm_output: Optional[str] = None,
) -> str:
    """Get the current weather in a given location

    Args:
        location (str): The city and optionally the state or country (e.g., "London", "San Francisco, CA").

    Returns:
        str: A single-line formatted weather string from wttr.in (``format=4`` with metric units).
    """
    url = f"https://wttr.in/{quote_plus(location)}?format=4&M"

    with urlopen(url, timeout=10) as resp:
        weather = resp.read().decode("utf-8", errors="replace")

    dest = llm_output if llm_output is not None else os.environ.get("LLM_OUTPUT", "/dev/stdout")

    if dest not in {"-", "/dev/stdout"}:
        path = Path(dest)
        path.parent.mkdir(parents=True, exist_ok=True)
        with path.open("a", encoding="utf-8") as fh:
            fh.write(weather)
    else:
        pass

    return weather
