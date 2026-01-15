# WebUI Architecture Guide

This document helps Claude work efficiently on the rqbit webui codebase.

## Tech Stack
- React 18 + TypeScript
- Tailwind CSS (dark mode via `dark:` prefix)
- Zustand for state management
- Vite for dev/build
- react-icons for icons (BsX, FaX, MdX, GoX prefixes)

## Directory Structure
```
src/
├── api-types.ts        # TypeScript types matching backend API
├── http-api.ts         # API client (all backend calls)
├── context.tsx         # React contexts (APIContext)
├── rqbit-web.tsx       # App shell, header, menu buttons
├── main.tsx            # Entry point
├── stores/             # Zustand stores
│   ├── torrentStore.ts # Global torrent list, loading states
│   ├── uiStore.ts      # View mode, selection state
│   ├── errorStore.ts   # Alerts and errors
│   └── statsStore.ts   # Session-wide stats
├── hooks/              # Custom React hooks
├── helper/             # Utility functions (formatBytes, etc.)
└── components/
    ├── RootContent.tsx         # Main content area, layout switching
    ├── CardLayout.tsx          # Card view layout (list of cards)
    ├── TorrentCard.tsx         # Card view data wrapper per torrent
    ├── TorrentCardContent.tsx  # Card view single torrent content
    ├── compact/                # Compact/table view components
    │   ├── CompactLayout.tsx    # Table view layout
    │   ├── TorrentTable.tsx     # Table with headers
    │   ├── TorrentTableRow.tsx  # Single table row
    │   ├── ActionBar.tsx        # Bulk action buttons
    │   ├── DetailPane.tsx       # Bottom detail panel
    │   └── *Tab.tsx             # Detail tabs
    ├── buttons/           # Reusable buttons
    ├── modal/             # Modal dialogs
    └── forms/             # Form components
```

## Key Patterns

### Data Fetching (per-torrent)
Each torrent fetches its own data independently. Pattern from `TorrentCard.tsx`:
```typescript
// Details: fetch once, retry on error
useEffect(() => {
  return loopUntilSuccess(async () => {
    await API.getTorrentDetails(id).then(setDetails);
  }, 1000);
}, [forceRefresh]);

// Stats: continuous polling with adaptive interval
useEffect(() => {
  return customSetInterval(async () => {
    return API.getTorrentStats(id).then(stats => {
      setStats(stats);
      // Fast poll (1s) when live, slow (10s) when paused
      return stats.state === "live" ? 1000 : 10000;
    });
  }, 0);
}, [forceRefresh]);
```

### State Management
Zustand stores are simple - just use hooks:
```typescript
// Reading state
const viewMode = useUIStore(state => state.viewMode);
const torrents = useTorrentStore(state => state.torrents);

// Actions
const selectTorrent = useUIStore(state => state.selectTorrent);
selectTorrent(id);
```

### Responsive Design
- Breakpoint: `lg` (1024px) for compact vs card view
- Use `useIsLargeScreen()` hook for JS logic
- Use Tailwind classes for CSS: `lg:flex-row`, `hidden lg:block`

### Dark Mode
Always add dark variants: `bg-white dark:bg-slate-800`

## API Types (api-types.ts)
Key types to know:
- `TorrentId`: `{ id: number, info_hash: string }`
- `TorrentDetails`: `{ name, info_hash, files[] }`
- `TorrentStats`: `{ state, error, progress_bytes, total_bytes, finished, live? }`
- `LiveTorrentStats`: speeds, ETA, peer_stats (only when state="live")

States: `"initializing"`, `"live"`, `"paused"`, `"error"`

## API Methods (http-api.ts)
```typescript
API.listTorrents()           // GET /torrents
API.getTorrentDetails(id)    // GET /torrents/{id}
API.getTorrentStats(id)      // GET /torrents/{id}/stats/v1
API.start(id)                // POST /torrents/{id}/start
API.pause(id)                // POST /torrents/{id}/pause
API.forget(id)               // POST /torrents/{id}/forget (remove from list)
API.delete(id)               // POST /torrents/{id}/delete (remove + delete files)
API.updateOnlyFiles(id, fileIds[])  // POST /torrents/{id}/update_only_files
```

