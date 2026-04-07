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
      <div className={`status-dot ${status}`}>
        {status === "resource-delivered" && (
          <svg width="8" height="8" viewBox="0 0 8 8" fill="none">
            <path d="M1.5 4L3.2 5.7L6.5 2.3" stroke="white" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" />
          </svg>
        )}
        {status === "failed" && (
          <svg width="8" height="8" viewBox="0 0 8 8" fill="none">
            <path d="M2 2L6 6M6 2L2 6" stroke="white" strokeWidth="1.5" strokeLinecap="round" />
          </svg>
        )}
      </div>
      <span className={`status-label ${status}`}>{labels[status]}</span>
    </div>
  );
}
