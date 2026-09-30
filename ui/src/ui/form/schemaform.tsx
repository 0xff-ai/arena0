import { useState } from "react";
import { Button, IconButton, Segmented } from "../button";
import { cx } from "../cx";
import { FieldError, NumberField, Select, Switch, TextField } from "../field";
import { Icons } from "../icons";
import type { JsonLike } from "../viz/json";
import { JsonEditor } from "./jsoneditor";
import { escapePointer, type SchemaIssue, validate } from "./validate";

type JsonObject = { [k: string]: JsonLike };
type Change = (value: JsonLike | undefined) => void;

const isObject = (value: JsonLike | undefined): value is JsonObject =>
  typeof value === "object" && value !== null && !Array.isArray(value);

// Deeper than any real program schema; a recursive $ref stops here instead of forever.
const MAX_DEPTH = 8;
const SEGMENTED_MAX = 5;

interface Shared {
  root: JsonLike;
  issues: SchemaIssue[];
}

interface FieldProps {
  shared: Shared;
  schema: JsonLike | undefined;
  name: string;
  required: boolean;
  path: string;
  value: JsonLike | undefined;
  onChange: Change;
  depth: number;
  autoFocus?: boolean;
  /** The label is already given by a parent (a variant picker); keep an accessible name only. */
  hideLabel?: boolean;
}

const childPath = (path: string, key: string) => `${path}/${escapePointer(key)}`;

/** Follows local `$ref`s (`#/$defs/x`, any pointer into the root schema); siblings of the `$ref` win. */
function resolve(root: JsonLike, schema: JsonLike | undefined): JsonObject | null {
  if (!isObject(schema)) return null;
  let current = schema;
  for (let hops = 0; hops < MAX_DEPTH; hops++) {
    const ref = current.$ref;
    if (typeof ref !== "string") return current;
    const { $ref: _ref, ...siblings } = current;
    let target: JsonLike | undefined = root;
    if (ref.startsWith("#")) {
      for (const segment of ref.slice(1).split("/").slice(1)) {
        const key = decodeURIComponent(segment).replace(/~1/g, "/").replace(/~0/g, "~");
        target = isObject(target)
          ? target[key]
          : Array.isArray(target)
            ? target[Number(key)]
            : undefined;
      }
    } else target = undefined;
    current = { ...(isObject(target) ? target : {}), ...siblings };
  }
  return current;
}

/** A variant validated on its own still needs the root's definitions for its own `$ref`s. */
function standalone(root: JsonLike, variant: JsonObject): JsonObject {
  if (!isObject(root)) return variant;
  return { ...variant, $defs: root.$defs ?? {}, definitions: root.definitions ?? {} };
}

type Kind =
  | "enum"
  | "union"
  | "integer"
  | "number"
  | "string"
  | "boolean"
  | "null"
  | "object"
  | "array"
  | "raw";

function kindOf(node: JsonObject): Kind {
  if ((Array.isArray(node.enum) && node.enum.length > 0) || "const" in node) return "enum";
  if (Array.isArray(node.oneOf) || Array.isArray(node.anyOf)) return "union";
  const type = node.type;
  if (Array.isArray(type)) return type.length > 1 ? "union" : "raw";
  switch (type) {
    case "integer":
    case "number":
    case "string":
    case "boolean":
    case "null":
      return type;
    case "object":
      return isObject(node.properties) ? "object" : "raw";
    case "array":
      return isObject(node.items) ? "array" : "raw";
    default:
      return "raw";
  }
}

function enumValues(node: JsonObject): JsonLike[] {
  if (Array.isArray(node.enum)) return node.enum;
  return "const" in node && node.const !== undefined ? [node.const] : [];
}

function variantsOf(root: JsonLike, node: JsonObject): JsonObject[] {
  const alternatives = Array.isArray(node.oneOf) ? node.oneOf : node.anyOf;
  if (Array.isArray(alternatives)) {
    const { oneOf: _one, anyOf: _any, title: _title, description: _description, ...shared } = node;
    return alternatives.map((alternative) => ({
      ...shared,
      ...(resolve(root, alternative) ?? {}),
    }));
  }
  const { title: _title, description: _description, ...shared } = node;
  const types = Array.isArray(node.type) ? node.type : [];
  return types.map((type) => ({ ...shared, type }));
}

