import { useState } from "react";
import type { UpdateSearch } from "~/app/router";
import type { Session } from "~/model";
import { useCall } from "~/sync";
import {
  Button,
  EmptyState,
  Icons,
  type JsonLike,
  JsonView,
  SchemaForm,
  Select,
  validate,
} from "~/ui";

export function QueryPane(props: {
  session: Session;
  host: string | undefined;
  update: UpdateSearch;
}) {
  const { session } = props;
  const queries = session.program?.schema.queries ?? [];
  const host = props.host ?? session.executions[0]?.host;
  const [name, setName] = useState<string | null>(null);
  const [request, setRequest] = useState<JsonLike | undefined>(undefined);
  const run = useCall("query");

  const selected = queries.find((query) => query.name === (name ?? queries[0]?.name));
  const execution = session.executions.find((e) => e.host === host);
  if (selected === undefined || execution === undefined) {
    return <EmptyState icon={Icons.query} title="This program declares no queries" />;
  }
  // A query without a request (unit) has nothing to fill in: an empty form means `null`.
  const value =
    request === undefined && validate(selected.request, null).length === 0 ? null : request;
  const issues = validate(selected.request, value);
  const ready = issues.length === 0;

  function submit() {
    if (execution === undefined || !ready) return;
    run.mutate({ host: execution.host, exec_id: execution.exec_id, query: value ?? null });
  }

  return (
    <div
      className="flex flex-col gap-3 p-3"
      onKeyDown={(event) => {
        if ((event.metaKey || event.ctrlKey) && event.key === "Enter") {
          event.preventDefault();
          submit();
        }
      }}
    >
      <div className="flex items-end gap-2">
        <div className="w-32">
          <Select
            label="Host"
            items={session.executions.map((e) => ({ id: e.host, label: e.host }))}
            value={host ?? null}
            onChange={(id) => props.update({ host: id })}
          />
        </div>
        <div className="w-56">
          <Select
            label="Query"
            items={queries.map((query) => ({ id: query.name, label: query.label }))}
            value={selected.name}
            onChange={(id) => {
              setName(id);
              setRequest(undefined);
              run.reset();
            }}
          />
        </div>
      </div>
      <SchemaForm schema={selected.request} value={request} onChange={setRequest} issues={issues} />
      <div>
        <Button
          variant="primary"
          icon={Icons.play}
          kbd="Mod+Enter"
          isDisabled={!ready || run.isPending}
          onPress={submit}
        >
          Run
        </Button>
      </div>
      {run.error && (
        <div role="alert" className="text-sm text-bad">
          {run.error.message}
        </div>
      )}
      {run.data !== undefined && (
        <section
          aria-label="Query result"
          className="rounded-sm border border-line-soft bg-editor p-2"
        >
          <JsonView value={run.data} collapseDepth={3} />
        </section>
      )}
    </div>
  );
}
