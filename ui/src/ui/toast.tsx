import type { LucideIcon } from "lucide-react";
import {
  Text,
  UNSTABLE_Toast as Toast,
  UNSTABLE_ToastContent as ToastContent,
  UNSTABLE_ToastQueue as ToastQueue,
  UNSTABLE_ToastRegion as ToastRegion_,
} from "react-aria-components";
import type { Tone } from "./badge";
import { IconButton } from "./button";
import { cx } from "./cx";
import { Icons } from "./icons";
import { Icon, toneText } from "./internal";

interface ToastContentValue {
  tone: Tone;
  title: string;
  body?: string;
}

const DEFAULT_TIMEOUT_MS = 6000;

// One queue for the whole app: any code can raise a toast, and the single
// ToastRegion mounted by the shell shows it.
const queue = new ToastQueue<ToastContentValue>({ maxVisibleToasts: 5 });

export const toasts = {
  show(t: { tone: Tone; title: string; body?: string; timeoutMs?: number }): void {
    queue.add(
      { tone: t.tone, title: t.title, body: t.body },
      { timeout: t.timeoutMs ?? DEFAULT_TIMEOUT_MS },
    );
  },
};

const toneIcon: Record<Tone, LucideIcon> = {
  neutral: Icons.info,
  accent: Icons.info,
  ok: Icons.check,
  done: Icons.check,
  bad: Icons.error,
  warn: Icons.warn,
  info: Icons.info,
};

/** Bottom-right stack of toasts. Mount once. */
export function ToastRegion() {
  return (
    <ToastRegion_
      queue={queue}
      className="fixed right-3 bottom-3 z-60 flex w-80 flex-col-reverse gap-2 outline-none"
    >
      {({ toast }) => (
        <Toast
          toast={toast}
          className="flex items-start gap-2 rounded-md border border-line bg-elev py-2 pr-1 pl-3 text-base shadow-overlay outline-none focus-visible:outline-1 focus-visible:outline-accent"
        >
          <span className={cx("mt-0.5", toneText[toast.content.tone])}>
            <Icon icon={toneIcon[toast.content.tone]} size={15} />
          </span>
          <ToastContent className="flex min-w-0 flex-1 flex-col gap-0.5">
            <Text slot="title" className="font-medium text-fg">
              {toast.content.title}
            </Text>
            {toast.content.body && (
              <Text slot="description" className="text-sm break-words text-muted">
                {toast.content.body}
              </Text>
            )}
          </ToastContent>
          <IconButton slot="close" icon={Icons.close} label="Dismiss" />
        </Toast>
      )}
    </ToastRegion_>
  );
}