function defaultOf(root: JsonLike, node: JsonObject): JsonLike | undefined {
  if (node.default !== undefined) return node.default;
  if ("const" in node) return node.const;
  switch (kindOf(node)) {
    case "object": {
      const value: JsonObject = {};
      for (const [key, property] of Object.entries(
        isObject(node.properties) ? node.properties : {},
      )) {
        const fallback = resolve(root, property)?.default;
        if (fallback !== undefined) value[key] = fallback;
      }
      return value;
    }
    case "array":
      return [];
    case "null":
      return null;
    case "boolean":
      return false;
    default:
      return undefined;
  }
}

const label = (props: FieldProps, node: JsonObject) => {
  const text = typeof node.title === "string" ? node.title : props.name;
  return props.required ? `${text} *` : text;
};

const describe = (node: JsonObject, extras: string[] = []) =>
  [typeof node.description === "string" ? node.description : null, ...extras]
    .filter(Boolean)
    .join(" · ") || undefined;

/** Applies edited JSON text: empty text clears the value; text that does not parse keeps it and reports why. */
function applyText(text: string, setError: (error: string | undefined) => void, onChange: Change) {
  try {
    const parsed: JsonLike | undefined = text.trim() === "" ? undefined : JSON.parse(text);
    setError(undefined);
    onChange(parsed);
  } catch (failure) {
    setError((failure as SyntaxError).message);
  }
}

function Issues(props: { shared: Shared; path: string }) {
  const here = props.shared.issues.filter((issue) => issue.path === props.path);
  if (here.length === 0) return null;
  return (
    <div className="flex flex-col gap-0.5">
      {here.map((issue) => (
        <FieldError key={issue.message}>{issue.message}</FieldError>
      ))}
    </div>
  );
}

function GroupLabel(props: { text: string; description?: string }) {
  return (
    <div className="flex flex-col gap-0.5">
      <span className="text-sm text-muted">{props.text}</span>
      {props.description && <span className="text-xs text-subtle">{props.description}</span>}
    </div>
  );
}

function RawField(props: FieldProps) {
  const [text, setText] = useState(
    props.value === undefined ? "" : JSON.stringify(props.value, null, 2),
  );
  const [error, setError] = useState<string | undefined>();
  return (
    <div className="flex flex-col gap-1">
      <JsonEditor
        label={props.name}
        value={text}
        error={error}
        rows={4}
        onChange={(next) => {
          setText(next);
          applyText(next, setError, props.onChange);
        }}
      />
      <Issues shared={props.shared} path={props.path} />
    </div>
  );
}

function EnumField(props: FieldProps & { node: JsonObject }) {
  const values = enumValues(props.node);
  const items = values.map((value) => ({
    id: JSON.stringify(value),
    label: typeof value === "string" ? value : JSON.stringify(value),
  }));
  const chosen = props.value === undefined ? null : JSON.stringify(props.value);
  const pick = (id: string) => {
    const index = items.findIndex((item) => item.id === id);
    props.onChange(values[index]);
  };
  const description = describe(props.node);
  return (
    <div className="flex flex-col gap-1">
      {items.length <= SEGMENTED_MAX ? (
        <>
          {props.hideLabel ? (
            description && <span className="text-xs text-subtle">{description}</span>
          ) : (
            <GroupLabel text={label(props, props.node)} description={description} />
          )}
          <div>
            <Segmented
              label={props.name}
              size="md"
              items={items}
              value={chosen ?? ""}
              onChange={pick}
            />
          </div>
        </>
      ) : (
        <Select
          label={props.hideLabel ? undefined : label(props, props.node)}
          items={items}
          value={chosen}
          onChange={pick}
          placeholder="Choose…"
        />
      )}
      <Issues shared={props.shared} path={props.path} />
    </div>
  );
}

