import { ReactNode } from "react";

export const Button: React.FC<{
  onClick: () => void;
  variant?: "cancel" | "primary" | "secondary" | "danger" | "none";
  className?: string;
  disabled?: boolean;
  children: ReactNode;
}> = ({ onClick, children, className, disabled, variant }) => {
  let variantClassNames = {
    secondary:
      "hover:bg-blue-500 transition-colors hover:text-white dark:hover:bg-blue-900/50",
    danger:
      "bg-red-400 text-white border-green-50 hover:border-red-700 hover:bg-red-600 dark:bg-red-800 dark:border-none dark:hover:bg-red-900",
    primary:
      "bg-blue-600 text-white hover:bg-blue-800 disabled:bg-blue-200 dark:disabled:bg-slate-600 dark:disabled:text-slate-300 dark:border-none",
    cancel:
      "hover:bg-slate-200 dark:bg-slate-600 dark:hover:bg-slate-700 dark:border-none",
    none: "",
  }[variant ?? "secondary"];
  return (
    <button
      disabled={disabled}
      onClick={(e) => {
        e.preventDefault();
        onClick();
      }}
      className={`inline-flex items-center gap-1 border rounded-lg  disabled:cursor-not-allowed px-2 py-1 dark:border-slate-700 ${variantClassNames} ${className}`}
    >
      {children}
    </button>
  );
};
