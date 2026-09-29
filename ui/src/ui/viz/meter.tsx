/** How many of a session's participants have signed: filled cells signed, hollow cells still missing. */
export function AgreementMeter(props: { signers: number[]; participants: number }) {
  const signed = props.signers.length;
  return (
    <span
      role="img"
      aria-label={`${signed} of ${props.participants} signed`}
      className="inline-flex shrink-0 gap-px align-middle"
    >
      {Array.from({ length: props.participants }, (_, participant) => (
        <span
          key={participant}
          className={
            props.signers.includes(participant)
              ? "h-2.5 w-1.5 bg-fg/70"
              : "h-2.5 w-1.5 border border-line"
          }
        />
      ))}
    </span>
  );
}
