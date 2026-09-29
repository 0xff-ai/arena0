import type { ReactNode } from "react";

/**
 * The frame of the program, Host and receipt documents: a header that stays
 * in view while the body scrolls under it (`main` is the scroll container).
 */
export function DocPage(props: {
  title: ReactNode;
  meta?: ReactNode;
  actions?: ReactNode;
  children: ReactNode;
}) {
  return (
    <div className="pb-10">
      <div className="sticky top-0 z-2 flex min-h-11 items-center gap-2.5 border-b border-line-soft bg-editor px-4 py-2">
        <h1 className="m-0 min-w-0 truncate font-mono text-lg font-medium text-fg">
          {props.title}
        </h1>
        {props.meta}
        <span className="flex-1" />
        {props.actions}
      </div>
      <div className="flex flex-col gap-5 px-4 py-4">{props.children}</div>
    </div>
  );
}

/** A labelled block of a document. */
export function DocSection(props: { title: string; note?: ReactNode; children: ReactNode }) {
  return (
    <section className="flex min-w-0 flex-col gap-1.5">
      <h2 className="m-0 flex items-baseline gap-2 text-xs font-medium tracking-wide text-subtle uppercase">
        {props.title}
        {props.note && <span className="font-normal normal-case">{props.note}</span>}
      </h2>
      {props.children}
    </section>
  );
}
