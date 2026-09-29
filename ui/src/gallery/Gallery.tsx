// The component gallery: every public piece of `ui/`, in every state, with
// arena0-shaped sample data. Dev only (route /gallery). The Playwright suite
// screenshots each <section> in both themes, so the ids below are contract.
import { type ReactNode, useEffect, useState } from "react";
import { DialogTrigger, Heading, Dialog as RACDialog } from "react-aria-components";
import {
  AgreementMeter,
  AnsiText,
  Badge,
  Button,
  Checkbox,
  CommandPalette,
  ConfirmDialog,
  Count,
  cx,
  DataTable,
  Dialog,
  DockHeader,
  type DocTab,
  DocTabs,
  Dot,
  EmptyState,
  FieldError,
  Fingerprint,
  HashChip,
  Hint,
  IconButton,
  Icons,
  type JsonLike,
  JsonView,
  Kbd,
  KeyValue,
  LifecycleStrip,
  List,
  MenuButton,
  MenuItem,
  MenuSection,
  MenuSeparator,
  NumberField,
  type PaletteItem,
  Panel,
  PanelGroup,
  PanelHandle,
  ParticipantChip,
  ParticipantDot,
  Popover,
  SchemaForm,
  SearchField,
  Section,
  Segmented,
  Select,
  Slider,
  Sparkline,
  type StripMark,
  type StripSpan,
  SubTabs,
  Switch,
  TextField,
  type TimeBucket,
  TimelineChart,
  ToastRegion,
  Toggle,
  type Tone,
  Toolbar,
  ToolbarSeparator,
  Tooltip,
  Tree,
  type TreeNode,
  toasts,
  validate,
} from "../ui";

// ---------------------------------------------------------------- sample data

// Fixed clock so the screenshots do not change from run to run.
const NOW = Date.UTC(2026, 8, 29, 14, 32, 0);
const MIN = 60_000;

/** A deterministic 64-hex-digit hash, standing in for a session, program or receipt id. */
function hex64(seed: number): string {
  let state = seed >>> 0;
  let out = "";
  for (let i = 0; i < 64; i++) {
    state = (Math.imul(state, 1664525) + 1013904223) >>> 0;
    out += "0123456789abcdef".charAt((state >>> 24) & 15);
  }
  return out;
}

const HASH_A = hex64(1);
const HASH_B = hex64(2);
const HASH_C = hex64(3);

const PROGRAMS = [
  "vickrey-auction",
  "chess",
  "prisoner-dilemma",
  "contract-net",
  "rock-paper-scissors",
];
const STATES = ["active", "active", "completed", "aborted", "negotiating", "failed"] as const;
type SessionState = (typeof STATES)[number];

interface SessionSample {
  key: string;
  program: string;
  hash: string;
  state: SessionState;
  step: number;
  ageMs: number;
  participants: number;
}

const SESSIONS: SessionSample[] = Array.from({ length: 400 }, (_, i) => ({
  key: `s${i}`,
  program: PROGRAMS[i % PROGRAMS.length] ?? "chess",
  hash: hex64(100 + i),
  state: STATES[i % STATES.length] ?? "active",
  step: (i * 7) % 43,
  ageMs: (i * 37 + 20) * 1000,
  participants: 2 + (i % 4),
}));

function fmtAge(ms: number): string {
  const s = Math.round(ms / 1000);
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  return m < 60
    ? `${m}m ${String(s % 60).padStart(2, "0")}s`
    : `${Math.floor(m / 60)}h ${String(m % 60).padStart(2, "0")}m`;
}

const stateTone: Record<SessionState, Tone> = {
  active: "ok",
  completed: "done",
  aborted: "bad",
  negotiating: "neutral",
  failed: "bad",
};
const stateIcon = {
  active: Icons.play,
  completed: Icons.check,
  aborted: Icons.stop,
  negotiating: Icons.negotiation,
  failed: Icons.error,
} as const;

function StateBadge(props: { state: SessionState }) {
  return (
    <Badge tone={stateTone[props.state]} icon={stateIcon[props.state]}>
      {props.state}
    </Badge>
  );
}

function Dots(props: { count: number; you?: number }) {
  return (
    <span className="inline-flex w-[4.5rem] shrink-0 items-center gap-1">
      {Array.from({ length: props.count }, (_, i) => (
        <ParticipantDot key={i} index={i} you={props.you === i} />
      ))}
    </span>
  );
}

// ---------------------------------------------------------------- layout helpers

function Sec(props: { id: string; title: string; note?: string; children: ReactNode }) {
  return (
    <section id={props.id} className="scroll-mt-12 border-b border-line-soft px-6 py-6">
      <h2 className="text-md font-semibold text-fg">{props.title}</h2>
      {props.note && <p className="mt-0.5 max-w-3xl text-sm text-muted">{props.note}</p>}
      <div className="mt-4 flex flex-col gap-5">{props.children}</div>
    </section>
  );
}

function Demo(props: { title: string; children: ReactNode; className?: string }) {
  return (
    <div className="min-w-0">
      <h3 className="mb-2 text-xs tracking-wide text-subtle uppercase">{props.title}</h3>
      <div className={cx("flex flex-wrap items-center gap-3", props.className)}>
        {props.children}
      </div>
    </div>
  );
}

function Frame(props: { className?: string; children: ReactNode }) {
  return (
    <div className={cx("overflow-hidden border border-line bg-surface", props.className)}>
      {props.children}
    </div>
  );
}

function Readout(props: { label: string; value: string }) {
  return (
    <span className="font-mono text-sm text-subtle">
      {props.label}: <span className="text-fg">{props.value}</span>
    </span>
  );
}

// ---------------------------------------------------------------- sections

function ButtonSection() {
  const [view, setView] = useState<"list" | "host">("list");
  const [size, setSize] = useState<"sm" | "md">("md");
  const [step, setStep] = useState(7);
  const [latest, setLatest] = useState(false);
  return (
    <Sec
      id="button"
      title="Button, IconButton, Segmented, Toggle, Slider"
      note="Default, primary, ghost and danger at 24 px (sm) and 28 px (md). Every icon-only button carries a tooltip."
    >
      {(["md", "sm"] as const).map((s) => (
        <Demo key={s} title={`Buttons · ${s}`}>
          <Button size={s}>Default</Button>
          <Button size={s} variant="primary">
            Primary
          </Button>
          <Button size={s} variant="ghost">
            Ghost
          </Button>
          <Button size={s} variant="danger">
            Danger
          </Button>
          <Button size={s} icon={Icons.add} variant="primary" kbd="Mod+Enter">
            New session
          </Button>
          <Button size={s} icon={Icons.filter}>
            Filter
          </Button>
          <Button size={s} isDisabled>
            Disabled
          </Button>
          <Button size={s} variant="primary" isDisabled>
            Disabled
          </Button>
          <Button size={s} variant="danger" isDisabled>
            Disabled
          </Button>
          <Button size={s} variant="ghost" isDisabled>
            Disabled
          </Button>
        </Demo>
      ))}
      <Demo title="Long label in a 160 px box">
        <div className="w-40">
          <Button variant="default" className="w-full">
            <span className="truncate">vickrey-auction/2026-09-29/host-02</span>
          </Button>
        </div>
      </Demo>
      <Demo title="IconButton · sm, md, with shortcut, disabled (hover for the tooltip)">
        <IconButton icon={Icons.search} label="Find" kbd="Mod+K" />
        <IconButton icon={Icons.filter} label="Filter" />
        <IconButton icon={Icons.pin} label="Pin tab" size="md" />
        <IconButton icon={Icons.more} label="More" size="md" />
        <IconButton icon={Icons.copy} label="Copy" isDisabled />
      </Demo>
      <Demo title="Segmented">
        <Segmented
          label="View mode"
          items={[
            { id: "list", label: "List" },
            { id: "host", label: "By host" },
          ]}
          value={view}
          onChange={setView}
        />
        <Segmented
          label="Row size"
          size="md"
          items={[
            { id: "sm", label: "Compact", icon: Icons.steps },
            { id: "md", label: "Comfortable", icon: Icons.session, count: 14 },
          ]}
          value={size}
          onChange={setSize}
        />
        <Readout label="view" value={view} />
        <Readout label="size" value={size} />
      </Demo>
      <Demo title="Toggle, Slider (a step picker: one tick per step up to 100)">
        <Slider label="Step" max={12} value={step} onChange={setStep} className="w-64" />
        <Readout label="step" value={String(step)} />
        <Toggle isSelected={latest} onChange={setLatest}>
          Latest
        </Toggle>
        <Slider label="Disabled" max={0} value={0} onChange={setStep} isDisabled className="w-40" />
      </Demo>
    </Sec>
  );
}

