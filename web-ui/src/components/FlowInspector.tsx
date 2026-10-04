import { useState } from "react";
import type { ReactNode } from "react";
import type { PaymentFlow } from "../types";
import { EventLog } from "./EventLog";
import { ReceiptLink } from "./ReceiptLink";
import { ChallengeHeaders } from "./ChallengeHeaders";

type Tab =
  | "inference"
  | "request"
  | "payment"
  | "response"
  | "events";

function prettyBody(body: string): string {
  try {
    return JSON.stringify(JSON.parse(body), null, 2);
  } catch {
    return body;
  }
}

function HeaderTable({
  headers,
  empty,
}: {
  headers?: Record<string, string>;
  empty: string;
}) {
  const entries = Object.entries(headers ?? {}).sort(([a], [b]) =>
    a.localeCompare(b),
  );
  if (entries.length === 0) return <p className="inspector-empty">{empty}</p>;

  return (
    <dl className="header-table">
      {entries.map(([name, value]) => (
        <div className="header-row" key={name}>
          <dt>{name}</dt>
          <dd className={value === "[REDACTED]" ? "redacted" : ""}>
            {value}
          </dd>
        </div>
      ))}
    </dl>
  );
}

function Fact({ label, value }: { label: string; value?: string | number }) {
  if (value == null || value === "") return null;
  return (
    <div className="inspector-fact">
      <span>{label}</span>
      <strong title={String(value)}>{value}</strong>
    </div>
  );
}

function RequestPanel({ flow }: { flow: PaymentFlow }) {
  return (
    <div className="inspector-scroll">
      <div className="inspector-facts">
        <Fact label="Method" value={flow.method ?? "—"} />
        <Fact label="Resource" value={flow.resource} />
        <Fact label="Client" value={flow.clientIp} />
      </div>
      <h4>Request headers</h4>
      <HeaderTable
        headers={flow.paymentHeaders}
        empty="No request headers were captured for this exchange."
      />
      <h4>Request body</h4>
      {flow.requestBody ? (
        <pre className="response-body">{prettyBody(flow.requestBody)}</pre>
      ) : (
        <p className="inspector-empty">No request body captured.</p>
      )}
    </div>
  );
}

function PaymentPanel({
  flow,
  visualization,
}: {
  flow: PaymentFlow;
  visualization?: ReactNode;
}) {
  const payment = flow.payment;
  const session = flow.session;
  const hasDetails = payment || session || flow.amount || flow.payer;
  return (
    <div className="inspector-scroll payment-panel">
      {hasDetails ? (
        <div className="inspector-facts payment-facts">
          <Fact label="Protocol" value={flow.protocol.toUpperCase()} />
          <Fact label="Scheme" value={flow.scheme} />
          <Fact label="Action" value={payment?.action ?? session?.action} />
          <Fact label="Amount" value={flow.amount} />
          <Fact label="Payer" value={flow.payer ?? session?.payer} />
          <Fact label="Recipient" value={payment?.recipient ?? session?.recipient} />
          <Fact label="Network" value={payment?.network} />
          <Fact label="Asset" value={payment?.asset ?? session?.currency} />
          <Fact
            label="Channel"
            value={payment?.channelId ?? session?.sessionId}
          />
          <Fact
            label="Deposit"
            value={payment?.depositAmount ?? session?.deposit}
          />
          <Fact
            label="Authorized"
            value={payment?.authorizedAmount ?? session?.approvedAmount}
          />
          <Fact
            label="Voucher"
            value={payment?.voucherAmount ?? session?.cumulative}
          />
          <Fact label="Vouchers" value={session?.voucherCount} />
          <Fact label="Settled" value={payment?.settlementAmount} />
          <Fact label="Receipt" value={payment?.receiptStatus} />
        </div>
      ) : (
        <p className="inspector-empty">No payment metadata for this exchange.</p>
      )}
      {payment?.settlementReference && (
        <ReceiptLink flow={flow} label="Open settlement receipt" />
      )}
      {visualization}
      <h4>402 challenge headers</h4>
      <ChallengeHeaders headers={flow.challengeHeaders} />
      <p className="inspector-security-note">
        Signatures, authorization tokens, receipts, and cookies are redacted.
      </p>
    </div>
  );
}

function ResponsePanel({ flow }: { flow: PaymentFlow }) {
  return (
    <div className="inspector-scroll">
      <div className="inspector-facts">
        <Fact label="Status" value={flow.responseStatus ?? "—"} />
        <Fact label="Duration" value={`${flow.durationMs} ms`} />
      </div>
      <h4>Response headers</h4>
      <HeaderTable
        headers={flow.responseHeaders}
        empty="No response headers captured yet."
      />
      <h4>Response body</h4>
      {flow.responseBody ? (
        <pre className="response-body">{prettyBody(flow.responseBody)}</pre>
      ) : (
        <p className="inspector-empty">No response body captured.</p>
      )}
    </div>
  );
}

export function FlowInspector({
  flow,
  inference,
  paymentVisualization,
}: {
  flow: PaymentFlow;
  inference?: ReactNode;
  paymentVisualization?: ReactNode;
}) {
  const [tab, setTab] = useState<Tab>(
    inference
      ? "inference"
      : flow.protocol === "http" && !flow.payment
        ? "request"
        : "payment",
  );
  const tabs: Array<{ id: Tab; label: string; count?: number }> = [
    ...(inference ? [{ id: "inference" as const, label: "Inference" }] : []),
    { id: "payment", label: "Payment" },
    { id: "request", label: "Request" },
    { id: "response", label: "Response" },
    { id: "events", label: "Events", count: flow.events.length },
  ];
  const selectedTab = tabs.some((item) => item.id === tab)
    ? tab
    : tabs[0].id;

  return (
    <section className="detail-panel" aria-label="Flow inspector">
      <div className="detail-tabs" role="tablist" aria-label="Flow details">
        {tabs.map((item) => (
          <button
            className={`detail-tab${selectedTab === item.id ? " active" : ""}`}
            type="button"
            role="tab"
            aria-selected={selectedTab === item.id}
            aria-controls={`flow-panel-${item.id}`}
            id={`flow-tab-${item.id}`}
            onClick={() => setTab(item.id)}
            key={item.id}
          >
            {item.label}
            {item.count != null && (
              <span className="tab-count">{item.count}</span>
            )}
          </button>
        ))}
      </div>
      <div
        className="detail-tab-panel"
        role="tabpanel"
        id={`flow-panel-${selectedTab}`}
        aria-labelledby={`flow-tab-${selectedTab}`}
      >
        {selectedTab === "inference" && inference}
        {selectedTab === "payment" && (
          <PaymentPanel flow={flow} visualization={paymentVisualization} />
        )}
        {selectedTab === "request" && <RequestPanel flow={flow} />}
        {selectedTab === "response" && <ResponsePanel flow={flow} />}
        {selectedTab === "events" && <EventLog events={flow.events} />}
      </div>
    </section>
  );
}
