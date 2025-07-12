import { create } from "zustand";

export interface ViewModeStore {
  compact: boolean;
  toggleCompact: () => void;
}

export const useViewModeStore = create<ViewModeStore>((set) => ({
  compact: false,
  toggleCompact: () => set((state) => ({ compact: !state.compact })),
}));