function KbdSection() {
  return (
    <Sec
      id="kbd"
      title="Kbd"
      note="Mod is ⌘ on macOS and Ctrl elsewhere; the glyphs and separators follow the platform."
    >
      <Demo title="Shortcuts">
        {[
          "Mod+K",
          "Mod+Shift+P",
          "Mod+W",
          "Mod+Enter",
          "Shift+F8",
          "F8",
          "Esc",
          "Enter",
          "Up",
          "Down",
          "Tab",
        ].map((keys) => (
          <Kbd key={keys} keys={keys} />
        ))}
      </Demo>
    </Sec>
  );
}

const TONES: Tone[] = ["neutral", "accent", "ok", "done", "bad", "warn", "info"];

function BadgeSection() {
  return (
    <Sec
      id="badge"
      title="Badge, Count"
      note="Nearly square (2 px). Soft fills and outlines in seven tones; the words carry the meaning."
    >
      <Demo title="Soft">
        {TONES.map((tone) => (
          <Badge key={tone} tone={tone}>
            {tone}
          </Badge>
        ))}
      </Demo>
      <Demo title="Outline">
        {TONES.map((tone) => (
          <Badge key={tone} tone={tone} variant="outline">
            {tone}
          </Badge>
        ))}
      </Demo>
      <Demo title="With icon">
        <Badge tone="ok" icon={Icons.play}>
          active
        </Badge>
        <Badge tone="done" icon={Icons.check}>
          completed
        </Badge>
        <Badge tone="bad" icon={Icons.error} variant="outline">
          failed
        </Badge>
        <Badge tone="warn" icon={Icons.warn}>
          ending · 1 peer unconfirmed
        </Badge>
        <Badge tone="accent" icon={Icons.you}>
          your seat
        </Badge>
        <Badge>ABI 25</Badge>
      </Demo>
      <Demo title="Count">
        <span className="flex items-center gap-1.5 text-sm text-muted">
          Sessions <Count value={14} />
        </span>
        <span className="flex items-center gap-1.5 text-sm text-muted">
          Needs input <Count value={2} tone="accent" />
        </span>
        <span className="flex items-center gap-1.5 text-sm text-muted">
          Problems <Count value={3} tone="warn" />
        </span>
        <span className="flex items-center gap-1.5 text-sm text-muted">
          Errors <Count value={1} tone="bad" />
        </span>
        <span className="flex items-center gap-1.5 text-sm text-muted">
          Ok <Count value={9} tone="ok" />
          Done <Count value={31} tone="done" />
          Info <Count value={120} tone="info" />
        </span>
      </Demo>
    </Sec>
  );
}

function StatusSection() {
  return (
    <Sec
      id="status"
      title="Dot, Hint"
      note="Dots are the only round shapes. Hints are an icon and words in the tone's colour, never a box."
    >
      <Demo title="Dot · tones, 6 px and 8 px">
        {TONES.map((tone) => (
          <span key={tone} className="flex items-center gap-1.5 font-mono text-xs text-muted">
            <Dot tone={tone} /> <Dot tone={tone} size={8} /> {tone}
          </span>
        ))}
      </Demo>
      <Demo title="Dot · participant tones, ring, pulse">
        {(["p0", "p1", "p2", "p3", "p4", "pq"] as const).map((tone) => (
          <span key={tone} className="flex items-center gap-1.5 font-mono text-xs text-muted">
            <Dot tone={tone} size={8} /> {tone}
          </span>
        ))}
        <span className="flex items-center gap-1.5 font-mono text-xs text-muted">
          <Dot tone="p0" size={8} ring="accent" /> ring accent
        </span>
        <span className="flex items-center gap-1.5 font-mono text-xs text-muted">
          <Dot tone="p1" size={8} ring="bad" /> ring bad
        </span>
        <span className="flex items-center gap-1.5 font-mono text-xs text-muted">
          <Dot tone="ok" size={8} pulse /> live (pulse)
        </span>
      </Demo>
      <Demo title="Hint" className="gap-x-6">
        <Hint tone="you">needs you · 41s</Hint>
        <Hint tone="warn">waiting 6m 12s · top 5%</Hint>
        <Hint tone="error">diverged at step 17</Hint>
        <Hint tone="info">events skipped · reloaded</Hint>
        <Hint tone="quiet">no step yet</Hint>
        <Hint tone="warn" icon={Icons.time}>
          no step for 8m
        </Hint>
      </Demo>
      <Demo title="Hint · truncated in 160 px">
        <div className="w-40">
          <Hint tone="warn">negotiation timed out at prepared · 1 of 2 activation signatures</Hint>
        </div>
      </Demo>
    </Sec>
  );
}

function TooltipSection() {
  return (
    <Sec
      id="tooltip"
      title="Tooltip"
      note="500 ms after hover, or on keyboard focus. Title, optional body and shortcut; the trigger must be focusable."
    >
      <Demo title="Hover or focus each trigger">
        <Tooltip title="Find" kbd="Mod+K" placement="bottom">
          <Button icon={Icons.search}>Find</Button>
        </Tooltip>
        <Tooltip
          title="Terminate session"
          body="Ends the session for every participant on this host. Peers are told to stop."
          placement="right"
        >
          <Button variant="danger" icon={Icons.stop}>
            Terminate
          </Button>
        </Tooltip>
        <Tooltip title="Long unbroken title" body={HASH_A} placement="top">
          <Button variant="ghost">Long body</Button>
        </Tooltip>
        <HashChip hash={HASH_B} />
        <Tooltip title="Left" placement="left">
          <Button variant="ghost">Left</Button>
        </Tooltip>
      </Demo>
    </Sec>
  );
}

function PopoverSection() {
  return (
    <Sec
      id="popover"
      title="Popover"
      note="The shared anchored surface. Menus and selects use it; here it holds a small form."
    >
      <Demo title="Anchored to a button">
        <DialogTrigger>
          <Button icon={Icons.filter}>Filter sessions</Button>
          <Popover placement="bottom start">
            <RACDialog
              aria-label="Filter sessions"
              className="flex w-64 flex-col gap-3 p-3 outline-none"
            >
              <Heading slot="title" className="text-xs tracking-wide text-subtle uppercase">
                Filter
              </Heading>
              <TextField label="Program" placeholder="vickrey-auction" mono />
              <Checkbox defaultSelected>Only sessions on my seats</Checkbox>
              <div className="flex justify-end gap-2">
                <Button size="sm" variant="ghost" slot="close">
                  Cancel
                </Button>
                <Button size="sm" variant="primary" slot="close">
                  Apply
                </Button>
              </div>
            </RACDialog>
          </Popover>
        </DialogTrigger>
      </Demo>
    </Sec>
  );
}

function MenuSectionDemo() {
  const [last, setLast] = useState<string | null>(null);
  const act = (name: string) => () => setLast(name);
  return (
    <Sec
      id="menu"
      title="MenuButton, MenuItem, MenuSection"
      note="Keyboard: arrows, type-ahead, Enter. Items take an icon, a shortcut, or the danger tone."
    >
      <Demo title="Button variants">
        <MenuButton label="Actions" icon={Icons.more}>
          <MenuSection title="Session">
            <MenuItem icon={Icons.view} kbd="Enter" onAction={act("Open")}>
              Open
            </MenuItem>
            <MenuItem icon={Icons.link} onAction={act("Copy link")}>
              Copy link
            </MenuItem>
            <MenuItem icon={Icons.pin} onAction={act("Pin tab")}>
              Pin tab
            </MenuItem>
          </MenuSection>
          <MenuSection title="Danger">
            <MenuItem icon={Icons.stop} danger onAction={act("Terminate")}>
              Terminate…
            </MenuItem>
          </MenuSection>
        </MenuButton>
        <MenuButton label="Ghost" variant="ghost" placement="bottom end">
          <MenuItem onAction={act("Light")}>Light theme</MenuItem>
          <MenuItem onAction={act("Dark")}>Dark theme</MenuItem>
          <MenuSeparator />
          <MenuItem icon={Icons.keyboard} kbd="Mod+K" onAction={act("Palette")}>
            Command palette
          </MenuItem>
        </MenuButton>
        <MenuButton label="New" variant="primary" icon={Icons.add}>
          <MenuItem icon={Icons.session} onAction={act("New session")}>
            New session
          </MenuItem>
          <MenuItem icon={Icons.program} onAction={act("Import program")}>
            Import program…
          </MenuItem>
        </MenuButton>
        <Readout label="last action" value={last ?? "none"} />
      </Demo>
    </Sec>
  );
}

