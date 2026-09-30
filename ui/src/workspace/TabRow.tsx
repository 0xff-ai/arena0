import { useRouter } from "@tanstack/react-router";
import { useEffect } from "react";
import { shortHash, useRows, useSessions } from "~/model";
import { useCollections } from "~/sync";
import { type DocTab, DocTabs, Icons } from "~/ui";
import { shortSessionKey } from "./common/SessionLabel";
import { docHref, useActiveDoc, useCloseDoc } from "./nav";
import { type DocRef, docId, type ListName, useDocTabs } from "./tabs";

export const LIST_TABS: Record<ListName, { label: string; icon: typeof Icons.session }> = {
  offers: { label: "Offers", icon: Icons.offer },
  sessions: { label: "Sessions", icon: Icons.session },
  programs: { label: "Programs", icon: Icons.program },
  receipts: { label: "Receipts", icon: Icons.receipt },
};

export function TabRow() {
  const router = useRouter();
  const collections = useCollections();
  const { sessions } = useSessions();
  const offers = useRows(collections.offers);
  const receipts = useRows(collections.receipts);
  const programs = useRows(collections.programs);
  const tabs = useDocTabs();
  const activeDoc = useActiveDoc();
  const close = useCloseDoc();

  // A document reached by URL (a pasted link, a reload of a closed tab) gets
  // its tab; a list's tab is never a preview.
  const activeId = activeDoc ? docId(activeDoc) : null;
  useEffect(() => {
    if (activeDoc) tabs.open(activeDoc, activeDoc.kind === "list");
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
      case "list":
        return {
          id: docId(ref),
          label: LIST_TABS[ref.list].label,
          icon: LIST_TABS[ref.list].icon,
          detail: String(counts[ref.list]),
          preview,
        };
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

  const items: DocTab[] = tabs.docs.map((doc) => docTab(doc.ref, doc.preview));

  const byId = (id: string) => tabs.docs.find((doc) => docId(doc.ref) === id)?.ref;

  return (
    <DocTabs
      tabs={items}
      activeId={activeId}
      onSelect={(id) => {
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
