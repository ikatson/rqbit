# Virtualization in rqbit WebUI

This document explains how list virtualization works in the rqbit web interface.

## Performance Results

| Metric | Before | After | Improvement |
|--------|--------|-------|-------------|
| LCP (Largest Contentful Paint) | 1,175 ms | 141 ms | **8.3x faster** |
| DOM elements | 44,228 | ~500 | **99% reduction** |
| Render delay | 1,168 ms | 138 ms | **8.5x faster** |

Tested with 1000 mock torrents.

## The Problem

With 1000 torrents, each card has ~44 DOM elements = 44,000 DOM nodes. The browser must:
1. Create all 44K nodes on initial render
2. Calculate styles for all 44K nodes
3. Layout all 44K nodes
4. Keep all 44K nodes in memory

This causes:
- Slow initial page load (~1.2 seconds)
- Sluggish scrolling
- High memory usage
- Slow filter/search updates (all nodes re-rendered)

## The Solution - react-virtuoso

Virtualization renders **only the visible items**. If your viewport shows 6 cards, only those 6 cards exist in the DOM.

```
┌─────────────────────────────────┐
│ Card 0  ← rendered              │  ← viewport top
│ Card 1  ← rendered              │
│ Card 2  ← rendered              │
│ Card 3  ← rendered              │
│ Card 4  ← rendered              │
│ Card 5  ← rendered              │  ← viewport bottom
├─────────────────────────────────┤
│ Card 6  ← NOT in DOM            │
│ Card 7  ← NOT in DOM            │
│ ...                             │
│ Card 999 ← NOT in DOM           │
└─────────────────────────────────┘
```

As you scroll, react-virtuoso:
1. Removes items that scroll out of view (unmounts React components)
2. Creates items that scroll into view (mounts new components)
3. **Measures item heights dynamically** - no fixed height required!

## Implementation

### Key Dependencies

```json
{
  "react-virtuoso": "^4.12.5"
}
```

### Why react-virtuoso over react-window?

| Feature | react-window | react-virtuoso |
|---------|--------------|----------------|
| Variable height items | Requires `VariableSizeList` + height function | Automatic measurement |
| Container sizing | Requires `AutoSizer` wrapper | Automatic |
| API complexity | `itemSize`, `itemData`, `style` prop | Just `totalCount` + `itemContent` |
| Mobile experience | Poor (fixed heights don't adapt) | Great (measures actual content) |

### Card View Implementation

```typescript
import { Virtuoso } from "react-virtuoso";

// Item renderer - receives index, returns JSX
const itemContent = useCallback(
  (index: number) => {
    const torrent = filteredTorrents![index];
    return (
      <div className="pb-2">
        <TorrentCard torrent={torrent} />
      </div>
    );
  },
  [filteredTorrents]
);

return (
  <div className="flex-1 min-h-0">
    <Virtuoso
      totalCount={filteredTorrents.length}
      itemContent={itemContent}
    />
  </div>
);
```

That's it! No `AutoSizer`, no `itemSize`, no `style` prop to pass through.

### Table View Implementation

Table view uses the same pattern. Each row wraps a `<table>` to maintain column alignment with the header.

```typescript
// TorrentTable.tsx
const itemContent = useCallback(
  (index: number) => {
    const torrent = filteredTorrents![index];
    return (
      <TorrentTableRow
        torrent={torrent}
        isSelected={selectedTorrentIds.has(torrent.id)}
        onRowClick={handleRowClick}
        onCheckboxChange={toggleSelection}
      />
    );
  },
  [filteredTorrents, selectedTorrentIds, handleRowClick, toggleSelection]
);

return (
  <div className="flex flex-col h-full">
    {/* Fixed header */}
    <table className="w-full table-fixed">
      <thead>...</thead>
    </table>
    {/* Virtualized body */}
    <div className="flex-1 min-h-0">
      <Virtuoso
        totalCount={filteredTorrents?.length ?? 0}
        itemContent={itemContent}
      />
    </div>
  </div>
);
```

```typescript
// TorrentTableRow.tsx - No style prop needed!
return (
  <table className="w-full table-fixed">
    <tbody>
      <tr className="h-[40px]">
        <td className="w-8 align-middle">...</td>
        <td className="w-12 align-middle">...</td>
        {/* ... more cells with explicit widths */}
      </tr>
    </tbody>
  </table>
);
```

### Parent Height Chain

react-virtuoso still needs a parent with explicit height. This is the most common gotcha.

**Required CSS chain:**
```
html, body: height: 100%
  └─ App container: h-screen flex flex-col
       └─ Content area: flex-1 min-h-0 (or grow min-h-0)
            └─ Virtuoso container: flex-1 min-h-0
                 └─ Virtuoso (fills container automatically)
```

Key classes:
- `h-screen` or `h-full` - explicit height
- `flex-1` - grow to fill available space
- `min-h-0` - allow shrinking below content size (critical for flex children!)

Without `min-h-0`, flex children have implicit `min-height: auto` which prevents shrinking.

## Hacks and Fixes

### 1. Hidden Scrollbar

**Problem**: Virtuoso creates a scrollable container with visible scrollbar, which looked bad in card view.

**Solution**: CSS to hide scrollbar while preserving scroll functionality.

```css
/* globals.css */
.hide-scrollbar {
  scrollbar-width: none; /* Firefox */
  -ms-overflow-style: none; /* IE/Edge */
}
.hide-scrollbar::-webkit-scrollbar {
  display: none; /* Chrome/Safari/Opera */
}
```

Apply to the Virtuoso: `className="hide-scrollbar"`

### 2. Table Row Height Consistency

**Problem**: Variable padding on table cells can cause inconsistent row heights.

**Solution**: Use explicit row height + vertical alignment.

```typescript
<tr className="h-[40px]">
  <td className="align-middle">...</td>
</tr>
```

## Trade-offs and Downsides

### 1. Parent Height Required
If any parent in the chain lacks explicit height, Virtuoso may not render correctly.

**Debugging:** Check if the container has height using browser dev tools.

### 2. Array Filtering Required
Unlike CSS `hidden` approach, you MUST filter the array before passing to Virtuoso.

```typescript
// With virtualization - filter first
const filtered = items.filter(item => matchesSearch(item));
<Virtuoso
  totalCount={filtered.length}
  itemContent={(index) => <Item data={filtered[index]} />}
/>

// Without virtualization - CSS hidden (doesn't work with virtualization!)
items.map(item => (
  <Item hidden={!matchesSearch(item)} data={item} />
))
```

### 3. Component State Resets on Scroll
When an item scrolls out of view, its React component unmounts. Local state is lost.

**Workarounds:**
- Lift state up to parent or global store
- Accept that expanded/selected state resets on scroll

### 4. Keyboard Navigation
Focus management needs extra work since items mount/unmount. We track focused index via store state, not DOM focus.

## Files Modified

- `src/components/CardLayout.tsx` - Card view virtualization
- `src/components/compact/TorrentTable.tsx` - Table view virtualization
- `src/components/compact/TorrentTableRow.tsx` - Row component (no style prop needed)
- `src/rqbit-web.tsx` - `h-screen` for proper height chain
- `src/components/RootContent.tsx` - Height classes for card view
- `src/globals.css` - `.hide-scrollbar` utility
- `package.json` - react-virtuoso dependency

## Testing

Use mock mode to test with 1000 torrents:
```bash
npm run dev:mock
```

Open Chrome DevTools Performance tab to measure:
- LCP (Largest Contentful Paint)
- DOM node count
- Layout/style recalculation times