function DialogSection() {
  const [open, setOpen] = useState<"sm" | "md" | "lg" | "confirm" | "danger" | null>(null);
  const [result, setResult] = useState<string>("none");
  const close = (next: boolean) => {
    if (!next) setOpen(null);
  };
  return (
    <Sec
      id="dialog"
      title="Dialog, ConfirmDialog"
      note="Modal, dismissable with Esc. Confirm dialogs put initial focus on Cancel."
    >
      <Demo title="Open one">
        <Button onPress={() => setOpen("sm")}>Small</Button>
        <Button onPress={() => setOpen("md")}>Medium</Button>
        <Button onPress={() => setOpen("lg")}>Large</Button>
        <Button variant="primary" onPress={() => setOpen("confirm")}>
          Confirm
        </Button>
        <Button variant="danger" onPress={() => setOpen("danger")}>
          Confirm (danger)
        </Button>
        <Readout label="result" value={result} />
      </Demo>
      {(["sm", "md", "lg"] as const).map((size) => (
        <Dialog
          key={size}
          size={size}
          title={`Import program (${size})`}
          isOpen={open === size}
          onOpenChange={close}
          footer={
            <>
              <Button onPress={() => setOpen(null)}>Cancel</Button>
              <Button variant="primary" onPress={() => setOpen(null)}>
                Import
              </Button>
            </>
          }
        >
          <div className="flex flex-col gap-3">
            <TextField label="Program file" placeholder="./vickrey-auction.wasm" mono />
            <p className="text-sm text-muted">
              The program's hash is {HASH_A.slice(0, 16)}…; participants agree on it before any
              session starts.
            </p>
          </div>
        </Dialog>
      ))}
      <ConfirmDialog
        title="Withdraw offer"
        body="Withdrawing removes the offer from the lobby. Participants who already signed keep their copy."
        confirmLabel="Withdraw"
        isOpen={open === "confirm"}
        onOpenChange={close}
        onConfirm={() => setResult("withdrawn")}
      />
      <ConfirmDialog
        title="Terminate session"
        body="Ends the session for every participant on host-02. This cannot be undone."
        confirmLabel="Terminate"
        danger
        isOpen={open === "danger"}
        onOpenChange={close}
        onConfirm={() => setResult("terminated")}
      />
    </Sec>
  );
}

function ToastSection() {
  return (
    <Sec
      id="toast"
      title="toasts, ToastRegion"
      note="Bottom-right, six seconds, dismissable. Tone icon plus words; no side stripe."
    >
      <Demo title="Show a toast">
        <Button
          onPress={() =>
            toasts.show({
              tone: "ok",
              title: "Program imported",
              body: "vickrey-auction is on host-01 and host-02.",
            })
          }
        >
          ok
        </Button>
        <Button
          onPress={() =>
            toasts.show({
              tone: "warn",
              title: "Events skipped",
              body: "host-02 lagged; its state was reloaded.",
            })
          }
        >
          warn
        </Button>
        <Button
          onPress={() =>
            toasts.show({
              tone: "bad",
              title: "Answer rejected",
              body: "reserve must be a non-negative integer.",
            })
          }
        >
          bad
        </Button>
        <Button
          onPress={() =>
            toasts.show({
              tone: "info",
              title: "Offer seen",
              body: "rock-paper-scissors from agent-3a91.",
            })
          }
        >
          info
        </Button>
        <Button onPress={() => toasts.show({ tone: "neutral", title: "Copied hash" })}>
          neutral, title only
        </Button>
      </Demo>
    </Sec>
  );
}

function FieldSection() {
  const [name, setName] = useState("host-02");
  const [rounds, setRounds] = useState(10);
  const [flag, setFlag] = useState(true);
  const [host, setHost] = useState<string | null>("host-02");
  return (
    <Sec
      id="field"
      title="TextField, SearchField, NumberField, Checkbox, Switch, Select, FieldError"
      note="28 px controls, square corners, accent border on focus, bad border when invalid."
    >
      <div className="grid grid-cols-3 gap-x-8 gap-y-5">
        <Demo title="TextField" className="flex-col items-stretch">
          <TextField
            label="Host name"
            description="Shown to other participants"
            value={name}
            onChange={setName}
          />
          <TextField label="Peer id (mono)" mono placeholder="64 hex digits" />
          <TextField label="Disabled" defaultValue="read only" isDisabled />
          <div className="flex flex-col gap-1">
            <TextField label="Invalid" defaultValue="P9" isInvalid />
            <FieldError>host names must be lowercase</FieldError>
          </div>
        </Demo>
        <Demo title="SearchField, NumberField" className="flex-col items-stretch">
          <SearchField label="Find sessions" placeholder="Find…" kbd="/" />
          <SearchField label="Find (filled)" defaultValue="vickrey" />
          <NumberField
            label="Rounds"
            description="min 1 · max 50"
            value={rounds}
            onChange={setRounds}
            className="max-w-56"
          />
          <NumberField label="Budget" defaultValue={500} isDisabled className="max-w-56" />
        </Demo>
        <Demo title="Checkbox, Switch, Select" className="flex-col items-stretch">
          <Checkbox>Only my seats</Checkbox>
          <Checkbox defaultSelected>Include completed</Checkbox>
          <Checkbox isIndeterminate>Some programs</Checkbox>
          <Checkbox isDisabled defaultSelected>
            Disabled
          </Checkbox>
          <Switch isSelected={flag} onChange={setFlag}>
            Verbose log
          </Switch>
          <Switch>Off</Switch>
          <Switch isDisabled isSelected>
            Disabled on
          </Switch>
          <Select
            label="Seat host"
            value={host}
            onChange={setHost}
            items={[
              { id: "host-01", label: "host-01", detail: "arena0d/0.6.0" },
              { id: "host-02", label: "host-02", detail: "arena0d/0.6.0" },
              { id: "agent-3a91", label: "agent-3a91", detail: "codex/0.44.0" },
            ]}
          />
          <Select
            label="Nothing chosen"
            value={null}
            onChange={setHost}
            placeholder="Choose a host…"
            items={[{ id: "host-01", label: "host-01" }]}
          />
        </Demo>
      </div>
    </Sec>
  );
}

function TabsSection() {
  const [sub, setSub] = useState<"steps" | "view" | "negotiation" | "query" | "evidence">("steps");
  const [tabs, setTabs] = useState<DocTab[]>([
    { id: "sessions", label: "Sessions", icon: Icons.session, fixed: true },
    { id: "offers", label: "Offers", icon: Icons.offer, fixed: true },
    { id: "receipts", label: "Receipts", icon: Icons.receipt, fixed: true },
    { id: "programs", label: "Programs", icon: Icons.program, fixed: true },
    { id: "s1", label: "vickrey-auction", icon: Icons.session, detail: "c7e1fd7c", tone: "warn" },
    { id: "s2", label: "chess", icon: Icons.session, detail: "5f2c81d0", preview: true },
    { id: "r1", label: "receipt", icon: Icons.receipt, detail: "a1b2c3d4" },
    { id: "h1", label: "host-01", icon: Icons.host, tone: "ok" },
    { id: "p1", label: "sealed-bid-auction-with-a-very-long-program-name", icon: Icons.program },
  ]);
  const [active, setActive] = useState<string | null>("s1");
  const [event, setEvent] = useState("none");
  return (
    <Sec
      id="tabs"
      title="SubTabs, DocTabs"
      note="Document tabs: preview tabs are italic, double-click pins, middle-click closes, fixed tabs have no close."
    >
      <Demo title="SubTabs" className="block">
        <SubTabs
          label="Session views"
          value={sub}
          onChange={setSub}
          items={[
            { id: "steps", label: "Steps", icon: Icons.steps, count: 17 },
            { id: "view", label: "View", icon: Icons.view },
            { id: "negotiation", label: "Negotiation", icon: Icons.negotiation },
            { id: "query", label: "Query", icon: Icons.query },
            { id: "evidence", label: "Evidence", icon: Icons.evidence },
          ]}
        />
      </Demo>
      <Demo title="DocTabs" className="block">
        <Frame className="max-w-3xl">
          <DocTabs
            tabs={tabs}
            activeId={active}
            onSelect={(id) => {
              setActive(id);
              setEvent(`select ${id}`);
            }}
            onClose={(id) => {
              setTabs((all) => all.filter((tab) => tab.id !== id));
              setActive((current) => (current === id ? "sessions" : current));
              setEvent(`close ${id}`);
            }}
            onPin={(id) => {
              setTabs((all) =>
                all.map((tab) => (tab.id === id ? { ...tab, preview: false } : tab)),
              );
              setEvent(`pin ${id}`);
            }}
          />
          <div className="h-16 bg-editor p-3 text-sm text-muted">
            Active document: {active ?? "none"}
          </div>
        </Frame>
        <div className="mt-2">
          <Readout label="last event" value={event} />
        </div>
      </Demo>
    </Sec>
  );
}

