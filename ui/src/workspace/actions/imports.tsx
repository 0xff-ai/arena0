import { Button, IconButton, Icons } from "~/ui";

// Import and Host actions shared by the Explorer headers (compact) and the
// list toolbars. W2 implements them; until then they render disabled.

/** `.wasm` file picker → `program_import` on every Host; toast with the hash. */
export function ImportProgramButton(props: { compact?: boolean }) {
  return props.compact ? (
    <IconButton icon={Icons.upload} label="Import program" isDisabled />
  ) : (
    <Button icon={Icons.upload} isDisabled>
      Import program
    </Button>
  );
}

/** Receipt JSON file → Host `Select` in a Dialog → `receipt_import`. */
export function ImportReceiptButton(props: { compact?: boolean }) {
  return props.compact ? (
    <IconButton icon={Icons.upload} label="Import receipt" isDisabled />
  ) : (
    <Button icon={Icons.upload} isDisabled>
      Import receipt
    </Button>
  );
}

/** Dialog with optional id and user agent → `host_open`. */
export function AddHostButton(props: { compact?: boolean }) {
  return props.compact ? (
    <IconButton icon={Icons.add} label="Add Host" isDisabled />
  ) : (
    <Button icon={Icons.add} isDisabled>
      Add Host
    </Button>
  );
}
