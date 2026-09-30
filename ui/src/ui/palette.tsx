import type { LucideIcon } from "lucide-react";
import {
  Autocomplete,
  Dialog,
  Header,
  Input,
  Menu,
  MenuItem,
  MenuSection,
  Modal,
  ModalOverlay,
  SearchField,
  useFilter,
} from "react-aria-components";
import { Icons } from "./icons";
import { fade, Icon, scrim } from "./internal";
import { Kbd } from "./kbd";

export interface PaletteItem {
  id: string;
  label: string;
  detail?: string;
  icon?: LucideIcon;
  kbd?: string;
  keywords?: string[];
  onAction: () => void;
}

/** Find-anything overlay: type to filter every item's label, detail and keywords; Enter runs the highlighted item and closes. */
export function CommandPalette(props: {
  isOpen: boolean;
  onOpenChange: (open: boolean) => void;
  groups: { id: string; title: string; items: PaletteItem[] }[];
  placeholder?: string;
}) {
  const { contains } = useFilter({ sensitivity: "base" });
  const placeholder = props.placeholder ?? "Type a command or search…";
  return (
    <ModalOverlay
      isOpen={props.isOpen}
      onOpenChange={props.onOpenChange}
      isDismissable
      className={scrim}
    >
      <Modal
        className={`w-140 max-w-[94vw] overflow-hidden rounded-md border border-line bg-elev shadow-overlay outline-none ${fade}`}
      >
        <Dialog aria-label="Command palette" className="flex max-h-[60vh] flex-col outline-none">
          {/* Esc closes at once; left alone, the search field would first clear its text. */}
          <div
            className="flex min-h-0 flex-1 flex-col"
            onKeyDownCapture={(event) => {
              if (event.key !== "Escape") return;
              event.stopPropagation();
              props.onOpenChange(false);
            }}
          >
            <Autocomplete filter={(textValue, inputValue) => contains(textValue, inputValue)}>
              <SearchField
                aria-label={placeholder}
                autoFocus
                className="flex h-10 shrink-0 items-center gap-2 border-b border-line-soft px-3"
              >
                <Icon icon={Icons.search} size={15} className="text-subtle" />
                <Input
                  placeholder={placeholder}
                  className="min-w-0 flex-1 bg-transparent text-md text-fg outline-none placeholder:text-faint [&::-webkit-search-cancel-button]:hidden"
                />
              </SearchField>
              <Menu
                aria-label="Results"
                className="min-h-0 flex-1 overflow-auto p-1 outline-none"
                renderEmptyState={() => (
                  <div className="px-3 py-6 text-center text-sm text-subtle">No matches</div>
                )}
              >
                {props.groups.map((group) => (
                  <MenuSection
                    key={group.id}
                    className="not-first:mt-1 not-first:border-t not-first:border-line-soft not-first:pt-1"
                  >
                    <Header className="px-2.5 pt-1 pb-0.5 text-xs tracking-wide text-subtle uppercase">
                      {group.title}
                    </Header>
                    {group.items.map((item) => (
                      <MenuItem
                        key={item.id}
                        id={item.id}
                        textValue={[item.label, item.detail, ...(item.keywords ?? [])].join(" ")}
                        onAction={() => {
                          item.onAction();
                          props.onOpenChange(false);
                        }}
                        className="flex h-7.5 cursor-default items-center gap-2 rounded-sm px-2.5 text-base text-fg outline-none focused:bg-selected focus-visible:bg-selected"
                      >
                        {item.icon && <Icon icon={item.icon} className="text-subtle" />}
                        <span className="shrink-0">{item.label}</span>
                        <span className="min-w-0 flex-1 truncate text-sm text-subtle">
                          {item.detail}
                        </span>
                        {item.kbd && <Kbd keys={item.kbd} />}
                      </MenuItem>
                    ))}
                  </MenuSection>
                ))}
              </Menu>
            </Autocomplete>
          </div>
          <div className="flex h-7 shrink-0 items-center gap-3 border-t border-line-soft px-3 text-xs text-subtle">
            <span className="flex items-center gap-1">
              <Kbd keys="Up" />
              <Kbd keys="Down" />
              move
            </span>
            <span className="flex items-center gap-1">
              <Kbd keys="Enter" />
              run
            </span>
            <span className="flex items-center gap-1">
              <Kbd keys="Esc" />
              close
            </span>
          </div>
        </Dialog>
      </Modal>
    </ModalOverlay>
  );
}