const HOST_PROGRAMS = (host: string): TreeNode[] =>
  ["vickrey-auction", "chess"].map((program) => ({
    id: `${host}/${program}`,
    label: program,
    icon: Icons.program,
    trailing: (
      <span className="font-mono text-xs text-subtle">
        {hex64(program.length + host.length).slice(0, 6)}
      </span>
    ),
  }));

const TREE: TreeNode[] = [
  {
    id: "hosts",
    label: "Hosts",
    icon: Icons.host,
    trailing: <Count value={3} />,
    children: [
      {
        id: "host-01",
        label: "host-01",
        icon: Icons.host,
        iconTone: "ok",
        trailing: (
          <>
            <span className="text-ok">
              <Sparkline values={[1, 3, 2, 5, 4, 6, 3, 7]} width={28} height={10} />
            </span>
            <span className="font-mono text-xs">3 live</span>
          </>
        ),
        children: HOST_PROGRAMS("host-01"),
      },
      {
        id: "host-02",
        label: "host-02",
        icon: Icons.host,
        iconTone: "ok",
        trailing: <span className="font-mono text-xs">4 live</span>,
        children: HOST_PROGRAMS("host-02"),
      },
      {
        id: "host-03",
        label: "host-03",
        icon: Icons.host,
        iconTone: "bad",
        trailing: <Badge tone="bad">offline</Badge>,
      },
      {
        id: "agent-636f6465783a3031396133663265",
        label: "agent-636f6465783a3031396133663265",
        icon: Icons.you,
        iconTone: "accent",
        title: "codex:019a3f2e · codex/0.44.0",
      },
    ],
  },
  {
    id: "programs",
    label: "Programs",
    icon: Icons.program,
    trailing: <Count value={5} />,
    children: PROGRAMS.map((program, i) => ({
      id: `program-${program}`,
      label: program,
      icon: Icons.program,
      iconTone: (["p0", "p1", "p2", "p3", "p4"] as const)[i],
      title: hex64(50 + i),
    })),
  },
  {
    id: "receipts",
    label: "Receipts",
    icon: Icons.receipt,
    trailing: <Count value={2} />,
    children: [
      { id: "receipt-1", label: "receipt c7e1fd7c", icon: Icons.receipt, iconTone: "done" },
      { id: "receipt-2", label: "stop report 56622149", icon: Icons.stopReport, iconTone: "warn" },
    ],
  },
];

function TreeSection() {
  const [selected, setSelected] = useState<string | null>("host-02");
  const [action, setAction] = useState("none");
  return (
    <Sec
      id="tree"
      title="Tree"
      note="24 px rows, 12 px indent per level. Click selects, Enter opens, arrows expand and collapse. Hover a title-carrying row for its tooltip."
    >
      <Demo title="Explorer" className="items-start gap-8">
        <Frame className="w-72 py-1">
          <Tree
            label="Explorer"
            items={TREE}
            selectedId={selected}
            onSelect={setSelected}
            onAction={setAction}
            defaultExpanded={["hosts", "host-01", "programs"]}
          />
        </Frame>
        <div className="flex flex-col gap-1">
          <Readout label="selected" value={selected ?? "none"} />
          <Readout label="opened" value={action} />
        </div>
      </Demo>
    </Sec>
  );
}

function SectionSection() {
  return (
    <Sec
      id="section"
      title="Section, DockHeader"
      note="Collapsible 22 px group labels for docks, and the 32 px dock title row."
    >
      <Demo title="In a 288 px dock" className="items-start gap-8">
        <Frame className="w-72">
          <DockHeader
            title="Explorer"
            count={12}
            actions={<IconButton icon={Icons.add} label="New session" kbd="Mod+N" />}
          />
          <div className="py-1">
            <Section
              title="Hosts"
              icon={Icons.host}
              count={3}
              actions={<IconButton icon={Icons.filter} label="Filter hosts" size="sm" />}
            >
              <p className="px-3 py-1 text-sm text-muted">host-01, host-02, host-03</p>
            </Section>
            <Section title="Programs" icon={Icons.program} count={5} defaultExpanded={false}>
              <p className="px-3 py-1 text-sm text-muted">Collapsed until opened.</p>
            </Section>
            <Section title="Receipts" count="31">
              <p className="px-3 py-1 text-sm text-muted">Every receipt this operator holds.</p>
            </Section>
          </div>
        </Frame>
        <Frame className="w-72">
          <DockHeader title="A dock title that is much too long to fit in one row" count={99} />
        </Frame>
      </Demo>
    </Sec>
  );
}

function PanelSection() {
  return (
    <Sec
      id="panel"
      title="PanelGroup, Panel, PanelHandle"
      note="Resizable docks: a 1 px line with a 5 px hit area that turns accent on hover and drag."
    >
      <Frame className="h-56 max-w-4xl">
        <PanelGroup orientation="horizontal">
          <Panel defaultSize="22%" minSize="12%" className="bg-surface p-2 text-sm text-muted">
            Explorer
          </Panel>
          <PanelHandle orientation="horizontal" />
          <Panel minSize="30%" className="bg-editor">
            <PanelGroup orientation="vertical">
              <Panel minSize="30%" className="p-2 text-sm text-muted">
                Document
              </Panel>
              <PanelHandle orientation="vertical" />
              <Panel defaultSize="30%" minSize="15%" className="bg-surface p-2 text-sm text-muted">
                Timeline
              </Panel>
            </PanelGroup>
          </Panel>
          <PanelHandle orientation="horizontal" />
          <Panel defaultSize="24%" minSize="12%" className="bg-surface p-2 text-sm text-muted">
            Inspector
          </Panel>
        </PanelGroup>
      </Frame>
    </Sec>
  );
}

function ToolbarSection() {
  const [state, setState] = useState<"live" | "flagged" | "all">("live");
  const [group, setGroup] = useState<"list" | "host">("list");
  return (
    <Sec
      id="toolbar"
      title="Toolbar, ToolbarSeparator"
      note="Arrow keys move between the controls."
    >
      <Frame className="max-w-4xl">
        <Toolbar aria-label="Sessions toolbar">
          <Segmented
            label="State"
            value={state}
            onChange={setState}
            items={[
              { id: "live", label: "Live", count: 6 },
              { id: "flagged", label: "Flagged", count: 3 },
              { id: "all", label: "All", count: 14 },
            ]}
          />
          <ToolbarSeparator />
          <Segmented
            label="Grouping"
            value={group}
            onChange={setGroup}
            items={[
              { id: "list", label: "List" },
              { id: "host", label: "By host" },
            ]}
          />
          <span className="flex-1" />
          <SearchField label="Find" placeholder="Find…" kbd="/" className="w-44" />
          <IconButton icon={Icons.filter} label="Filters" />
          <MenuButton label="More" variant="ghost" placement="bottom end">
            <MenuItem icon={Icons.settings}>Settings</MenuItem>
          </MenuButton>
        </Toolbar>
      </Frame>
    </Sec>
  );
}

function SessionRow(props: { session: SessionSample }) {
  const { session } = props;
  return (
    <div className="flex w-full min-w-0 items-center gap-3">
      <Dots count={session.participants} you={session.key === "s3" ? 1 : undefined} />
      <span className="w-32 shrink-0 truncate">{session.program}</span>
      <HashChip hash={session.hash} />
      <StateBadge state={session.state} />
      <span className="ml-auto font-mono text-xs text-subtle tabular">step {session.step}</span>
      <span className="w-14 shrink-0 text-right font-mono text-xs text-subtle tabular">
        {fmtAge(session.ageMs)}
      </span>
    </div>
  );
}

