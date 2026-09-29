import { Label, TextArea, TextField } from "react-aria-components";
import { FieldError } from "../field";

/** A plain-text JSON editor. It reports every edit as text and leaves parsing, and what to do with a parse error, to the caller. */
export function JsonEditor(props: {
  value: string;
  onChange: (text: string) => void;
  error?: string;
  label: string;
  rows?: number;
}) {
  return (
    <TextField
      value={props.value}
      onChange={props.onChange}
      isInvalid={props.error !== undefined}
      className="flex min-w-0 flex-col gap-1"
    >
      <Label className="text-sm text-muted">{props.label}</Label>
      <TextArea
        rows={props.rows ?? 6}
        spellCheck={false}
        className="w-full min-w-0 resize-y rounded-sm border border-line bg-editor px-2 py-1.5 font-mono text-sm text-fg outline-none transition-colors duration-120 placeholder:text-faint hovered:border-faint focused:border-accent invalid:border-bad disabled:opacity-50"
      />
      {props.error !== undefined && <FieldError>{props.error}</FieldError>}
    </TextField>
  );
}
