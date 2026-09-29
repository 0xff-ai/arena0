import type { LucideIcon } from "lucide-react";
import { Tab, TabList, Tabs } from "react-aria-components";
import type { Tone } from "./badge";
import { cx } from "./cx";
import { Icons } from "./icons";
import { Icon, toneText } from "./internal";
import { Count } from "./status";

const subTab =
  "relative flex h-full cursor-default items-center gap-1.5 px-2.5 text-sm whitespace-nowrap text-muted outline-none transition-colors duration-120 hovered:text-fg selected:text-fg focus-visible:outline-1 focus-visible:-outline-offset-1 focus-visible:outline-accent after:absolute after:inset-x-1 after:-bottom-px after:h-0.5 after:bg-transparent selected:after:bg-accent";

/** Underlined tabs that switch the sub-view of one document. */
export function SubTabs<K extends string>(props: {
  label: string;
  items: { id: K; label: string; icon?: LucideIcon; count?: number }[];
  value: K;
  onChange: (id: K) => void;
}) {
  return (
    <Tabs
      selectedKey={props.value}
      onSelectionChange={(key) => {
        const item = props.items.find((candidate) => candidate.id === key);
        if (item) props.onChange(item.id);
      }}
    >
      <TabList
        aria-label={props.label}
        className="flex h-7 shrink-0 border-b border-line-soft px-1.5"
      >
        {props.items.map((item) => (
          <Tab key={item.id} id={item.id} className={subTab}>
            {item.icon && <Icon icon={item.icon} size={12} />}
            {item.label}
            {item.count !== undefined && <Count value={item.count} />}
          </Tab>
        ))}
      </TabList>
    </Tabs>
  );
}

export interface DocTab {
  id: string;
  label: string;
  icon: LucideIcon;
  preview?: boolean;
  fixed?: boolean;
  detail?: string;
  tone?: Tone;
}

const stop = (event: { stopPropagation: () => void }) => event.stopPropagation();

/**
 * The strip of open documents. Selection and arrow-key navigation come from
 * the tab list. Closing is a pointer affordance only: the close icon is
 * hidden from assistive tech and keyboard users close the active tab with
 * `Mod+W`, which the workspace handles. When `activeId` is null the tab list
 * selects the first tab itself and reports it through `onSelect`.
 */
export function DocTabs(props: {
  tabs: DocTab[];
  activeId: string | null;
  onSelect: (id: string) => void;
  onClose: (id: string) => void;
  onPin: (id: string) => void;
}) {
  return (
    <Tabs
      selectedKey={props.activeId ?? ""}
      onSelectionChange={(key) => {
        const tab = props.tabs.find((candidate) => candidate.id === key);
        if (tab && tab.id !== props.activeId) props.onSelect(tab.id);
      }}
      className="min-w-0"
    >
      <TabList
        aria-label="Open documents"
        className="flex h-8.5 overflow-x-auto border-b border-line bg-surface [scrollbar-width:none]"
      >
        {props.tabs.map((tab) => (
          <Tab
            key={tab.id}
            id={tab.id}
            data-preview={tab.preview || undefined}
            onDoubleClick={() => props.onPin(tab.id)}
            onMouseDown={(event) => {
              // Middle press would start the browser's autoscroll.
              if (event.button === 1) event.preventDefault();
            }}
            onAuxClick={(event) => {
              if (event.button === 1 && !tab.fixed) props.onClose(tab.id);
            }}
            className="group relative flex h-full shrink-0 cursor-default items-center gap-1.5 border-t border-r border-t-transparent border-r-line bg-surface px-3 text-base whitespace-nowrap text-muted outline-none transition-colors duration-120 hovered:text-fg selected:border-t-accent selected:bg-editor selected:text-fg focus-visible:outline-1 focus-visible:-outline-offset-1 focus-visible:outline-accent selected:after:absolute selected:after:inset-x-0 selected:after:-bottom-px selected:after:h-px selected:after:bg-editor"
          >
            <Icon icon={tab.icon} className={cx(tab.tone && toneText[tab.tone])} />
            <span className={cx("max-w-48 truncate", tab.preview && "italic")}>{tab.label}</span>
            {tab.detail && (
              <span className="max-w-24 truncate font-mono text-xs text-subtle">{tab.detail}</span>
            )}
            {!tab.fixed && (
              <span
                aria-hidden
                onPointerDown={stop}
                onMouseDown={stop}
                onClick={(event) => {
                  event.stopPropagation();
                  props.onClose(tab.id);
                }}
                className="flex size-4 items-center justify-center rounded-xs text-subtle opacity-0 hover:bg-hover hover:text-fg group-hovered:opacity-100 group-selected:opacity-100"
              >
                <Icon icon={Icons.close} size={12} />
              </span>
            )}
          </Tab>
        ))}
      </TabList>
    </Tabs>
  );
}