## Adding New Features

### New Component
1. Create in appropriate directory (`components/` or `components/compact/`)
2. Use existing patterns for data fetching if needed
3. Import from parent component

### New Store State
1. Add to existing store or create new in `stores/`
2. Follow Zustand pattern: `create<StoreType>((set, get) => ({...}))`
3. For persistence, use `localStorage` in actions

### New API Call
1. Add type to `api-types.ts`
2. Add method to `http-api.ts` using `makeRequest()`

## Common Tasks

### Show loading state
```typescript
if (!data) return <Spinner />;
```

### Show error
```typescript
const setCloseableError = useErrorStore(state => state.setCloseableError);
setCloseableError({ text: "Error message", details: error });
```

### Refresh torrent data
```typescript
const refreshTorrents = useTorrentStore(state => state.refreshTorrents);
refreshTorrents();
```

### Format display values
```typescript
import { formatBytes } from "../helper/formatBytes";
import { getCompletionETA } from "../helper/getCompletionETA";
import { torrentDisplayName } from "../helper/getTorrentDisplayName";
```

## Testing Changes
```bash
# Dev server (hot reload)
npm run dev  # or: make webui-dev from repo root

# Type check
npx tsc --noEmit

# Build
npm run build
```

Dev server runs at http://localhost:3031/, connects to backend at :3030.

## Mock Mode (No Backend Required)

For UI testing without a real rqbit server, use mock mode:

```bash
npm run dev:mock
```

This starts the dev server on port 3032 and opens http://localhost:3032/mock.html with:
- 1000 generated torrents (Linux distro names)
- ~30 active (live/initializing), rest paused
- Simulated download progress for live torrents
- Stable peer IPs with incrementing counters (for speed calculations)
- Working pause/start/forget/delete actions

Mock code (`mock-api.ts`, `main-mock.tsx`, `mock.html`) is excluded from production builds.

Use this to test UI performance, layout with many torrents, or develop without running the full stack.

## Performance Guidelines

When working with large lists (1000+ torrents), follow these patterns to avoid re-render issues:

### Visibility-Based Filtering (Not Array Filtering)
**DON'T** filter arrays to hide items - this causes massive re-renders when filter changes:
```typescript
// BAD: Filtered items unmount, then remount when filter clears
const filtered = torrents.filter(t => matchesSearch(t.name, query));
return filtered.map(t => <TorrentCard key={t.id} torrent={t} />);
```

**DO** keep all items in DOM and use CSS visibility:
```typescript
// GOOD: Items stay mounted, just hidden/shown via CSS
return torrents.map(t => (
  <TorrentCard
    key={t.id}
    torrent={t}
    hidden={!matchesSearch(t.name, query)}
  />
));

// In TorrentCard: use CSS hidden class
<div className={hidden ? "hidden" : ""}>{content}</div>
```

### Memoization
- Use `memo()` for row/card components that receive torrent data
- Use `useMemo()` for expensive computations (sorting, filtering for navigation)
- Use `useCallback()` for handlers passed to child components

### Debouncing
- Always debounce search input (150ms is good)
- Use local state for instant feedback, debounced update to store:
```typescript
const [localSearch, setLocalSearch] = useState(searchQuery);
const debouncedSetSearch = useCallback(
  debounce((value: string) => setSearchQuery(value), 150),
  [setSearchQuery]
);
```

### Shared Utilities
Common filtering/sorting logic is in `helper/torrentFilters.ts`:
- `isTorrentVisible(t, query, statusFilter)` - combined visibility check
- `compareTorrents(a, b, column, direction)` - sorting comparison
- Type definitions: `TorrentSortColumn`, `SortDirection`, `StatusFilter`
