import { ReactNode } from "react";

export const Button: React.FC<{
  onClick: () => void;
  variant?: "cancel" | "primary" | "secondary" | "danger" | "none";
  className?: string;
  disabled?: boolean;
  children: ReactNode;
}> = ({ onClick, children, className, disabled, variant }) => {
  let variantClassNames = {
    secondary: "hover:bg-blue-500 transition-colors hover:text-white",
    danger:
      "bg-red-400 text-white border-green-50 hover:border-red-700 hover:bg-red-600",
    primary: "bg-blue-600 text-white hover:bg-blue-800 disabled:bg-blue-200",
    cancel: "hover:bg-slate-200",
    none: "",
  }[variant ?? "secondary"];
  return (
    <button
      disabled={disabled}
      onClick={(e) => {
        e.preventDefault();
        onClick();
      }}
      className={`flex inline-flex items-center gap-1 border rounded-lg border disabled:cursor-not-allowed px-2 py-1 transition-colors duration-300 ${variantClassNames} ${className}`}
    >
      {children}
    </button>
  );
};
