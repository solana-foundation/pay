import { PayWordmark } from "./PayWordmark";

export interface ProgressLine {
  text: string;
  state: "done" | "active" | "error";
}

interface Props {
  title: string;
  lines: ProgressLine[];
}

/** Terminal-styled progress log, used while a provider callback completes. */
export function TerminalProgress({ title, lines }: Props) {
  return (
    <section className="cloud-term" aria-label={title}>
      <div className="cloud-term-banner">
        <PayWordmark />
        <div className="cloud-term-tagline">Toolchain for agentic payments</div>
      </div>
      <div className="cloud-term-lines" role="status" aria-live="polite">
        <div className="cloud-term-line">
          <span className="cloud-term-prompt">$</span> {title}
        </div>
        {lines.map((line, i) => (
          <div
            key={i}
            className={
              line.state === "error"
                ? "cloud-term-line cloud-term-line--error"
                : line.state === "done"
                  ? "cloud-term-line cloud-term-line--muted"
                  : "cloud-term-line"
            }
          >
            {line.state === "done" ? "✔ " : line.state === "error" ? "✖ " : "… "}
            {line.text}
          </div>
        ))}
      </div>
    </section>
  );
}
