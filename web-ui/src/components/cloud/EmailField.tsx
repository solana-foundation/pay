import { useId } from "react";

interface Props {
  value: string;
  onChange: (value: string) => void;
  disabled?: boolean;
}

/**
 * Tall bordered email input with the "Email" label rendered inside the box
 * at the top-left, placeholder-style.
 */
export function EmailField({ value, onChange, disabled }: Props) {
  const id = useId();
  return (
    <label className="cloud-field" htmlFor={id}>
      <span className="cloud-field-label">Email</span>
      <input
        id={id}
        className="cloud-field-input"
        type="email"
        name="email"
        autoComplete="email"
        autoCapitalize="none"
        spellCheck={false}
        autoFocus
        value={value}
        disabled={disabled}
        onChange={(e) => onChange(e.target.value)}
      />
    </label>
  );
}
