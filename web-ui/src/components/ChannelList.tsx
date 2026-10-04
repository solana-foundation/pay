import type { PaymentChannel } from "../lib/channels";
import { shortAddr } from "../lib/format";
import type { PaymentFlow, ProviderSummary } from "../types";
import { FlowDetail } from "./FlowDetail";
import { ProtocolBadge } from "./ProtocolBadge";
import { StatusIndicator } from "./StatusIndicator";

function time(value: string): string {
  return new Date(value).toLocaleTimeString(undefined, {
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
    fractionalSecondDigits: 3,
    hour12: false,
  });
}

function capacityLabel(percent: number | undefined): string {
  if (percent === undefined) return "Capacity requires a captured deposit";
  return `${percent.toFixed(percent < 1 ? 2 : percent % 1 === 0 ? 0 : 1)}% used`;
}

function requestDescription(flow: PaymentFlow): string | undefined {
  return flow.inference?.model ?? flow.inference?.provider;
}

interface Props {
  readonly channels: PaymentChannel[];
  readonly selectedId: string | null;
  readonly onSelect: (id: string | null) => void;
  readonly providers?: ProviderSummary[];
}

/** Render payment activity grouped by channel instead of by individual flow. */
export function ChannelList({ channels, selectedId, onSelect, providers }: Props) {
  if (channels.length === 0) {
    return (
      <div className="flow-list">
        <div className="flow-empty">
          Channels appear after a request opens or uses a captured payment channel.
        </div>
      </div>
    );
  }

  return (
    <div className="channel-list-view">
      {channels.map((channel) => (
        <section className="channel-card" key={channel.id}>
          <header className="channel-header">
            <div className="channel-identity">
              <ProtocolBadge protocol={channel.protocol} scheme={channel.scheme} />
              <div>
                <h3 title={channel.id}>{shortAddr(channel.id)}</h3>
                <span>{channel.id}</span>
              </div>
            </div>
            <div className={`channel-state ${channel.state}`}>
              <span aria-hidden="true" />
              {channel.state}
            </div>
          </header>

          <div className="channel-capacity">
            <div className="channel-capacity-primary">
              <span>Available capacity</span>
              <strong>{channel.remaining ?? "Unknown"}</strong>
            </div>
            <dl className="channel-metrics">
              <div>
                <dt>Funded</dt>
                <dd>{channel.deposited ?? "Not captured"}</dd>
              </div>
              <div>
                <dt>Claimable</dt>
                <dd>{channel.consumed ?? "No voucher yet"}</dd>
              </div>
              <div>
                <dt>Requests</dt>
                <dd>{channel.requests.length}</dd>
              </div>
              <div>
                <dt>Updated</dt>
                <dd>{time(channel.updatedAt)}</dd>
              </div>
            </dl>
            <div className="channel-capacity-track" aria-label={capacityLabel(channel.usagePercent)}>
              <span style={{ width: `${channel.usagePercent ?? 0}%` }} />
            </div>
            <p>{capacityLabel(channel.usagePercent)}</p>
          </div>

          <div className="channel-parties">
            <span>Payer <code title={channel.payer}>{shortAddr(channel.payer)}</code></span>
            <span>Recipient <code title={channel.recipient}>{shortAddr(channel.recipient)}</code></span>
          </div>

          <div className="channel-requests">
            <h4>Requests</h4>
            {channel.requests.map(({ flow, action, actionLabel, amount }) => {
              const selected = selectedId === flow.id;
              return (
                <div className="channel-request" key={flow.id}>
                  <button
                    className={`channel-request-row${selected ? " selected" : ""}`}
                    type="button"
                    aria-expanded={selected}
                    onClick={() => onSelect(selected ? null : flow.id)}
                  >
                    <time>{time(flow.startedAt)}</time>
                    {actionLabel ? (
                      <span className={`channel-action ${action ?? "other"}`}>
                        {actionLabel}
                      </span>
                    ) : (
                      <span className="channel-action request">Request</span>
                    )}
                    <span className="channel-request-resource">
                      <strong>{flow.resource}</strong>
                      {requestDescription(flow) && <small>{requestDescription(flow)}</small>}
                    </span>
                    {amount && <code className="channel-request-amount">{amount}</code>}
                    <StatusIndicator status={flow.status} />
                  </button>
                  {selected && <FlowDetail flow={flow} providers={providers} />}
                </div>
              );
            })}
          </div>
        </section>
      ))}
    </div>
  );
}
