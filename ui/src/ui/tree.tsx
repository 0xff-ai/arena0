import type { LucideIcon } from "lucide-react";
import type { ReactNode } from "react";
import { Button, Tree as RACTree, TreeItem, TreeItemContent } from "react-aria-components";
import type { Tone } from "./badge";
import { Icons } from "./icons";
import { Icon, isParticipantTone, participantText, toneText } from "./internal";
import type { ParticipantToneName } from "./status";

export interface TreeNode {
  id: string;
  label: ReactNode;
  icon?: LucideIcon;
  iconTone?: Tone | ParticipantToneName;
  trailing?: ReactNode;
  children?: TreeNode[];
  title?: string;
}

const INDENT_PX = 12;
const GUTTER_PX = 4;

function renderNode(node: TreeNode): ReactNode {
  const tone = node.iconTone;
  return (
    <TreeItem
      key={node.id}
      id={node.id}
      textValue={node.title ?? (typeof node.label === "string" ? node.label : node.id)}
      className="relative flex cursor-default items-center text-base text-fg outline-none hovered:bg-hover selected:bg-selected focus-visible:outline-1 focus-visible:-outline-offset-1 focus-visible:outline-accent"
    >
      <TreeItemContent>
        {({ level, hasChildItems, isExpanded }) => (
          <div
            title={node.title}
            className="flex h-6 min-w-0 flex-1 items-center gap-1 pr-2"
            style={{ paddingInlineStart: GUTTER_PX + (level - 1) * INDENT_PX }}
          >
            {hasChildItems ? (
              <Button
                slot="chevron"
                className="flex size-4 shrink-0 cursor-default items-center justify-center rounded-xs text-subtle outline-none hovered:text-fg"
              >
                <Icon icon={isExpanded ? Icons.chevronDown : Icons.chevronRight} size={12} />
              </Button>
            ) : (
              <span className="size-4 shrink-0" />
            )}
            {node.icon && (
              <Icon
                icon={node.icon}
                className={
                  tone
                    ? isParticipantTone(tone)
                      ? participantText[tone]
                      : toneText[tone]
                    : "text-subtle"
                }
              />
            )}
            <span className="min-w-0 flex-1 truncate">{node.label}</span>
            {node.trailing && (
              <span className="flex shrink-0 items-center gap-1.5 text-muted">{node.trailing}</span>
            )}
          </div>
        )}
      </TreeItemContent>
      {node.children?.map(renderNode)}
    </TreeItem>
  );
}

/** A tree with 24 px rows. Click selects, Enter or double-click runs `onAction`, arrows expand and collapse. */
export function Tree(props: {
  label: string;
  items: TreeNode[];
  selectedId?: string | null;
  onSelect?: (id: string) => void;
  onAction?: (id: string) => void;
  defaultExpanded?: string[];
}) {
  return (
    <RACTree
      aria-label={props.label}
      selectionMode="single"
      selectionBehavior="replace"
      selectedKeys={props.selectedId ? [props.selectedId] : []}
      onSelectionChange={(keys) => {
        if (keys === "all") return;
        const [id] = keys;
        if (typeof id === "string") props.onSelect?.(id);
      }}
      onAction={(key) => {
        if (typeof key === "string") props.onAction?.(key);
      }}
      defaultExpandedKeys={props.defaultExpanded}
      className="flex flex-col outline-none"
    >
      {props.items.map(renderNode)}
    </RACTree>
  );
}
