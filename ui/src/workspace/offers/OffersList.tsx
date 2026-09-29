import { fmtAgo, useNow, useRows } from "~/model";
import { DaemonError, type OfferRow, useCall, useCollections } from "~/sync";
import {
  type Column,
  DataTable,
  EmptyState,
  HashChip,
  Icons,
  MenuButton,
  MenuItem,
  Toolbar,
  toasts,
} from "~/ui";

const ROW_HEIGHT = 30;

export function OffersList() {
  const collections = useCollections();
  const offers = useRows(collections.offers);
  const hosts = useRows(collections.hosts);
  const programs = useRows(collections.programs);
  const executions = useRows(collections.executions);
  const now = useNow();
  const join = useCall("join");

  const hostOfPeer = (peer: string) => hosts.find((host) => host.peer_id === peer)?.id;
  const sorted = [...offers].sort((a, b) => b.first_seen_ms - a.first_seen_ms);

  const joinFrom = (offer: OfferRow, host: string) =>
    join.mutate(
      {
        host,
        program: offer.program,
        target: { creator: offer.creator, negotiation_id: offer.negotiation_id },
        blobs: [],
      },
      {
        onSuccess: () => toasts.show({ tone: "ok", title: `Joined from ${host}` }),
        onError: (error: DaemonError) =>
          toasts.show({ tone: "bad", title: `Could not join from ${host}`, body: error.message }),
      },
    );

  const columns: Column<OfferRow>[] = [
    {
      id: "program",
      title: "Program",
      width: 200,
      minWidth: 200,
      isRowHeader: true,
      render: (offer) =>
        programs.find((program) => program.hash === offer.program)?.display_name ?? (
          <HashChip hash={offer.program} />
        ),
    },
    {
      id: "creator",
      title: "Creator",
      width: 140,
      minWidth: 140,
      render: (offer) => {
        const local = hostOfPeer(offer.creator);
        return local ?? <HashChip hash={offer.creator} />;
      },
    },
    {
      id: "participants",
      title: "Participants",
      width: 80,
      minWidth: 80,
      align: "end",
      render: (offer) => <span className="font-mono text-xs tabular">{offer.target_size}</span>,
    },
    {
      id: "seen-by",
      title: "Seen by",
      width: "1fr",
      minWidth: 120,
      render: (offer) => <span className="truncate">{offer.seen_by.join(", ")}</span>,
    },
    {
      id: "deadline",
      title: "Deadline",
      width: 96,
      minWidth: 96,
      align: "end",
      render: (offer) => (
        <span className="font-mono text-xs text-subtle">
          {offer.deadline_ms > now
            ? `in ${Math.ceil((offer.deadline_ms - now) / 1000)}s`
            : fmtAgo(offer.deadline_ms, now)}
        </span>
      ),
    },
    {
      id: "first",
      title: "First seen",
      width: 96,
      minWidth: 96,
      align: "end",
      render: (offer) => (
        <span className="font-mono text-xs text-subtle">{fmtAgo(offer.first_seen_ms, now)}</span>
      ),
    },
    {
      id: "join",
      title: "",
      width: 120,
      minWidth: 120,
      align: "end",
      render: (offer) => {
        const inNegotiation = new Set(
          executions
            .filter((execution) => execution.negotiation_id === offer.negotiation_id)
            .map((execution) => execution.host),
        );
        const candidates = hosts.filter((host) => !inNegotiation.has(host.id));
        if (candidates.length === 0) return null;
        return (
          <MenuButton label="Join from…">
            {candidates.map((host) => (
              <MenuItem key={host.id} id={host.id} onAction={() => joinFrom(offer, host.id)}>
                {host.id}
              </MenuItem>
            ))}
          </MenuButton>
        );
      },
    },
  ];

  return (
    <div className="flex h-full min-h-0 flex-col">
      <Toolbar aria-label="Offers">
        <span className="text-base text-fg">Offers</span>
        <span className="font-mono text-xs text-subtle tabular">{offers.length}</span>
        <span className="text-sm text-subtle">
          Offers seen on program topics that no local Host has joined
        </span>
      </Toolbar>
      <div className="min-h-0 flex-1">
        <DataTable
          label="Offers"
          columns={columns}
          rows={sorted}
          getKey={(offer) => offer.key}
          rowHeight={ROW_HEIGHT}
          empty={
            <EmptyState
              icon={Icons.offer}
              title="No offers seen"
              body="Offers are seen live on program topics: one appears here when a peer publishes it while this workspace is open."
            />
          }
        />
      </div>
    </div>
  );
}
