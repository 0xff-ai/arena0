import type { SessionSearch } from "~/app/router";

/**
 * Changes some of the document's search parameters and keeps the others. A key
 * present with `undefined` clears that parameter. Every sub tab reads and
 * writes its state here, so the address always reproduces what is shown.
 */
export type UpdateSearch = (patch: Partial<SessionSearch>) => void;
