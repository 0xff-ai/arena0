import { type Schema, Validator } from "@cfworker/json-schema";
import type { JsonLike } from "../viz/json";

export interface SchemaIssue {
  /** RFC 6901 JSON pointer into the value: "" is the root, "/field/0" an array element. */
  path: string;
  message: string;
}

// Keywords whose errors only say "something below failed"; the leaf errors
// carry the message and the exact path.
const AGGREGATE = new Set(["properties", "items", "prefixItems", "$ref", "$dynamicRef", "allOf"]);

const REQUIRED_PROPERTY = /required property "(.*)"\.$/;

export const escapePointer = (key: string) => key.replace(/~/g, "~0").replace(/\//g, "~1");

/** A schema that admits only `null`, so there is nothing to fill in. */
export const isNullSchema = (schema: JsonLike): boolean =>
  typeof schema === "object" && schema !== null && !Array.isArray(schema) && schema.type === "null";

/**
 * Validates `value` against a JSON Schema (draft 2020-12) and returns one
 * issue per leaf failure. A missing property is reported at the property's
 * own path so a form can show it under the field.
 */
export function validate(schema: JsonLike, value: JsonLike | undefined): SchemaIssue[] {
  if (value === undefined) return [{ path: "", message: "a value is required" }];
  const result = new Validator(schema as Schema, "2020-12", false).validate(value);
  if (result.valid) return [];

  // Under oneOf/anyOf every alternative reports its own failure; those are
  // noise next to the "matches none of the options" error.
  const alternatives = result.errors.filter(
    (error) => error.keyword === "oneOf" || error.keyword === "anyOf",
  );
  const issues: SchemaIssue[] = [];
  const seen = new Set<string>();
  for (const error of result.errors) {
    if (AGGREGATE.has(error.keyword)) continue;
    if (alternatives.some((a) => error.keywordLocation.startsWith(`${a.keywordLocation}/`)))
      continue;
    // cfworker reports locations as URI-fragment pointers ("#/a/b").
    let path = decodeURI(error.instanceLocation.replace(/^#/, ""));
    let message = error.error;
    const missing = error.keyword === "required" ? REQUIRED_PROPERTY.exec(message) : null;
    if (missing?.[1] !== undefined) {
      path = `${path}/${escapePointer(missing[1])}`;
      message = "is required";
    }
    const key = `${path}\u0000${message}`;
    if (seen.has(key)) continue;
    seen.add(key);
    issues.push({ path, message });
  }
  return issues;
}
