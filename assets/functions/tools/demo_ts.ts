/**
 * Demonstrates all supported TypeScript parameter types and variations.
 *
 * @param string - A required string property
 * @param string_enum - A required string property constrained to specific values
 * @param boolean - A required boolean property
 * @param number - A required number property
 * @param array_bracket - A required string array using bracket syntax
 * @param array_generic - A required string array using generic syntax
 * @param string_optional - An optional string using the question mark syntax
 * @param string_nullable - An optional string using the union-with-null syntax
 * @param number_with_default - An optional number with a default value
 * @param boolean_with_default - An optional boolean with a default value
 * @param string_with_default - An optional string with a default value
 * @param array_optional - An optional string array using the question mark syntax
 */
export function run(
  string: string,
  string_enum: "foo" | "bar",
  boolean: boolean,
  number: number,
  array_bracket: string[],
  array_generic: Array<string>,
  string_optional?: string,
  string_nullable: string | null = null,
  number_with_default: number = 42,
  boolean_with_default: boolean = true,
  string_with_default: string = "hello",
  array_optional?: string[],
): string {
  const parts = [
    `string: ${string}`,
    `string_enum: ${string_enum}`,
    `boolean: ${boolean}`,
    `number: ${number}`,
    `array_bracket: ${JSON.stringify(array_bracket)}`,
    `array_generic: ${JSON.stringify(array_generic)}`,
    `string_optional: ${string_optional}`,
    `string_nullable: ${string_nullable}`,
    `number_with_default: ${number_with_default}`,
    `boolean_with_default: ${boolean_with_default}`,
    `string_with_default: ${string_with_default}`,
    `array_optional: ${JSON.stringify(array_optional)}`,
  ];

  for (const [key, value] of Object.entries(process.env)) {
    if (key.startsWith("LLM_")) {
      parts.push(`${key}: ${value}`);
    }
  }

  return parts.join("\n");
}