function ListSection() {
  const [selected, setSelected] = useState<string | null>("s3");
  const [opened, setOpened] = useState("none");
  return (
    <Sec
      id="list"
      title="List"
      note="Virtualized (400 rows here), 26 px rows, single selection. Enter or double-click opens."
    >
      <Demo title="Sessions" className="items-start gap-6">
        <Frame className="h-64 w-[640px] bg-editor">
          <List
            label="Sessions"
            items={SESSIONS}
            getKey={(session) => session.key}
            selectedKey={selected}
            onSelect={setSelected}
            onAction={setOpened}
            empty={<EmptyState icon={Icons.session} title="No sessions" />}
          >
            {(session) => <SessionRow session={session} />}
          </List>
        </Frame>
        <Frame className="h-64 w-64 bg-editor">
          <List
            label="Empty list"
            items={[] as SessionSample[]}
            getKey={(session) => session.key}
            onSelect={() => {}}
            empty={
              <EmptyState
                icon={Icons.session}
                title="No live sessions"
                body="Sessions appear here as soon as a Host reports one."
              />
            }
          >
            {(session) => <SessionRow session={session} />}
          </List>
        </Frame>
        <div className="flex flex-col gap-1">
          <Readout label="selected" value={selected ?? "none"} />
          <Readout label="opened" value={opened} />
        </div>
      </Demo>
    </Sec>
  );
}

function TableSection() {
  const [selected, setSelected] = useState<string | null>("s2");
  return (
    <Sec
      id="table"
      title="DataTable"
      note="Virtualized (400 rows), 24 px header, 26 px rows. Completed rows are quiet."
    >
      <Demo title="Sessions" className="items-start gap-6">
        <Frame className="h-64 w-[760px] bg-editor">
          <DataTable
            label="Sessions table"
            rows={SESSIONS}
            getKey={(session) => session.key}
            selectedKey={selected}
            onSelect={setSelected}
            rowTone={(session) => (session.state === "completed" ? "quiet" : "normal")}
            empty={<EmptyState icon={Icons.session} title="No sessions" />}
            columns={[
              {
                id: "program",
                title: "Program",
                isRowHeader: true,
                width: "2fr",
                minWidth: 140,
                render: (s) => <span className="truncate">{s.program}</span>,
              },
              {
                id: "session",
                title: "Session",
                width: 130,
                render: (s) => <HashChip hash={s.hash} />,
              },
              {
                id: "participants",
                title: "Participants",
                width: 100,
                render: (s) => <Dots count={s.participants} />,
              },
              {
                id: "state",
                title: "State",
                width: 110,
                render: (s) => <StateBadge state={s.state} />,
              },
              {
                id: "step",
                title: "Step",
                width: 56,
                align: "end",
                render: (s) => <span className="font-mono tabular">{s.step}</span>,
              },
              {
                id: "age",
                title: "Age",
                width: 72,
                align: "end",
                render: (s) => <span className="font-mono tabular">{fmtAge(s.ageMs)}</span>,
              },
            ]}
          />
        </Frame>
        <Frame className="h-64 w-64 bg-editor">
          <DataTable
            label="Empty table"
            rows={[] as SessionSample[]}
            getKey={(session) => session.key}
            empty={<EmptyState icon={Icons.receipt} title="No receipts" />}
            columns={[
              {
                id: "id",
                title: "Receipt",
                isRowHeader: true,
                width: "1fr",
                render: (s) => s.hash,
              },
              { id: "n", title: "Signers", align: "end", width: 64, render: () => 0 },
            ]}
          />
        </Frame>
      </Demo>
    </Sec>
  );
}

function KeyValueSection() {
  return (
    <Sec
      id="keyvalue"
      title="KeyValue"
      note="Aligned label and value columns for the inspector. A title puts the full value in a tooltip."
    >
      <Demo title="Normal, dense" className="items-start gap-12">
        <div className="w-72">
          <KeyValue
            items={[
              { k: "program", v: "vickrey-auction" },
              { k: "session", v: HASH_A.slice(0, 16) + "…", mono: true, title: HASH_A },
              { k: "participants", v: "4", mono: true },
              { k: "state", v: <Badge tone="ok">active</Badge> },
              {
                k: "host",
                v: "host-02 · arena0d/0.6.0 · a very long value that has to be truncated",
                title: "host-02 · arena0d/0.6.0 · a very long value that has to be truncated",
              },
            ]}
          />
        </div>
        <div className="w-72">
          <KeyValue
            dense
            items={[
              { k: "step", v: "17", mono: true },
              { k: "certified", v: "14:03:07", mono: true },
              { k: "signers", v: "2/2", mono: true },
              { k: "post-state", v: HASH_B.slice(0, 12), mono: true, title: HASH_B },
            ]}
          />
        </div>
      </Demo>
    </Sec>
  );
}

function EmptySection() {
  return (
    <Sec id="empty" title="EmptyState">
      <div className="grid max-w-4xl grid-cols-3 gap-4">
        <Frame className="bg-editor">
          <EmptyState icon={Icons.receipt} title="No receipts yet" />
        </Frame>
        <Frame className="bg-editor">
          <EmptyState
            icon={Icons.offer}
            title="No offers"
            body="Offers appear when another participant proposes a program on a topic this operator listens to."
          />
        </Frame>
        <Frame className="bg-editor">
          <EmptyState
            icon={Icons.session}
            title="No sessions on host-02"
            body="Start one from a program."
            action={
              <Button variant="primary" icon={Icons.add}>
                New session
              </Button>
            }
          />
        </Frame>
      </div>
    </Sec>
  );
}

function PaletteSection(props: { open: () => void; last: string | null }) {
  return (
    <Sec
      id="palette"
      title="CommandPalette"
      note="560 px, top-anchored at 12vh. Type to filter labels, details and keywords; Enter runs the highlighted item and closes."
    >
      <Demo title="Open it (or press Mod+K)">
        <Button icon={Icons.search} kbd="Mod+K" onPress={props.open} data-testid="open-palette">
          Open command palette
        </Button>
        <Readout label="last action" value={props.last ?? "none"} />
      </Demo>
    </Sec>
  );
}

function FingerprintSection() {
  return (
    <Sec
      id="fingerprint"
      title="Fingerprint"
      note="Eight bars from the first eight hex digits. Same hash, same shape; the colour is inherited."
    >
      <Demo title="sm (10 px) and md (14 px)">
        {[HASH_A, HASH_B, HASH_C, hex64(4), hex64(5)].map((hash) => (
          <span key={hash} className="flex items-center gap-2 font-mono text-xs text-muted">
            <Fingerprint hash={hash} size="sm" />
            <Fingerprint hash={hash} />
            {hash.slice(0, 8)}
          </span>
        ))}
        <span className="flex items-center gap-2 font-mono text-xs text-muted">
          <Fingerprint hash={HASH_A} />
          <Fingerprint hash={HASH_A} />
          same hash twice
        </span>
      </Demo>
      <Demo title="Inherits colour">
        <span className="text-accent">
          <Fingerprint hash="0123456789abcdef" />
        </span>
        <span className="text-ok">
          <Fingerprint hash="fedcba9876543210" />
        </span>
        <span className="text-bad">
          <Fingerprint hash="ffffffff00000000" />
        </span>
      </Demo>
    </Sec>
  );
}

function HashChipSection() {
  return (
    <Sec
      id="hashchip"
      title="HashChip"
      note="First eight digits and a fingerprint; the tooltip holds the full hash. Copy writes the whole hash to the clipboard."
    >
      <Demo title="Variants" className="gap-x-8">
        <HashChip hash={HASH_A} />
        <HashChip hash={HASH_A} copy />
        <HashChip hash={HASH_B} chars={16} copy />
        <HashChip hash={HASH_C} title="Program hash" />
        <HashChip hash={HASH_C} chars={4} />
        <span className="text-accent">
          <HashChip hash={HASH_A} />
        </span>
      </Demo>
    </Sec>
  );
}

