import { Minus } from "lucide-react";
import type { ReactNode } from "react";
import {
  composeRenderProps,
  Group,
  Input,
  Label,
  ListBox,
  ListBoxItem,
  Button as RACButton,
  Checkbox as RACCheckbox,
  type CheckboxProps as RACCheckboxProps,
  FieldError as RACFieldError,
  NumberField as RACNumberField,
  type NumberFieldProps as RACNumberFieldProps,
  SearchField as RACSearchField,
  type SearchFieldProps as RACSearchFieldProps,
  Select as RACSelect,
  Switch as RACSwitch,
  type SwitchProps as RACSwitchProps,
  TextField as RACTextField,
  type TextFieldProps as RACTextFieldProps,
  SelectValue,
  Text,
} from "react-aria-components";
import { cx, tv } from "./cx";
import { Icons } from "./icons";
import { Icon } from "./internal";
import { Kbd } from "./kbd";
import { Popover } from "./popover";

const labelClass = "text-sm text-muted";
const descriptionClass = "text-xs text-subtle";
const control =
  "rounded-sm border border-line bg-editor text-fg outline-none transition-colors duration-120 hovered:border-faint focused:border-accent invalid:border-bad disabled:opacity-50";

const input = tv({
  base: `h-7 w-full min-w-0 px-2 placeholder:text-faint ${control}`,
  variants: { mono: { true: "font-mono text-sm", false: "text-base" } },
  defaultVariants: { mono: false },
});

type TextFieldProps = Omit<RACTextFieldProps, "children"> & {
  label?: string;
  description?: string;
  mono?: boolean;
  placeholder?: string;
};

/** Callers without a visible `label` must pass `aria-label`. */
export function TextField({
  label: text,
  description: help,
  mono,
  placeholder,
  ...props
}: TextFieldProps) {
  return (
    <RACTextField
      {...props}
      className={composeRenderProps(props.className, (className) =>
        cx("flex min-w-0 flex-col gap-1", className),
      )}
    >
      {text && <Label className={labelClass}>{text}</Label>}
      <Input placeholder={placeholder} className={input({ mono })} />
      {help && (
        <Text slot="description" className={descriptionClass}>
          {help}
        </Text>
      )}
      <RACFieldError className="text-xs text-bad" />
    </RACTextField>
  );
}

export function SearchField(
  props: RACSearchFieldProps & { label: string; placeholder?: string; kbd?: string },
) {
  const { label: name, placeholder, kbd, ...rest } = props;
  return (
    <RACSearchField
      {...rest}
      aria-label={name}
      className={composeRenderProps(props.className, (className) =>
        cx(
          "group flex h-6 min-w-0 items-center gap-1.5 rounded-sm border border-line-soft bg-editor px-1.5 transition-colors duration-120 focus-within:border-accent",
          className,
        ),
      )}
    >
      <Icon icon={Icons.search} size={13} className="text-subtle" />
      <Input
        placeholder={placeholder}
        className="min-w-0 flex-1 bg-transparent text-sm text-fg outline-none placeholder:text-faint [&::-webkit-search-cancel-button]:hidden"
      />
      <RACButton
        aria-label="Clear"
        className="flex size-4 cursor-default items-center justify-center rounded-xs text-subtle outline-none hovered:bg-hover hovered:text-fg group-data-empty:hidden"
      >
        <Icon icon={Icons.close} size={12} />
      </RACButton>
      {kbd && (
        <span className="group-focus-within:hidden group-not-data-empty:hidden">
          <Kbd keys={kbd} />
        </span>
      )}
    </RACSearchField>
  );
}

const stepper =
  "flex size-6 cursor-default items-center justify-center rounded-xs text-subtle outline-none hovered:bg-hover hovered:text-fg pressed:bg-active disabled:opacity-50";

type NumberFieldProps = Omit<RACNumberFieldProps, "children"> & {
  label?: string;
  description?: string;
};

/** Values are never clamped to min/max: out-of-range input stays visible so the schema can report it. */
export function NumberField({ label: text, description: help, ...props }: NumberFieldProps) {
  return (
    <RACNumberField
      formatOptions={{ useGrouping: false, maximumFractionDigits: 20 }}
      {...props}
      className={composeRenderProps(props.className, (className) =>
        cx("flex min-w-0 flex-col gap-1", className),
      )}
    >
      {text && <Label className={labelClass}>{text}</Label>}
      <Group
        className={`flex h-7 items-center gap-px pr-0.5 focus-within:border-accent ${control}`}
      >
        <Input className="h-full min-w-0 flex-1 bg-transparent px-2 font-mono text-sm tabular text-fg outline-none placeholder:text-faint" />
        <RACButton slot="decrement" className={stepper}>
          <Icon icon={Minus} size={12} />
        </RACButton>
        <RACButton slot="increment" className={stepper}>
          <Icon icon={Icons.add} size={12} />
        </RACButton>
      </Group>
      {help && (
        <Text slot="description" className={descriptionClass}>
          {help}
        </Text>
      )}
      <RACFieldError className="text-xs text-bad" />
    </RACNumberField>
  );
}

