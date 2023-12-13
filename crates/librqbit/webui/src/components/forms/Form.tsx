import { ReactNode } from "react";

export const Form = ({ children }: { children: ReactNode }) => {
  return <form>{children}</form>;
};
