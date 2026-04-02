import type { FlowStatus } from "../types";

const labels: Record<FlowStatus, string> = {
  "payment-required": "Payment Required",
  "payment-received": "Payment Received",
  "resource-delivered": "Resource Delivered",
  failed: "Failed",
};

export function StatusIndicator({ status }: { status: FlowStatus }) {
  return (
    <div className="status-indicator">
      <div className={`status-dot ${status}`} />
      <span className={`status-label ${status}`}>{labels[status]}</span>
    </div>
  );
}