export function Checkbox(props: Omit<RACCheckboxProps, "children"> & { children: ReactNode }) {
  const { children, ...rest } = props;
  return (
    <RACCheckbox
      {...rest}
      className={composeRenderProps(props.className, (className) =>
        cx(
          "group flex cursor-default items-center gap-2 text-base text-fg outline-none disabled:opacity-50",
          className,
        ),
      )}
    >
      {({ isSelected, isIndeterminate }) => (
        <>
          <span
            className={cx(
              "flex size-3.5 shrink-0 items-center justify-center rounded-xs border border-line bg-editor text-editor transition-colors duration-120 group-hovered:border-faint group-focus-visible:outline-1 group-focus-visible:outline-offset-1 group-focus-visible:outline-accent",
              (isSelected || isIndeterminate) &&
                "border-accent bg-accent group-hovered:border-accent",
            )}
          >
            {isIndeterminate ? (
              <Icon icon={Minus} size={10} />
            ) : (
              isSelected && <Icon icon={Icons.check} size={11} />
            )}
          </span>
          {children}
        </>
      )}
    </RACCheckbox>
  );
}

/** A squared toggle; round shapes are reserved for dots. */
export function Switch(props: Omit<RACSwitchProps, "children"> & { children: ReactNode }) {
  const { children, ...rest } = props;
  return (
    <RACSwitch
      {...rest}
      className={composeRenderProps(props.className, (className) =>
        cx(
          "group flex cursor-default items-center gap-2 text-base text-fg outline-none disabled:opacity-50",
          className,
        ),
      )}
    >
      <span className="flex h-4 w-7 shrink-0 items-center rounded-sm border border-line bg-surface px-px transition-colors duration-120 group-hovered:border-faint group-selected:border-accent group-selected:bg-accent group-focus-visible:outline-1 group-focus-visible:outline-offset-1 group-focus-visible:outline-accent">
        <span className="size-2.5 rounded-xs bg-faint transition-transform duration-120 group-selected:translate-x-3 group-selected:bg-editor" />
      </span>
      {children}
    </RACSwitch>
  );
}

export function Select<K extends string>(props: {
  label?: string;
  items: { id: K; label: string; detail?: string }[];
  value: K | null;
  onChange: (id: K) => void;
  placeholder?: string;
}) {
  return (
    <RACSelect
      selectedKey={props.value}
      onSelectionChange={(key) => {
        const item = props.items.find((candidate) => candidate.id === key);
        if (item) props.onChange(item.id);
      }}
      placeholder={props.placeholder}
      aria-label={props.label ? undefined : (props.placeholder ?? "Select")}
      className="flex min-w-0 flex-col gap-1"
    >
      {props.label && <Label className={labelClass}>{props.label}</Label>}
      <RACButton
        className={`flex h-7 w-full min-w-0 cursor-default items-center gap-2 px-2 text-base ${control}`}
      >
        <SelectValue className="min-w-0 flex-1 truncate text-left data-placeholder:text-faint">
          {({ isPlaceholder, selectedText, defaultChildren }) =>
            isPlaceholder ? defaultChildren : selectedText
          }
        </SelectValue>
        <Icon icon={Icons.chevronDown} size={13} className="text-subtle" />
      </RACButton>
      <Popover className="min-w-(--trigger-width)">
        <ListBox items={props.items} className="max-h-72 overflow-auto p-1 outline-none">
          {(item) => (
            <ListBoxItem
              id={item.id}
              textValue={item.label}
              className="flex h-6 cursor-default items-center gap-2 rounded-sm px-2 text-base text-fg outline-none focused:bg-selected"
            >
              {({ isSelected }) => (
                <>
                  <span className="min-w-0 flex-1 truncate">{item.label}</span>
                  {item.detail && (
                    <span className="truncate font-mono text-xs text-subtle">{item.detail}</span>
                  )}
                  <span className="w-3 shrink-0">
                    {isSelected && <Icon icon={Icons.check} size={12} className="text-accent" />}
                  </span>
                </>
              )}
            </ListBoxItem>
          )}
        </ListBox>
      </Popover>
    </RACSelect>
  );
}

/** An error line under a field. Independent of the field's own validation, so callers can show schema issues. */
export function FieldError(props: { children: ReactNode }) {
  return (
    <div role="alert" className="flex items-start gap-1 text-xs text-bad">
      <Icon icon={Icons.error} size={12} className="mt-0.5" />
      <span className="min-w-0 break-words">{props.children}</span>
    </div>
  );
}