function ParticipantSection() {
  return (
    <Sec
      id="participant"
      title="ParticipantChip, ParticipantDot"
      note="Five participant colours, then one shared grey; the index text carries identity beyond that."
    >
      <Demo title="ParticipantChip">
        {[0, 1, 2, 3, 4, 5, 7].map((index) => (
          <ParticipantChip key={index} index={index} label={`host-0${(index % 4) + 1}`} />
        ))}
        <ParticipantChip index={1} label="host-03" you title="P1 · host-03 · your seat" />
        <ParticipantChip index={2} label="agent-636f6465783a3031396133663265" />
      </Demo>
      <Demo title="ParticipantDot · active, done, bad, idle, you">
        {(["active", "done", "bad", "idle"] as const).map((state) => (
          <span key={state} className="flex items-center gap-2 text-sm text-muted">
            {[0, 1, 2, 3, 4, 5].map((index) => (
              <ParticipantDot key={index} index={index} state={state} />
            ))}
            {state}
          </span>
        ))}
        <span className="flex items-center gap-2 text-sm text-muted">
          <ParticipantDot index={0} you />
          <ParticipantDot index={1} state="bad" you />
          you
        </span>
      </Demo>
    </Sec>
  );
}

function MeterSection() {
  return (
    <Sec
      id="meter"
      title="AgreementMeter"
      note="One cell per participant: filled when signed, hollow while missing."
    >
      <Demo title="Signed of N" className="gap-8">
        {[
          { signers: [], participants: 3 },
          { signers: [0, 2], participants: 3 },
          { signers: [0, 1, 2], participants: 3 },
          { signers: [0, 1], participants: 6 },
          { signers: [0, 1, 2, 3, 4, 5, 6, 7], participants: 12 },
        ].map((meter) => (
          <span
            key={`${meter.signers.length}/${meter.participants}`}
            className="flex items-center gap-2 font-mono text-xs text-muted"
          >
            <AgreementMeter signers={meter.signers} participants={meter.participants} />
            {meter.signers.length}/{meter.participants}
          </span>
        ))}
      </Demo>
    </Sec>
  );
}

function SparklineSection() {
  return (
    <Sec
      id="sparkline"
      title="Sparkline"
      note="A line scaled to its own range. Inherits the text colour."
    >
      <Demo title="Series" className="gap-8">
        <span className="text-muted">
          <Sparkline values={[1, 3, 2, 5, 4, 6, 3, 7, 5, 8]} width={64} height={16} />
        </span>
        <span className="text-ok">
          <Sparkline values={[4, 4, 5, 7, 8, 8, 9, 12]} width={96} height={20} />
        </span>
        <span className="text-warn">
          <Sparkline values={[9, 8, 8, 5, 3, 3, 1]} width={96} height={20} />
        </span>
        <span className="text-subtle">
          <Sparkline values={[3, 3, 3, 3]} width={48} height={12} />
        </span>
        <span className="text-subtle">
          <Sparkline values={[5]} width={48} height={12} />
        </span>
      </Demo>
    </Sec>
  );
}

const AXIS_FROM = NOW - 30 * MIN;
const at = (minutesAgo: number) => NOW - minutesAgo * MIN;

const STRIPS: { label: string; spans: StripSpan[]; marks: StripMark[] }[] = [
  {
    label: "negotiating, active, waiting on you",
    spans: [
      { from: at(30), to: at(27), kind: "negotiating" },
      { from: at(27), to: NOW, kind: "active" },
      { from: at(1), to: NOW, kind: "waiting-you" },
    ],
    marks: [4, 9, 11, 14, 16, 19, 21, 24, 26].map((m) => ({
      at: at(27 - (26 - m) + 0),
      kind: "step" as const,
    })),
  },
  {
    label: "waiting on a peer, then long",
    spans: [
      { from: at(20), to: NOW, kind: "active" },
      { from: at(8), to: at(3), kind: "waiting" },
      { from: at(3), to: NOW, kind: "waiting-long" },
    ],
    marks: [19, 17, 15, 12, 9].map((m) => ({ at: at(m), kind: "step" as const })),
  },
  {
    label: "completed",
    spans: [
      { from: at(28), to: at(26), kind: "negotiating" },
      { from: at(26), to: at(12), kind: "active" },
    ],
    marks: [
      ...[25, 23, 21, 19, 17, 15, 13].map((m) => ({ at: at(m), kind: "step" as const })),
      { at: at(12), kind: "end-ok" as const },
    ],
  },
  {
    label: "failed (diverged)",
    spans: [
      { from: at(24), to: at(22), kind: "negotiating" },
      { from: at(22), to: at(9), kind: "active" },
    ],
    marks: [
      ...[21, 19, 16, 13, 11].map((m) => ({ at: at(m), kind: "step" as const })),
      { at: at(9), kind: "end-bad" as const },
    ],
  },
  {
    label: "ending, an observation gap",
    spans: [
      { from: at(30), to: at(14), kind: "active" },
      { from: at(14), to: at(5), kind: "ending" },
    ],
    marks: [
      ...[28, 25, 22, 18].map((m) => ({ at: at(m), kind: "step" as const })),
      { at: at(20), kind: "gap" as const },
      { at: at(14), kind: "end-ok" as const },
    ],
  },
  {
    label: "still negotiating",
    spans: [{ from: at(6), to: NOW, kind: "negotiating" }],
    marks: [],
  },
];

function StripSection() {
  return (
    <Sec
      id="strip"
      title="LifecycleStrip"
      note="A session's life on a shared axis: dashed negotiating, solid green active, waits on a second line, step ticks above."
    >
      <div className="flex max-w-3xl flex-col gap-1">
        {STRIPS.map((strip) => (
          <div key={strip.label} className="flex items-center gap-4">
            <span className="w-64 shrink-0 text-sm text-muted">{strip.label}</span>
            <LifecycleStrip
              from={AXIS_FROM}
              to={NOW}
              width={360}
              spans={strip.spans}
              marks={strip.marks}
            />
          </div>
        ))}
      </div>
      <Demo title="Legend" className="gap-x-6 text-sm text-muted">
        <span className="flex items-center gap-2">
          <LifecycleStrip
            from={0}
            to={1}
            width={40}
            spans={[{ from: 0, to: 1, kind: "negotiating" }]}
            marks={[]}
            height={12}
          />
          negotiating
        </span>
        <span className="flex items-center gap-2">
          <LifecycleStrip
            from={0}
            to={1}
            width={40}
            spans={[{ from: 0, to: 1, kind: "active" }]}
            marks={[]}
            height={12}
          />
          active
        </span>
        <span className="flex items-center gap-2">
          <LifecycleStrip
            from={0}
            to={1}
            width={40}
            spans={[{ from: 0, to: 1, kind: "waiting" }]}
            marks={[]}
            height={20}
          />
          waiting
        </span>
        <span className="flex items-center gap-2">
          <LifecycleStrip
            from={0}
            to={1}
            width={40}
            spans={[{ from: 0, to: 1, kind: "waiting-you" }]}
            marks={[]}
            height={20}
          />
          waiting on you
        </span>
        <span className="flex items-center gap-2">
          <LifecycleStrip
            from={0}
            to={1}
            width={40}
            spans={[{ from: 0, to: 1, kind: "waiting-long" }]}
            marks={[]}
            height={20}
          />
          waiting long
        </span>
        <span className="flex items-center gap-2">
          <LifecycleStrip
            from={0}
            to={1}
            width={40}
            spans={[{ from: 0, to: 1, kind: "ending" }]}
            marks={[]}
            height={12}
          />
          ending
        </span>
      </Demo>
    </Sec>
  );
}

const BUCKETS: TimeBucket[] = Array.from({ length: 60 }, (_, i) => {
  const up = Math.round(6 + 5 * Math.sin(i / 4) + ((i * 7) % 5));
  return {
    t: NOW - (60 - i) * MIN,
    up,
    down: i > 20 ? (i % 9 < 5 ? 1 + (i % 3) : 0) : 0,
    bad: i === 41 || i === 42 ? Math.min(up, 3) : 0,
  };
});

