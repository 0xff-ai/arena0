import { EmptyState, Icons } from "~/ui";
import { useComposer } from "./store";

export function ComposerDock() {
  const { calloutKey } = useComposer();
  if (calloutKey === null) return null;
  return <EmptyState icon={Icons.callout} title="Composer — W5" />;
}
