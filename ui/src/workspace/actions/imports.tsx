import { useState } from "react";
import { FileTrigger } from "react-aria-components";
import type { JsonValue } from "~/api/types.gen";
import { shortHash, useRows } from "~/model";
import { useCall, useCollections } from "~/sync";
import { Button, Dialog, IconButton, Icons, Select, TextField, toasts } from "~/ui";

// Import and Host actions shared by the Explorer headers (compact) and the
// list toolbars.

const DEFAULT_USER_AGENT = "arena0-ui";

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function PickerButton(props: { compact?: boolean; label: string; isDisabled?: boolean }) {
  return props.compact ? (
    <IconButton icon={Icons.upload} label={props.label} isDisabled={props.isDisabled} />
  ) : (
    <Button icon={Icons.upload} isDisabled={props.isDisabled}>
      {props.label}
    </Button>
  );
}

/**
 * `.wasm` file picker → `program_import` on every Host, one Host at a time so
 * each Host's outcome (or error) gets its own toast and a failing Host does
 * not hide the others.
 */
export function ImportProgramButton(props: { compact?: boolean }) {
  const hosts = useRows(useCollections().hosts);
  const importProgram = useCall("program_import");
  const [busy, setBusy] = useState(false);

  async function run(files: FileList | null) {
    const file = files?.item(0);
    if (!file) return;
    setBusy(true);
    try {
      for (const host of hosts) {
        try {
          const imported = await importProgram.mutateAsync({ hosts: [host.id], file });
          toasts.show({
            tone: "ok",
            title: "Program imported",
            body: `${shortHash(imported.hash)} on ${host.id}`,
          });
        } catch (error) {
          toasts.show({
            tone: "bad",
            title: `Program import failed on ${host.id}`,
            body: errorText(error),
          });
        }
      }
    } finally {
      setBusy(false);
    }
  }

  return (
    <FileTrigger acceptedFileTypes={[".wasm"]} onSelect={run}>
      <PickerButton compact={props.compact} label="Import program" isDisabled={busy} />
    </FileTrigger>
  );
}

/** Receipt JSON file → Host `Select` in a Dialog → `receipt_import`. */
export function ImportReceiptButton(props: { compact?: boolean }) {
  const hosts = useRows(useCollections().hosts);
  const importReceipt = useCall("receipt_import");
  const [artifact, setArtifact] = useState<{ name: string; value: JsonValue } | null>(null);
  const [host, setHost] = useState<string | null>(null);

  async function pick(files: FileList | null) {
    const file = files?.item(0);
    if (!file) return;
    // The file is user input: a parse failure is reported, never thrown.
    try {
      setArtifact({ name: file.name, value: JSON.parse(await file.text()) });
      setHost(hosts[0]?.id ?? null);
    } catch (error) {
      toasts.show({
        tone: "bad",
        title: `${file.name} is not JSON`,
        body: errorText(error),
      });
    }
  }

  async function submit() {
    if (artifact === null || host === null) return;
    try {
      const imported = await importReceipt.mutateAsync({
        host,
        // The daemon authenticates the artifact; its shape is not ours to check.
        artifact: artifact.value,
      });
      toasts.show({
        tone: "ok",
        title: "Receipt imported",
        body: `${shortHash(imported.receipt_id)} on ${host}`,
      });
      setArtifact(null);
    } catch (error) {
      toasts.show({
        tone: "bad",
        title: `Receipt import failed on ${host}`,
        body: errorText(error),
      });
    }
  }

  return (
    <>
      <FileTrigger acceptedFileTypes={[".json"]} onSelect={pick}>
        <PickerButton compact={props.compact} label="Import receipt" />
      </FileTrigger>
      <Dialog
        title="Import receipt"
        size="sm"
        isOpen={artifact !== null}
        onOpenChange={(open) => {
          if (!open) setArtifact(null);
        }}
        footer={
          <>
            <Button onPress={() => setArtifact(null)}>Cancel</Button>
            <Button
              variant="primary"
              isDisabled={host === null || importReceipt.isPending}
              onPress={submit}
            >
              Import
            </Button>
          </>
        }
      >
        <div className="flex flex-col gap-3">
          <div className="text-muted">
            Import <span className="font-mono text-fg">{artifact?.name}</span> into a Host.
          </div>
          <Select
            label="Host"
            items={hosts.map((h) => ({ id: h.id, label: h.id }))}
            value={host}
            onChange={setHost}
          />
        </div>
      </Dialog>
    </>
  );
}

/** Dialog with optional id and user agent → `host_open`. */
export function AddHostButton(props: { compact?: boolean }) {
  const openHost = useCall("host_open");
  const [isOpen, setOpen] = useState(false);
  const [id, setId] = useState("");
  const [userAgent, setUserAgent] = useState("");

  async function submit() {
    try {
      const host = await openHost.mutateAsync({
        id: id.trim() === "" ? null : id.trim(),
        user_agent: userAgent.trim() === "" ? DEFAULT_USER_AGENT : userAgent.trim(),
      });
      toasts.show({ tone: "ok", title: "Host added", body: host.id });
      setOpen(false);
      setId("");
      setUserAgent("");
    } catch (error) {
      toasts.show({ tone: "bad", title: "Could not add the Host", body: errorText(error) });
    }
  }

  return (
    <>
      {props.compact ? (
        <IconButton icon={Icons.add} label="Add Host" onPress={() => setOpen(true)} />
      ) : (
        <Button icon={Icons.add} onPress={() => setOpen(true)}>
          Add Host
        </Button>
      )}
      <Dialog
        title="Add Host"
        size="sm"
        isOpen={isOpen}
        onOpenChange={setOpen}
        footer={
          <>
            <Button onPress={() => setOpen(false)}>Cancel</Button>
            <Button variant="primary" isDisabled={openHost.isPending} onPress={submit}>
              Add Host
            </Button>
          </>
        }
      >
        <form
          className="flex flex-col gap-3"
          onSubmit={(event) => {
            event.preventDefault();
            void submit();
          }}
        >
          <TextField
            label="Id"
            description="Optional. The daemon picks the next free host-NN."
            placeholder="host-03"
            value={id}
            onChange={setId}
            mono
          />
          <TextField
            label="User agent"
            description={`Optional. Defaults to ${DEFAULT_USER_AGENT}.`}
            value={userAgent}
            onChange={setUserAgent}
          />
        </form>
      </Dialog>
    </>
  );
}