function TimelineSection() {
  const [brush, setBrush] = useState<{ from: number; to: number } | null>(null);
  return (
    <Sec
      id="timeline"
      title="TimelineChart"
      note="Steps per minute above the line, open callouts below, failures in red. Drag to select a range; click to clear it."
    >
      <div className="max-w-4xl">
        <Frame className="bg-editor">
          <TimelineChart
            buckets={BUCKETS}
            from={NOW - 60 * MIN}
            to={NOW}
            height={96}
            brush={brush}
            onBrush={setBrush}
            upLabel="steps/min"
            downLabel="open callouts"
          />
        </Frame>
        <div className="mt-2" data-testid="brush-readout">
          <Readout
            label="brush"
            value={brush ? `${Math.round((brush.to - brush.from) / MIN)} min selected` : "none"}
          />
        </div>
      </div>
      <Demo title="Preselected range, short height" className="block">
        <Frame className="max-w-4xl bg-editor">
          <TimelineChart
            buckets={BUCKETS.slice(30)}
            from={NOW - 30 * MIN}
            to={NOW}
            height={64}
            brush={{ from: NOW - 20 * MIN, to: NOW - 8 * MIN }}
            onBrush={() => {}}
            upLabel="steps/min"
            downLabel="open callouts"
          />
        </Frame>
      </Demo>
    </Sec>
  );
}

const ESC = "\u001b";
const sgr = (codes: string, text: string) => `${ESC}[${codes}m${text}${ESC}[0m`;

const CHESS_BOARD = (() => {
  const rows = [
    "r . . q . r k .",
    ". . . n b p p p",
    ". . . p b . . .",
    ". . . . p P P .",
    ". p . B P . . .",
    ". P . . . . . .",
    "P P P Q . . . P",
    ". K . R . B . R",
  ];
  const glyph: Record<string, string> = {
    K: "♔",
    Q: "♕",
    R: "♖",
    B: "♗",
    N: "♘",
    P: "♙",
    k: "♚",
    q: "♛",
    r: "♜",
    b: "♝",
    n: "♞",
    p: "♟",
  };
  let board = `${sgr("1", "Chess - black's turn, move 22")}\n\n    a   b   c   d   e   f   g   h\n  +---+---+---+---+---+---+---+---+\n`;
  rows.forEach((row, i) => {
    const cells = row.split(" ").map((piece) => {
      if (piece === ".") return " . ";
      const g = glyph[piece] ?? "?";
      return ` ${piece === piece.toUpperCase() ? sgr("1", g) : sgr("35", g)} `;
    });
    board += `${8 - i} |${cells.join("|")}| ${8 - i}\n  +---+---+---+---+---+---+---+---+\n`;
  });
  return `${board}    a   b   c   d   e   f   g   h\n${sgr("90", "Last move: e5d4")}`;
})();

const SIXTEEN = Array.from({ length: 16 }, (_, n) => {
  const fg = n < 8 ? 30 + n : 82 + n;
  return sgr(String(fg), ` ${String(n).padStart(2, "0")} `);
}).join("");
const SIXTEEN_BG = Array.from({ length: 16 }, (_, n) =>
  sgr(String(n < 8 ? 40 + n : 92 + n), "    "),
).join(" ");

const STYLES_SAMPLE = [
  sgr("1", "bold"),
  sgr("2", "dim"),
  sgr("3", "italic"),
  sgr("4", "underline"),
  sgr("7", "inverse"),
  sgr("31;7", "inverse red"),
  sgr("1;3;4;33", "bold italic underline yellow"),
  `${ESC}[1mbold ${ESC}[22mnormal ${ESC}[3mitalic ${ESC}[23mupright${ESC}[0m`,
].join("  ");

const EXTENDED_SAMPLE = [
  sgr("38;5;208", "256 orange"),
  sgr("38;5;33", "256 blue"),
  sgr("48;5;238;38;5;252", " 256 grey bg "),
  sgr("38;2;255;105;180", "true colour pink"),
  sgr("48;2;30;80;60;38;2;220;255;230", " true colour bg "),
].join("  ");

// Sequences a guest could emit that must never reach the DOM.
const HOSTILE = `${ESC}]0;window title${String.fromCharCode(7)}${ESC}[2J${ESC}[H${ESC}[?25lline one${String.fromCharCode(13)}${String.fromCharCode(8)}${String.fromCharCode(8)}${ESC}[5Aline two${ESC}[K ${ESC}]8;;https://example.invalid${ESC}\\link text${ESC}]8;;${ESC}\\ ${ESC}(B${ESC}cend ${sgr("32", "green survives")}`;

function AnsiSection() {
  return (
    <Sec
      id="ansi"
      title="AnsiText"
      note="Colour and weight escapes become spans (theme colours for the 16-colour codes). Every other escape and control sequence is dropped; output is text nodes only."
    >
      <Demo title="A guest's board (Chess view)" className="items-start gap-8">
        <Frame className="bg-editor p-3">
          <div data-testid="ansi-chess">
            <AnsiText text={CHESS_BOARD} />
          </div>
        </Frame>
        <div className="flex flex-col gap-4">
          <div>
            <h4 className="mb-1 text-xs text-subtle">16 foreground colours</h4>
            <AnsiText text={SIXTEEN} />
          </div>
          <div>
            <h4 className="mb-1 text-xs text-subtle">16 background colours (0 to 15)</h4>
            <AnsiText text={SIXTEEN_BG} />
          </div>
          <div>
            <h4 className="mb-1 text-xs text-subtle">Weight and style</h4>
            <AnsiText text={STYLES_SAMPLE} />
          </div>
          <div>
            <h4 className="mb-1 text-xs text-subtle">256 and true colour (inline colours)</h4>
            <AnsiText text={EXTENDED_SAMPLE} />
          </div>
        </div>
      </Demo>
      <Demo title="Hostile input, before and after" className="items-start gap-8">
        <div>
          <h4 className="mb-1 text-xs text-subtle">Escape characters shown as ␛</h4>
          <pre className="font-mono text-sm whitespace-pre-wrap text-muted" data-testid="ansi-raw">
            {HOSTILE.replaceAll(ESC, "␛").replace(/[\u0000-\u0008\u000b-\u001f]/g, "·")}
          </pre>
        </div>
        <div>
          <h4 className="mb-1 text-xs text-subtle">Rendered</h4>
          <AnsiText text={HOSTILE} />
        </div>
      </Demo>
    </Sec>
  );
}

const SESSION_JSON: JsonLike = {
  session_id: HASH_A,
  program: { name: "vickrey-auction", version: "1.0.0", hash: HASH_B },
  participants: [
    { index: 0, host: "host-01", peer_id: hex64(7), you: false },
    { index: 1, host: "host-02", peer_id: hex64(8), you: true },
  ],
  params: {
    item: "widget",
    reserve: 10,
    settlement: { kind: "second-price", tiebreak: "lowest-index" },
  },
  step: 17,
  terminal: null,
  receipts: [],
  flags: {
    diverged: false,
    gaps: 1,
    note: "a long string value that wraps instead of overflowing its container when it gets long enough",
  },
  empty_object: {},
  empty_array: [],
};

function JsonSection() {
  return (
    <Sec
      id="json"
      title="JsonView"
      note="Syntax colours; containers nested deeper than collapseDepth start collapsed behind a toggle."
    >
      <Demo title="collapseDepth 1 and 3" className="items-start gap-12">
        <Frame className="w-[480px] bg-editor p-3">
          <JsonView value={SESSION_JSON} collapseDepth={1} />
        </Frame>
        <Frame className="w-[480px] bg-editor p-3">
          <JsonView value={SESSION_JSON} collapseDepth={3} />
        </Frame>
      </Demo>
    </Sec>
  );
}

const PARAMS_SCHEMA: JsonLike = {
  type: "object",
  required: ["item", "currency", "rounds"],
  properties: {
    item: {
      type: "string",
      title: "Item",
      description: "What is being sold",
      pattern: "^[a-z][a-z0-9-]*$",
    },
    currency: { enum: ["credit", "usd", "eur"], title: "Currency" },
    seller: {
      enum: ["host-01", "host-02", "host-03", "host-04", "agent-3a91", "agent-5f2c"],
      title: "Seller",
    },
    rounds: { type: "integer", title: "Rounds", minimum: 1, maximum: 50 },
    reserve: { type: ["integer", "null"], title: "Reserve price", minimum: 0 },
    settlement: {
      title: "Settlement",
      oneOf: [
        {
          title: "Second price",
          type: "object",
          properties: { tiebreak: { $ref: "#/$defs/tiebreak" } },
        },
        {
          title: "First price",
          type: "object",
          required: ["fee"],
          properties: { fee: { type: "number", minimum: 0, title: "Fee" } },
        },
      ],
    },
    bidders: {
      type: "array",
      title: "Bidders",
      maxItems: 4,
      items: { $ref: "#/$defs/bidder" },
    },
    verbose: {
      type: "boolean",
      title: "Verbose log",
      description: "Print every step to the host log",
    },
  },
  $defs: {
    tiebreak: { enum: ["lowest-index", "random-seeded"], title: "Tie break" },
    bidder: {
      type: "object",
      required: ["name", "budget"],
      properties: {
        name: { type: "string", title: "Name" },
        budget: { type: "integer", minimum: 0, title: "Budget" },
      },
    },
  },
};

