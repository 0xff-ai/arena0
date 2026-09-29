import { useEffect, useRef, useState } from "react";
import { Button } from "react-aria-components";
import { IconButton } from "../button";
import { Icons } from "../icons";
import { Tooltip } from "../tooltip";
import { Fingerprint } from "./fingerprint";

const COPIED_MS = 1200;

/** A hash as its first `chars` digits and a fingerprint; the full hash is in the tooltip and behind the copy button. */
export function HashChip(props: { hash: string; chars?: number; copy?: boolean; title?: string }) {
  const [copied, setCopied] = useState(false);
  const timer = useRef<number | undefined>(undefined);
  useEffect(() => () => window.clearTimeout(timer.current), []);

  function copy() {
    // The clipboard is a browser permission boundary; a refusal just leaves the icon unchanged.
    navigator.clipboard.writeText(props.hash).then(
      () => {
        setCopied(true);
        window.clearTimeout(timer.current);
        timer.current = window.setTimeout(() => setCopied(false), COPIED_MS);
      },
      () => {},
    );
  }

  return (
    <span className="inline-flex items-center gap-1 font-mono text-sm whitespace-nowrap text-muted">
      <Tooltip
        title={props.title ?? props.hash}
        body={props.title ? <span className="font-mono">{props.hash}</span> : undefined}
      >
        {/* A real button rather than `Focusable`: inside virtualized rows the latter
            mounts while hidden and logs a warning. Pressing it does nothing. */}
        <Button className="inline-flex cursor-default items-center gap-1.5 rounded-xs font-mono outline-none focus-visible:outline-1 focus-visible:outline-offset-1 focus-visible:outline-accent">
          {/* The digits beside it already name the hash. */}
          <span aria-hidden className="inline-flex">
            <Fingerprint hash={props.hash} size="sm" />
          </span>
          {props.hash.slice(0, props.chars ?? 8)}
        </Button>
      </Tooltip>
      {props.copy && (
        <IconButton
          icon={copied ? Icons.check : Icons.copy}
          label={copied ? "Copied" : "Copy hash"}
          onPress={copy}
          className="size-4"
        />
      )}
    </span>
  );
}
