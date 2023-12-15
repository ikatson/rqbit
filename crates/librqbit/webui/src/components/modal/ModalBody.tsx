import { ReactNode } from "react";

export const ModalBody = ({ children }: { children: ReactNode }) => {
  return <div className="p-3 border-b dark:border-slate-500">{children}</div>;
};
