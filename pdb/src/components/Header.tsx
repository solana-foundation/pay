interface Props {
  theme: "dark" | "light";
  onToggleTheme: () => void;
  sidebarOpen: boolean;
  onToggleSidebar: () => void;
}

export function Header({ theme, onToggleTheme, sidebarOpen, onToggleSidebar }: Props) {
  return (
    <div className="header">
      <svg
        width="24"
        height="24"
        viewBox="0 0 12 12"
        fill="none"
        xmlns="http://www.w3.org/2000/svg"
        aria-hidden="true"
        style={{ flexShrink: 0 }}
      >
        <path
          d="M2.3546 8.4557C2.4215 8.3889 2.5123 8.3507 2.6079 8.3507H11.8035C11.9566 8.3507 12.0331 8.5373 11.9231 8.6473L9.7933 10.7771C9.7264 10.844 9.6356 10.8821 9.54 10.8821H0.344353C0.191253 10.8821 0.114753 10.6955 0.224753 10.5855L2.3546 8.4557Z"
          fill="currentColor"
        />
        <path
          d="M2.3546 1.22098C2.4239 1.15411 2.5147 1.11597 2.6079 1.11597H11.8035C11.9566 1.11597 12.0331 1.30257 11.9231 1.41257L9.7933 3.54237C9.7264 3.60927 9.6356 3.64737 9.54 3.64737H0.344353C0.191253 3.64737 0.114753 3.46077 0.224753 3.35077L2.3546 1.22098Z"
          fill="currentColor"
        />
        <path
          d="M9.7933 4.8196C9.7264 4.7527 9.6356 4.7146 9.54 4.7146H0.344353C0.191253 4.7146 0.114753 4.9012 0.224753 5.0112L2.3546 7.141C2.4215 7.2079 2.5123 7.246 2.6079 7.246H11.8035C11.9566 7.246 12.0331 7.0594 11.9231 6.9494L9.7933 4.8196Z"
          fill="currentColor"
        />
      </svg>
      <h1>
        payment <span className="debugger">debugger</span>
      </h1>
      <div style={{ flex: 1 }} />
      <button
        className="theme-toggle"
        onClick={onToggleSidebar}
        aria-label={sidebarOpen ? "Hide sidebar" : "Show sidebar"}
        title={sidebarOpen ? "Hide sidebar" : "Show sidebar"}
      >
        <svg width="16" height="16" viewBox="0 0 16 16" fill="none">
          <rect x="1" y="2" width="14" height="12" rx="2" stroke="currentColor" strokeWidth="1.5" />
          <line x1="10.5" y1="2" x2="10.5" y2="14" stroke="currentColor" strokeWidth="1.5" opacity={sidebarOpen ? 1 : 0.4} />
        </svg>
      </button>
      <button
        className="theme-toggle"
        onClick={onToggleTheme}
        aria-label={`Switch to ${theme === "dark" ? "light" : "dark"} mode`}
        title={`Switch to ${theme === "dark" ? "light" : "dark"} mode`}
      >
        {theme === "dark" ? (
          <svg width="16" height="16" viewBox="0 0 16 16" fill="none">
            <circle cx="8" cy="8" r="3.5" stroke="currentColor" strokeWidth="1.5" />
            <path d="M8 1.5v1M8 13.5v1M1.5 8h1M13.5 8h1M3.4 3.4l.7.7M11.9 11.9l.7.7M3.4 12.6l.7-.7M11.9 4.1l.7-.7" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" />
          </svg>
        ) : (
          <svg width="16" height="16" viewBox="0 0 16 16" fill="none">
            <path d="M14 9.6A6.5 6.5 0 016.4 2 6 6 0 1014 9.6z" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" />
          </svg>
        )}
      </button>
    </div>
  );
}
