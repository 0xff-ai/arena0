import { EmptyState, Icons } from "~/ui";

/** What the document area shows once every tab is closed. */
export function EmptyWorkspace() {
  return (
    <EmptyState
      icon={Icons.focus}
      title="No open documents"
      body="Open sessions, offers, programs or receipts from the focus button on their Explorer heading."
    />
  );
}