function UnionField(props: FieldProps & { node: JsonObject }) {
  const variants = variantsOf(props.shared.root, props.node);
  const [chosen, setChosen] = useState<number | null>(null);
  const fits = (index: number) => {
    const variant = variants[index];
    return (
      variant !== undefined &&
      validate(standalone(props.shared.root, variant), props.value).length === 0
    );
  };
  const matching = props.value === undefined ? -1 : variants.findIndex((_, i) => fits(i));
  const selected =
    chosen !== null && (props.value === undefined || fits(chosen))
      ? chosen
      : matching >= 0
        ? matching
        : null;
  const items = variants.map((variant, index) => ({
    id: String(index),
    label:
      typeof variant.title === "string"
        ? variant.title
        : "const" in variant
          ? JSON.stringify(variant.const)
          : typeof variant.type === "string"
            ? variant.type
            : `option ${index + 1}`,
  }));
  const variant = selected === null ? undefined : variants[selected];
  return (
    <div className="flex flex-col gap-1.5">
      <Select
        label={label(props, props.node)}
        items={items}
        value={selected === null ? null : String(selected)}
        placeholder="Choose a variant…"
        onChange={(id) => {
          const index = Number(id);
          const next = variants[index];
          setChosen(index);
          props.onChange(next ? defaultOf(props.shared.root, next) : undefined);
        }}
      />
      {variant && (
        <Field
          {...props}
          schema={variant}
          name={typeof variant.title === "string" ? variant.title : props.name}
          required={false}
          depth={props.depth + 1}
          hideLabel
        />
      )}
      {/* With a variant chosen, its own field shows the issues at this path. */}
      {!variant && <Issues shared={props.shared} path={props.path} />}
    </div>
  );
}

function ObjectField(props: FieldProps & { node: JsonObject }) {
  const properties = Object.entries(isObject(props.node.properties) ? props.node.properties : {});
  const required = Array.isArray(props.node.required) ? props.node.required : [];
  const current = isObject(props.value) ? props.value : {};
  const fields = properties.map(([key, schema], index) => (
    <Field
      key={key}
      shared={props.shared}
      schema={schema}
      name={key}
      required={required.includes(key)}
      path={childPath(props.path, key)}
      value={current[key]}
      depth={props.depth + 1}
      autoFocus={props.autoFocus && index === 0}
      onChange={(next) => {
        const { [key]: _removed, ...rest } = current;
        props.onChange(next === undefined ? rest : { ...rest, [key]: next });
      }}
    />
  ));
  if (props.depth === 0) {
    return (
      <div className="flex flex-col gap-3">
        {fields}
        <Issues shared={props.shared} path={props.path} />
      </div>
    );
  }
  return (
    <fieldset className="flex min-w-0 flex-col gap-3 rounded-sm border border-line-soft p-2.5">
      <legend className="px-1 text-sm text-muted">{label(props, props.node)}</legend>
      {describe(props.node) && (
        <span className="-mt-1 text-xs text-subtle">{describe(props.node)}</span>
      )}
      {fields}
      <Issues shared={props.shared} path={props.path} />
    </fieldset>
  );
}

function ArrayField(props: FieldProps & { node: JsonObject }) {
  const items = Array.isArray(props.value) ? props.value : [];
  const max =
    typeof props.node.maxItems === "number" ? props.node.maxItems : Number.POSITIVE_INFINITY;
  const itemSchema = props.node.items;
  const set = (next: JsonLike[]) => props.onChange(next);
  return (
    <div className="flex min-w-0 flex-col gap-1.5">
      <GroupLabel text={label(props, props.node)} description={describe(props.node)} />
      {items.map((item, index) => (
        // Rows have no identity beyond their position; removing one shifts the rest up.
        <div key={index} className="flex items-start gap-1.5">
          <div className="min-w-0 flex-1">
            <Field
              shared={props.shared}
              schema={itemSchema}
              name={`${props.name} ${index + 1}`}
              required={false}
              path={childPath(props.path, String(index))}
              value={item}
              depth={props.depth + 1}
              onChange={(next) =>
                set(items.map((existing, i) => (i === index ? (next ?? null) : existing)))
              }
            />
          </div>
          <IconButton
            icon={Icons.close}
            label={`Remove ${props.name} ${index + 1}`}
            onPress={() => set(items.filter((_, i) => i !== index))}
          />
        </div>
      ))}
      <div>
        <Button
          size="sm"
          icon={Icons.add}
          isDisabled={items.length >= max}
          onPress={() => {
            const template = resolve(props.shared.root, itemSchema);
            set([...items, (template && defaultOf(props.shared.root, template)) ?? null]);
          }}
        >
          Add item
        </Button>
      </div>
      <Issues shared={props.shared} path={props.path} />
    </div>
  );
}

