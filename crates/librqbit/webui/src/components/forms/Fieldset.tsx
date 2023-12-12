import { ReactNode } from "react";

export const Fieldset = ({
  children,
  label,
  help,
  className,
}: {
  children: ReactNode;
  label: string;
  help?: string;
  className?: string;
}) => {
  return (
    <fieldset className={`mb-4 ${className}`}>
      <label className="text-md font-md mb-2 block">{label}</label>
      {children}
    </fieldset>
  );
};
