import { ReactNode } from "react";

export const ModalFooter = ({
  children,
  className,
}: {
  children: ReactNode;
  className?: string;
}) => {
  return (
    <div className={`p-3 flex justify-end gap-2 ${className}`}>{children}</div>
  );
};
