import { ReactNode } from "react";

export const Fieldset = ({
  children,
  label,
  help,
  className,
}: {
  children: ReactNode;
  label?: string;
  help?: string;
  className?: string;
}) => {
  return (
    <fieldset className={`mb-4 ${className}`}>
      {label && (
        <label className="font-medium mb-3 block pb-1 border-b border-divider">
          {label}
        </label>
      )}
      {children}
    </fieldset>
  );
};
