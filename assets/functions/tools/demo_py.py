import os
from typing import List, Literal, Optional


def run(
    string: str,
    string_enum: Literal["foo", "bar"],
    boolean: bool,
    integer: int,
    number: float,
    array: List[str],
    string_optional: Optional[str] = None,
    integer_with_default: int = 42,
    boolean_with_default: bool = True,
    number_with_default: float = 3.14,
    string_with_default: str = "hello",
    array_optional: Optional[List[str]] = None,
):
    """Demonstrates all supported Python parameter types and variations.
    Args:
        string: A required string property
        string_enum: A required string property constrained to specific values
        boolean: A required boolean property
        integer: A required integer property
        number: A required number (float) property
        array: A required string array property
        string_optional: An optional string property (Optional[str] with None default)
        integer_with_default: An optional integer with a non-None default value
        boolean_with_default: An optional boolean with a default value
        number_with_default: An optional number with a default value
        string_with_default: An optional string with a default value
        array_optional: An optional string array property
    """
    output = f"""string: {string}
string_enum: {string_enum}
boolean: {boolean}
integer: {integer}
number: {number}
array: {array}
string_optional: {string_optional}
integer_with_default: {integer_with_default}
boolean_with_default: {boolean_with_default}
number_with_default: {number_with_default}
string_with_default: {string_with_default}
array_optional: {array_optional}"""

    for key, value in os.environ.items():
        if key.startswith("LLM_"):
            output = f"{output}\n{key}: {value}"

    return output
