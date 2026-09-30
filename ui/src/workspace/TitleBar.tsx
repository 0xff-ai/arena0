import { CheckboxGroup, Dialog, DialogTrigger } from "react-aria-components";
import { useRows, useTheme, useYourHosts } from "~/model";
import { useCollections, useConnection, useDaemonInfo } from "~/sync";
import { Button, Checkbox, Dot, IconButton, Icons, Kbd, Popover, type Tone } from "~/ui";
import { openPalette } from "./shell";

const NEXT_THEME = { light: "dark", dark: "system", system: "light" } as const;

export const connectionTone: Record<string, Tone> = {
  live: "ok",
  syncing: "warn",
  connecting: "warn",
  offline: "bad",
};

export function TitleBar() {
  const hello = useDaemonInfo();
  const connection = useConnection();
  const [theme, setTheme] = useTheme();
  const hosts = useRows(useCollections().hosts);
  const [yourHosts, setYourHosts] = useYourHosts(hosts.map((host) => host.id));

  return (
    <header className="flex h-9 items-center gap-3 border-b border-line bg-title px-3">
      <span className="flex items-center gap-1.5 font-mono text-base font-medium text-fg">
        <span aria-hidden>▣</span>
        arena0
      </span>
      {hello && <span className="font-mono text-xs text-subtle">{hello.version}</span>}

      <Button
        onPress={() => openPalette("all")}
        className="ml-2 w-72 max-w-[40vw] justify-start gap-2 bg-editor! text-subtle"
        aria-label="Find"
      >
        <span className="flex min-w-0 flex-1 items-center gap-1.5 text-left text-sm">
          <Icons.search size={13} strokeWidth={1.5} aria-hidden />
          Find…
        </span>
        <Kbd keys="Mod+K" />
      </Button>

      <span className="flex-1" />

      <Button size="sm" variant="primary" icon={Icons.add} onPress={() => openPalette("programs")}>
        New session
      </Button>

      <DialogTrigger>
        <Button size="sm" icon={Icons.host}>
          Your participants: {yourHosts.size === 0 ? "none" : [...yourHosts].join(", ")}
          <Icons.chevronDown size={12} strokeWidth={1.5} className="text-subtle" aria-hidden />
        </Button>
        <Popover placement="bottom end">
          <Dialog
            aria-label="Your participants"
            className="flex w-64 flex-col gap-2 p-2 outline-none"
          >
            <CheckboxGroup
              aria-label="Your participants"
              value={[...yourHosts]}
              onChange={setYourHosts}
              className="flex flex-col gap-1.5"
            >
              {hosts.map((host) => (
                <Checkbox key={host.id} value={host.id}>
                  {host.id}
                </Checkbox>
              ))}
            </CheckboxGroup>
            <p className="text-xs text-subtle">
              You answer the callouts of these Hosts' participants here. This is not an access
              boundary.
            </p>
          </Dialog>
        </Popover>
      </DialogTrigger>

      <span
        role="status"
        data-testid="connection"
        className="inline-flex h-5 items-center gap-1.5 rounded-xs border border-line px-1.5 text-xs text-muted"
      >
        <Dot tone={connectionTone[connection.status] ?? "neutral"} />
        {connection.status}
      </span>

      <IconButton
        icon={Icons.theme}
        label={`Theme: ${theme} (click for ${NEXT_THEME[theme]})`}
        onPress={() => setTheme(NEXT_THEME[theme])}
      />
    </header>
  );
}