const INITIAL_PARAMS: JsonLike = {
  item: "widget",
  currency: "credit",
  seller: "host-02",
  rounds: 10,
  reserve: 10,
  settlement: { tiebreak: "lowest-index" },
  bidders: [{ name: "host-03", budget: 500 }],
  verbose: false,
};

const CHESS_MOVE: JsonLike = {
  type: "string",
  pattern: "^[a-h][1-8][a-h][1-8][qrbn]?$",
  description: "from-square, to-square, optional promotion",
};
const CHOICE: JsonLike = { enum: ["Cooperate", "Defect"] };
const ADD: JsonLike = { type: "integer", minimum: 0, maximum: 100 };
const UNSUPPORTED: JsonLike = { allOf: [{ type: "object" }, { required: ["x"] }] };

function MiniForm(props: {
  title: string;
  schema: JsonLike;
  initial: JsonLike | undefined;
  disabled?: boolean;
}) {
  const [value, setValue] = useState<JsonLike | undefined>(props.initial);
  const issues = validate(props.schema, value);
  return (
    <div className="flex min-w-0 flex-col gap-2">
      <h4 className="text-xs tracking-wide text-subtle uppercase">{props.title}</h4>
      <SchemaForm
        schema={props.schema}
        value={value}
        onChange={setValue}
        issues={issues}
        disabled={props.disabled}
      />
      <Readout label="value" value={value === undefined ? "undefined" : JSON.stringify(value)} />
    </div>
  );
}

function SchemaFormSection() {
  const [value, setValue] = useState<JsonLike | undefined>(INITIAL_PARAMS);
  const issues = validate(PARAMS_SCHEMA, value);
  return (
    <Sec
      id="schemaform"
      title="SchemaForm, JsonEditor, validate"
      note="Generated from a JSON Schema (2020-12): enum, integer, string, boolean, object, array, oneOf, local $ref. Issues appear under the field they belong to."
    >
      <div className="grid max-w-5xl grid-cols-[minmax(0,1fr)_minmax(0,1fr)] gap-8">
        <div data-testid="params-form">
          <SchemaForm schema={PARAMS_SCHEMA} value={value} onChange={setValue} issues={issues} />
        </div>
        <div className="flex min-w-0 flex-col gap-3">
          <h4 className="text-xs tracking-wide text-subtle uppercase">Value</h4>
          <div data-testid="value-view">
            <Frame className="bg-editor p-3">
              <JsonView value={value ?? null} collapseDepth={3} />
            </Frame>
          </div>
          <h4 className="text-xs tracking-wide text-subtle uppercase">Issues ({issues.length})</h4>
          <div className="flex flex-col gap-1 font-mono text-xs" data-testid="issue-list">
            {issues.length === 0 && <span className="text-ok">valid</span>}
            {issues.map((issue) => (
              <span key={`${issue.path}${issue.message}`} className="text-bad">
                {issue.path || "/"} {issue.message}
              </span>
            ))}
          </div>
        </div>
      </div>
      <div className="grid max-w-5xl grid-cols-4 gap-8">
        <MiniForm title="Callout · enum" schema={CHOICE} initial={undefined} />
        <MiniForm title="Callout · integer" schema={ADD} initial={150} />
        <MiniForm title="Callout · string pattern" schema={CHESS_MOVE} initial="e9e4" />
        <MiniForm title="Unsupported → raw" schema={UNSUPPORTED} initial={{ y: 1 }} />
      </div>
      <div className="grid max-w-5xl grid-cols-4 gap-8">
        <MiniForm title="Disabled" schema={PARAMS_SCHEMA} initial={INITIAL_PARAMS} disabled />
      </div>
    </Sec>
  );
}

// ---------------------------------------------------------------- page

export function Gallery() {
  const [theme, setTheme] = useState<"light" | "dark">("light");
  const [paletteOpen, setPaletteOpen] = useState(false);
  const [lastAction, setLastAction] = useState<string | null>(null);

  useEffect(() => {
    document.documentElement.dataset.theme = theme;
  }, [theme]);

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "k") {
        event.preventDefault();
        setPaletteOpen(true);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  const run = (label: string) => () => setLastAction(label);
  const groups: { id: string; title: string; items: PaletteItem[] }[] = [
    {
      id: "sessions",
      title: "Sessions",
      items: [
        {
          id: "s-auction",
          label: "vickrey-auction",
          detail: "c7e1fd7c · 4 participants · needs you",
          icon: Icons.session,
          keywords: ["auction", "sealed"],
          onAction: run("vickrey-auction"),
        },
        {
          id: "s-chess",
          label: "chess",
          detail: "5f2c81d0 · agent-3a91 vs agent-5f2c",
          icon: Icons.session,
          onAction: run("chess"),
        },
        {
          id: "s-pd",
          label: "prisoner-dilemma",
          detail: "56622149 · completed",
          icon: Icons.session,
          onAction: run("prisoner-dilemma"),
        },
      ],
    },
    {
      id: "programs",
      title: "Programs",
      items: [
        {
          id: "p-contract",
          label: "contract-net",
          detail: "Collect work proposals and select an award",
          icon: Icons.program,
          onAction: run("contract-net"),
        },
        {
          id: "p-rps",
          label: "rock-paper-scissors",
          detail: "Commit and reveal simultaneous choices",
          icon: Icons.program,
          keywords: ["rps"],
          onAction: run("rock-paper-scissors"),
        },
      ],
    },
    {
      id: "actions",
      title: "Actions",
      items: [
        {
          id: "a-new",
          label: "New session",
          icon: Icons.add,
          kbd: "Mod+N",
          keywords: ["create", "start"],
          onAction: run("New session"),
        },
        {
          id: "a-theme",
          label: "Toggle theme",
          icon: Icons.theme,
          keywords: ["dark", "light"],
          onAction: run("Toggle theme"),
        },
        {
          id: "a-next",
          label: "Next problem",
          icon: Icons.problem,
          kbd: "F8",
          onAction: run("Next problem"),
        },
      ],
    },
  ];

  return (
    <div
      data-testid="gallery"
      className="fixed inset-0 scroll-pt-12 overflow-y-auto bg-editor text-fg"
    >
      <header className="sticky top-0 z-30 flex h-10 items-center gap-4 border-b border-line bg-title px-6">
        <h1 className="text-base font-semibold">arena0 · components</h1>
        <Segmented
          label="Theme"
          items={[
            { id: "light", label: "Light" },
            { id: "dark", label: "Dark" },
          ]}
          value={theme}
          onChange={setTheme}
        />
        <span className="flex-1" />
        <span className="font-mono text-sm text-subtle" data-testid="last-action">
          last action: <span className="text-fg">{lastAction ?? "none"}</span>
        </span>
        <Button size="sm" icon={Icons.search} kbd="Mod+K" onPress={() => setPaletteOpen(true)}>
          Find
        </Button>
      </header>
      <main>
        <ButtonSection />
        <KbdSection />
        <BadgeSection />
        <StatusSection />
        <TooltipSection />
        <PopoverSection />
        <MenuSectionDemo />
        <DialogSection />
        <ToastSection />
        <FieldSection />
        <TabsSection />
        <TreeSection />
        <SectionSection />
        <PanelSection />
        <ToolbarSection />
        <ListSection />
        <TableSection />
        <KeyValueSection />
        <EmptySection />
        <PaletteSection open={() => setPaletteOpen(true)} last={lastAction} />
        <FingerprintSection />
        <HashChipSection />
        <ParticipantSection />
        <MeterSection />
        <SparklineSection />
        <StripSection />
        <TimelineSection />
        <AnsiSection />
        <JsonSection />
        <SchemaFormSection />
      </main>
      <CommandPalette isOpen={paletteOpen} onOpenChange={setPaletteOpen} groups={groups} />
      <ToastRegion />
    </div>
  );
}