function Field(props: FieldProps) {
  const { shared } = props;
  const node = resolve(shared.root, props.schema);
  if (!node || props.depth > MAX_DEPTH) return <RawField {...props} />;
  const kind = kindOf(node);
  const invalid = shared.issues.some((issue) => issue.path === props.path);

  switch (kind) {
    case "integer":
    case "number": {
      const limits = [
        kind === "integer" ? "integer" : null,
        typeof node.minimum === "number" ? `min ${node.minimum}` : null,
        typeof node.maximum === "number" ? `max ${node.maximum}` : null,
      ].filter((part): part is string => part !== null);
      return (
        <div className="flex flex-col gap-1">
          <NumberField
            label={props.hideLabel ? undefined : label(props, node)}
            aria-label={props.hideLabel ? props.name : undefined}
            description={describe(node, limits)}
            value={typeof props.value === "number" ? props.value : Number.NaN}
            onChange={(next) => props.onChange(Number.isNaN(next) ? undefined : next)}
            isInvalid={invalid}
            autoFocus={props.autoFocus}
            validationBehavior="aria"
            className="max-w-56"
          />
          <Issues shared={shared} path={props.path} />
        </div>
      );
    }
    case "string": {
      const pattern = typeof node.pattern === "string" ? [`pattern ${node.pattern}`] : [];
      return (
        <div className="flex flex-col gap-1">
          <TextField
            label={props.hideLabel ? undefined : label(props, node)}
            aria-label={props.hideLabel ? props.name : undefined}
            description={describe(node, pattern)}
            mono={pattern.length > 0}
            value={typeof props.value === "string" ? props.value : ""}
            onChange={(next) => props.onChange(next === "" ? undefined : next)}
            isInvalid={invalid}
            autoFocus={props.autoFocus}
            validationBehavior="aria"
          />
          <Issues shared={shared} path={props.path} />
        </div>
      );
    }
    case "boolean":
      return (
        <div className="flex flex-col gap-1">
          <Switch isSelected={props.value === true} onChange={props.onChange}>
            {label(props, node)}
          </Switch>
          <Issues shared={shared} path={props.path} />
        </div>
      );
    case "null":
      return (
        <div className="flex items-center gap-2 text-sm">
          <span className="text-muted">{label(props, node)}</span>
          <span className="font-mono text-subtle">null</span>
        </div>
      );
    case "enum":
      return <EnumField {...props} node={node} />;
    case "union":
      return <UnionField {...props} node={node} />;
    case "object":
      return <ObjectField {...props} node={node} />;
    case "array":
      return <ArrayField {...props} node={node} />;
    case "raw":
      return <RawField {...props} />;
  }
}

/**
 * A form generated from a JSON Schema. It edits `value` through `onChange`
 * and shows `issues` (from `validate`) under the field their path points at.
 * `Raw JSON` swaps the form for a text editor over the same value; text that
 * does not parse leaves the value unchanged and shows the parse error.
 */
export function SchemaForm(props: {
  schema: JsonLike;
  value: JsonLike | undefined;
  onChange: (value: JsonLike | undefined) => void;
  issues?: SchemaIssue[];
  disabled?: boolean;
  autoFocus?: boolean;
}) {
  const [raw, setRaw] = useState(false);
  const [text, setText] = useState("");
  const [parseError, setParseError] = useState<string | undefined>();
  const issues = props.issues ?? [];
  const shared: Shared = { root: props.schema, issues };

  return (
    // `inert` blocks pointer and keyboard use of every control at once, including
    // ones (Segmented, Select) that have no disabled state of their own.
    <div
      inert={props.disabled}
      className={cx("flex min-w-0 flex-col gap-3", props.disabled && "opacity-60")}
    >
      {raw ? (
        <>
          <JsonEditor
            label="Raw JSON"
            value={text}
            error={parseError}
            onChange={(next) => {
              setText(next);
              applyText(next, setParseError, props.onChange);
            }}
          />
          {issues.map((issue) => (
            <FieldError key={`${issue.path}\u0000${issue.message}`}>
              {issue.path === "" ? issue.message : `${issue.path} ${issue.message}`}
            </FieldError>
          ))}
        </>
      ) : (
        <Field
          shared={shared}
          schema={props.schema}
          name="value"
          required={false}
          path=""
          value={props.value}
          onChange={props.onChange}
          depth={0}
          autoFocus={props.autoFocus}
          hideLabel
        />
      )}
      <Switch
        isSelected={raw}
        onChange={(next) => {
          if (next) {
            setText(props.value === undefined ? "" : JSON.stringify(props.value, null, 2));
            setParseError(undefined);
          }
          setRaw(next);
        }}
      >
        Raw JSON
      </Switch>
    </div>
  );
}
