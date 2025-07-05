import { create } from "zustand";

import { SessionStats } from "../api-types";

export interface StatsStore {
  stats: SessionStats;
  setStats: (stats: SessionStats) => void;
}

export const useStatsStore = create<StatsStore>((set) => ({
  stats: {
    counters: {
      fetched_bytes: 0,
      uploaded_bytes: 0,
      blocked_incoming: 0,
      blocked_outgoing: 0,
    },
    peers: {
      connecting: 0,
      dead: 0,
      live: 0,
      not_needed: 0,
      queued: 0,
      seen: 0,
    },
    download_speed: { human_readable: "N/A", mbps: 0 },
    upload_speed: { human_readable: "N/A", mbps: 0 },
    uptime_seconds: 0,
    connections: {
      tcp: {
        v4: {
          attempts: 0,
          successes: 0,
          errors: 0,
        },
        v6: {
          attempts: 0,
          successes: 0,
          errors: 0,
        },
      },
      utp: {
        v4: {
          attempts: 0,
          successes: 0,
          errors: 0,
        },
        v6: {
          attempts: 0,
          successes: 0,
          errors: 0,
        },
      },
      socks: {
        v4: {
          attempts: 0,
          successes: 0,
          errors: 0,
        },
        v6: {
          attempts: 0,
          successes: 0,
          errors: 0,
        },
      },
    },
  },
  setStats: (stats) => {
    set({ stats });
  },
}));
