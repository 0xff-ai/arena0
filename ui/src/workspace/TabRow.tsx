import { useRouter, useRouterState } from "@tanstack/react-router";
import { useEffect } from "react";
import { shortHash, useRows, useSessions } from "~/model";
import { useCollections } from "~/sync";
import { type DocTab, DocTabs, Icons } from "~/ui";
import { shortSessionKey } from "./common/SessionLabel";
import { docHref, useActiveDoc, useCloseDoc } from "./nav";
import { type DocRef, docId, useDocTabs } from "./tabs";

const FIXED = [
  { id: "sessions", label: "Sessions", icon: Icons.session, path: "/sessions" },
  { id: "offers", label: "Offers", icon: Icons.offer, path: "/offers" },
  { id: "receipts", label: "Receipts", icon: Icons.receipt, path: "/receipts" },
  { id: "programs", label: "Programs", icon: Icons.program, path: "/programs" },
] as const;

/** The list route the current path belongs to, when the leaf route is a list. */
function useActiveList(): string | null {
  const leaf = useRouterState({ select: (state) => state.matches.at(-1)?.routeId });
  const match = FIXED.find((fixed) => leaf === fixed.path);
  return match ? `fixed:${match.id}` : null;
}

export function TabRow() {
  const router = useRouter();
  const collections = useCollections();
  const { sessions } = useSessions();
  const offers = useRows(collections.offers);
  const receipts = useRows(collections.receipts);
  const programs = useRows(collections.programs);
  const tabs = useDocTabs();
  const activeDoc = useActiveDoc();
  const activeList = useActiveList();
  const close = useCloseDoc();

  // A document reached by URL (a pasted link, a reload of a closed tab) gets its tab.
  const activeId = activeDoc ? docId(activeDoc) : null;
  useEffect(() => {
    if (activeDoc) tabs.open(activeDoc, false);
  }, [activeId]);

  const counts = {
    sessions: sessions.length,
    offers: offers.length,
    receipts: receipts.length,
    programs: programs.length,
  };
  const programName = (hash: string) =>
    programs.find((program) => program.hash === hash)?.display_name ?? shortHash(hash);

  const docTab = (ref: DocRef, preview: boolean): DocTab => {
    switch (ref.kind) {
      case "session": {
        const session = sessions.find((candidate) => candidate.key === ref.key);
        return {
          id: docId(ref),
          label: session?.program?.display_name ?? "session",
          detail: shortSessionKey(ref.key),
          icon: Icons.session,
          preview,
        };
      }
      case "program":
        return { id: docId(ref), label: programName(ref.hash), icon: Icons.program, preview };
      case "host":
        return { id: docId(ref), label: ref.id, icon: Icons.host, preview };
      case "receipt": {
        const receipt = receipts.find((r) => r.host === ref.host && r.receipt_id === ref.id);
        return {
          id: docId(ref),
          label: shortHash(ref.id),
          icon: receipt?.kind === "stop_report" ? Icons.stopReport : Icons.receipt,
          preview,
        };
      }
    }
  };

  const items: DocTab[] = [
    ...FIXED.map((fixed) => ({
      id: `fixed:${fixed.id}`,
      label: fixed.label,
      icon: fixed.icon,
      detail: String(counts[fixed.id]),
      fixed: true,
    })),
    ...tabs.docs.map((doc) => docTab(doc.ref, doc.preview)),
  ];

  const byId = (id: string) => tabs.docs.find((doc) => docId(doc.ref) === id)?.ref;

  return (
    <DocTabs
      tabs={items}
      activeId={activeId ?? activeList}
      onSelect={(id) => {
        const fixed = FIXED.find((candidate) => `fixed:${candidate.id}` === id);
        if (fixed) {
          router.history.push(fixed.path);
          return;
        }
        const ref = byId(id);
        if (ref) router.history.push(docHref(ref));
      }}
      onClose={(id) => {
        const ref = byId(id);
        if (ref) close(ref);
      }}
      onPin={(id) => {
        const ref = byId(id);
        if (ref) tabs.pin(ref);
      }}
    />
  );
}
