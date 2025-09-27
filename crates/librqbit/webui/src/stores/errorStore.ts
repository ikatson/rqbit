import { create } from "zustand";
import { ErrorDetails } from "../api-types";

export interface ErrorWithLabel {
  text: string;
  details?: ErrorDetails;
}

export const useErrorStore = create<{
  alert: ErrorWithLabel | null;
  setAlert: (alert: ErrorWithLabel) => void;
  removeAlert: () => void;

  closeableError: ErrorWithLabel | null;
  setCloseableError: (error: ErrorWithLabel | null) => void;

  otherError: ErrorWithLabel | null;
  setOtherError: (error: ErrorWithLabel | null) => void;
}>((set) => ({
  closeableError: null,
  setCloseableError: (closeableError) => set(() => ({ closeableError })),

  otherError: null,
  setOtherError: (otherError) => set(() => ({ otherError })),

  alert: null,
  setAlert: (alert) => set(() => ({ alert })),
  removeAlert: () => set(() => ({ alert: null })),
}));
