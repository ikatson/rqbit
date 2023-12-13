import { ReactNode } from "react";

export const ModalBody = ({ children }: { children: ReactNode }) => {
  return <div className="p-3 border-b">{children}</div>;
};
