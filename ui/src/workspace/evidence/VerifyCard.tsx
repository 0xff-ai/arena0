import type { VerifyTarget } from "~/sync";
import { EmptyState, Icons } from "~/ui";

export function VerifyCard(_props: {
  host: string;
  target: VerifyTarget;
  expect?: { program?: string; sessionId?: string; participants?: string[] };
}) {
  return <EmptyState icon={Icons.verify} title="Verify — W4" />;
}
