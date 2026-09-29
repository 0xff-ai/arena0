import type { ReactNode } from "react";
import { Heading, Modal, ModalOverlay, Dialog as RACDialog } from "react-aria-components";
import { Button, IconButton } from "./button";
import { tv } from "./cx";
import { Icons } from "./icons";
import { fade, scrim } from "./internal";

const modal = tv({
  base: `max-h-[80vh] max-w-[calc(100vw-2rem)] overflow-hidden rounded-md border border-line bg-elev text-fg shadow-overlay outline-none ${fade}`,
  variants: {
    size: { sm: "w-90", md: "w-120", lg: "w-160" },
  },
  defaultVariants: { size: "md" },
});

// Both dialogs share one shell; only the ARIA role differs.
function Shell(props: {
  role: "dialog" | "alertdialog";
  title: string;
  isOpen: boolean;
  onOpenChange: (open: boolean) => void;
  children: ReactNode;
  footer?: ReactNode;
  size?: "sm" | "md" | "lg";
}) {
  return (
    <ModalOverlay
      isOpen={props.isOpen}
      onOpenChange={props.onOpenChange}
      isDismissable={props.role === "dialog"}
      className={scrim}
    >
      <Modal className={modal({ size: props.size })}>
        <RACDialog role={props.role} className="flex max-h-[80vh] flex-col outline-none">
          <div className="flex h-8 shrink-0 items-center gap-2 border-b border-line-soft pr-1 pl-3">
            <Heading slot="title" className="min-w-0 flex-1 truncate text-base font-medium">
              {props.title}
            </Heading>
            <IconButton slot="close" icon={Icons.close} label="Close" />
          </div>
          <div className="min-h-0 flex-1 overflow-auto p-3 text-base">{props.children}</div>
          {props.footer && (
            <div className="flex shrink-0 items-center justify-end gap-2 border-t border-line-soft p-2">
              {props.footer}
            </div>
          )}
        </RACDialog>
      </Modal>
    </ModalOverlay>
  );
}

export function Dialog(props: {
  title: string;
  isOpen: boolean;
  onOpenChange: (open: boolean) => void;
  children: ReactNode;
  footer?: ReactNode;
  size?: "sm" | "md" | "lg";
}) {
  return <Shell role="dialog" {...props} />;
}

/** Asks before a consequential action. Cancel has the initial focus, so Enter never confirms by accident. */
export function ConfirmDialog(props: {
  title: string;
  body: ReactNode;
  confirmLabel: string;
  danger?: boolean;
  isOpen: boolean;
  onOpenChange: (open: boolean) => void;
  onConfirm: () => void;
}) {
  return (
    <Shell
      role="alertdialog"
      size="sm"
      title={props.title}
      isOpen={props.isOpen}
      onOpenChange={props.onOpenChange}
      footer={
        <>
          <Button autoFocus onPress={() => props.onOpenChange(false)}>
            Cancel
          </Button>
          <Button
            variant={props.danger ? "danger" : "primary"}
            onPress={() => {
              props.onConfirm();
              props.onOpenChange(false);
            }}
          >
            {props.confirmLabel}
          </Button>
        </>
      }
    >
      {props.body}
    </Shell>
  );
}
