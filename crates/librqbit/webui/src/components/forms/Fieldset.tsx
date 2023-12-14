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
        <label className="text-md font-md mb-3 block pb-1 border-b">
          {label}
        </label>
      )}
      {children}
    </fieldset>
  );
};
