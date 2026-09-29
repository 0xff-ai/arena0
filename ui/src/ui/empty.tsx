import type { LucideIcon } from "lucide-react";
import type { ReactNode } from "react";
import { Icon } from "./internal";

/** What a list or document shows when it has nothing: an icon, a plain sentence, and at most one action. */
export function EmptyState(props: {
  icon: LucideIcon;
  title: string;
  body?: ReactNode;
  action?: ReactNode;
}) {
  return (
    <div className="flex flex-col items-center gap-2 px-5 py-10 text-center">
      <Icon icon={props.icon} size={28} strokeWidth={1} className="text-faint" />
      <div className="text-base text-muted">{props.title}</div>
      {props.body && <div className="max-w-80 text-sm text-subtle">{props.body}</div>}
      {props.action && <div className="mt-1">{props.action}</div>}
    </div>
  );
}
