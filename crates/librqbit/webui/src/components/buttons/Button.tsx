import { ReactNode } from "react";

export const Button: React.FC<{
  onClick: () => void;
  variant?: "cancel" | "primary" | "secondary" | "danger" | "none";
  size?: "sm" | "default";
  className?: string;
  disabled?: boolean;
  children: ReactNode;
}> = ({ onClick, children, className, disabled, variant, size }) => {
  const sizeClassNames = {
    sm: "text-xs px-2 py-1",
    default: "text-sm px-3 py-1.5",
  }[size ?? "default"];

  const variantClassNames = {
    secondary:
      "bg-surface-raised text-text border-border hover:bg-primary hover:text-white transition-colors",
    danger:
      "bg-error-bg text-white border-transparent hover:bg-red-600 transition-colors",
    primary:
      "bg-primary-bg text-white border-transparent hover:bg-primary-bg-hover disabled:opacity-50 transition-colors",
    cancel:
      "bg-surface-raised text-text-secondary border-border hover:bg-surface-sunken transition-colors",
    none: "",
  }[variant ?? "secondary"];

  return (
    <button
      disabled={disabled}
      onClick={(e) => {
        e.preventDefault();
        onClick();
      }}
      className={`inline-flex items-center gap-1 border rounded font-medium disabled:cursor-not-allowed ${sizeClassNames} ${variantClassNames} ${className ?? ""}`}
    >
      {children}
    </button>
  );
};
