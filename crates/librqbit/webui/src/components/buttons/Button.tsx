import { ReactNode } from "react";

export const Button: React.FC<{
  onClick: React.MouseEventHandler<HTMLButtonElement>;
  variant?: "cancel" | "primary" | "secondary" | "danger" | "none";
  className?: string;
  disabled?: boolean;
  children: ReactNode;
}> = ({ onClick, children, className, disabled, variant }) => {
  let variantClassNames = {
    secondary:
      "hover:bg-blue-600 transition-colors duration-100 hover:text-white",
    danger:
      "bg-red-500 text-white border-green-50 hover:border-red-700 hover:bg-red-600",
    primary: "bg-blue-400 text-white hover:bg-blue-600",
    cancel: "bg-slate-50 hover:bg-slate-200",
    none: "",
  }[variant ?? "secondary"];
  return (
    <button
      disabled={disabled}
      onClick={onClick}
      className={`flex inline-flex items-center gap-1 border rounded-lg border mx-1 disabled:cursor-not-allowed px-2 py-1 transition-colors duration-300 ${variantClassNames} ${className}`}
    >
      {children}
    </button>
  );
};
