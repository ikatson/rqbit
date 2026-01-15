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

## The Solution - react-window

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

As you scroll, react-window:
1. Removes items that scroll out of view (unmounts React components)
2. Creates items that scroll into view (mounts new components)
3. Positions them using absolute positioning + CSS transforms

## Implementation

### Key Dependencies

```json
{
  "react-window": "^1.8.10",
  "react-virtualized-auto-sizer": "^1.0.24"
}
```

**Warning**: react-window v2.x has a different API (no `FixedSizeList` export). Stick with v1.x.

### Key Components

1. **FixedSizeList** - The virtualized list container
   - Needs explicit height and width
   - Needs to know item height (`itemSize` prop)
   - Renders only visible items + overscan buffer

2. **AutoSizer** - Measures parent container dimensions
   - Wraps the List component
   - Provides `height` and `width` to its children
   - Requires parent to have explicit dimensions

3. **Row renderer** - Function component that renders a single item
   - Receives `index` and `style` props
   - Must apply `style` to the outer element (for positioning)

### Card View Implementation

```typescript
import { FixedSizeList as List } from "react-window";
import type { ListChildComponentProps } from "react-window";
import AutoSizer from "react-virtualized-auto-sizer";

const CARD_HEIGHT = 120; // Must match actual card height + gap

const Row = useCallback(
  ({ index, style }: ListChildComponentProps) => {
    const torrent = filteredTorrents![index];
    return (
      <div style={style} className="pb-2">
        <TorrentCard torrent={torrent} />
      </div>
    );
  },
  [filteredTorrents]
);

return (
  <div className="flex-1 min-h-0 hide-scrollbar">
    <AutoSizer>
      {({ height, width }) => (
        <List
          height={height}
          width={width}
          itemCount={filteredTorrents.length}
          itemSize={CARD_HEIGHT}
          className="hide-scrollbar"
        >
          {Row}
        </List>
      )}
    </AutoSizer>
  </div>
);
```

### Table View Implementation

Table view is more complex because each row needs to match header column widths.

**Key difference**: Each row wraps a `<table>` inside the positioned `<div>` to maintain column alignment with the header.

```typescript
// TorrentTableRow.tsx - Row receives style prop for positioning
return (
  <div style={style}>
    <table className="w-full table-fixed h-full">
      <tbody>
        <tr className="h-[40px]">  {/* Explicit height! */}
          <td className="w-8 align-middle">...</td>
          <td className="w-12 align-middle">...</td>
          {/* ... more cells with explicit widths */}
        </tr>
      </tbody>
    </table>
  </div>
);
```

**Why nested tables?** react-window positions items using absolute positioning on a wrapper div. We can't make the `<tr>` the positioned element directly, so we wrap each row in `<div style={style}><table>...</table></div>`.

### Parent Height Chain

AutoSizer needs explicit height from its parent chain. This is the most common gotcha.

**Required CSS chain:**
```
html, body: height: 100%
  └─ App container: h-screen flex flex-col
       └─ Content area: flex-1 min-h-0 (or grow min-h-0)
            └─ AutoSizer container: h-full or flex-1 min-h-0
                 └─ AutoSizer (measures and provides height/width)
                      └─ List (uses the measured dimensions)
```

Key classes:
- `h-screen` or `h-full` - explicit height
- `flex-1` - grow to fill available space
- `min-h-0` - allow shrinking below content size (critical for flex children!)

Without `min-h-0`, flex children have implicit `min-height: auto` which prevents shrinking.

## Hacks and Fixes

### 1. Row Flickering on Selection (Table View)

**Problem**: When the Row callback depends on changing state (like `selectedTorrentIds`), it gets recreated on every state change. react-window sees a new function and re-renders ALL visible rows.

**Solution**: Use the `itemData` prop to pass changing data. Keep the Row callback stable with empty dependencies.

```typescript
// Data passed via itemData - changes don't recreate Row function
const itemData = useMemo(
  () => ({
    torrents: filteredTorrents,
    selectedTorrentIds,
    onRowClick: handleRowClick,
    onCheckboxChange: toggleSelection,
  }),
  [filteredTorrents, selectedTorrentIds, handleRowClick, toggleSelection]
);

// Row renderer - STABLE function, all data via itemData
const Row = useCallback(
  ({ index, style, data }: ListChildComponentProps<typeof itemData>) => {
    const torrent = data.torrents![index];
    return (
      <TorrentTableRow
        torrent={torrent}
        isSelected={data.selectedTorrentIds.has(torrent.id)}
        style={style}
        onRowClick={data.onRowClick}
        onCheckboxChange={data.onCheckboxChange}
      />
    );
  },
  [] // Empty deps! Data comes via itemData prop
);

// Pass itemData to List
<List itemData={itemData}>{Row}</List>
```

### 2. Table Row Height Glitch

**Problem**: Using `py-2` padding on table cells caused inconsistent row heights and visual gaps between rows.

**Solution**: Use explicit row height + vertical alignment instead of padding.

```typescript
// Before - padding causes height inconsistency
<tr><td className="py-2">...</td></tr>

// After - explicit height + align-middle
<tr className="h-[40px]">
  <td className="align-middle">...</td>
</tr>
```

The `ROW_HEIGHT` constant (41px) = 40px row + 1px border.

### 3. Hidden Scrollbar

**Problem**: react-window creates a scrollable container with visible scrollbar, which looked bad in card view.

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

Apply to both the container and the List: `className="hide-scrollbar"`

### 4. Card Height Mismatch

**Problem**: `CARD_HEIGHT` was set to 156px but actual cards were ~110px, causing huge gaps.

**Solution**: Measure actual card height and set `CARD_HEIGHT` to match (including gap).

```typescript
const CARD_HEIGHT = 120; // ~110px card + 8px pb-2 gap + buffer
```

If cards have variable heights (e.g., error messages), you need to either:
- Use the maximum possible height
- Use `VariableSizeList` with a height estimator function
- Accept that some content may be clipped

## Trade-offs and Downsides

### 1. Fixed Height Items
All items must be the same height. The list pre-calculates positions based on `itemSize`.

**Workarounds:**
- `VariableSizeList` - allows different heights but you must provide a function
- Collapse expanded state when item scrolls out of view

### 2. Parent Height Required
If any parent in the chain lacks explicit height, AutoSizer reports 0 and nothing renders.

**Debugging:**
```javascript
<AutoSizer>
  {({ height, width }) => {
    console.log('AutoSizer dimensions:', { height, width });
    // If both are 0, check parent heights
  }}
</AutoSizer>
```

### 3. Array Filtering Required
Unlike CSS `hidden` approach, you MUST filter the array before passing to the virtualized list.

```typescript
// With virtualization - filter first
const filtered = items.filter(item => matchesSearch(item));
<List itemCount={filtered.length}>
  {({ index }) => <Item data={filtered[index]} />}
</List>

// Without virtualization - CSS hidden (doesn't work with virtualization!)
items.map(item => (
  <Item hidden={!matchesSearch(item)} data={item} />
))
```

### 4. Component State Resets on Scroll
When an item scrolls out of view, its React component unmounts. Local state is lost.

**Workarounds:**
- Lift state up to parent or global store
- Accept that expanded/selected state resets on scroll

### 5. Keyboard Navigation
Focus management needs extra work since items mount/unmount. We track focused index via store state, not DOM focus.

## Files Modified

- `src/components/CardLayout.tsx` - Card view virtualization
- `src/components/compact/TorrentTable.tsx` - Table view virtualization with itemData pattern
- `src/components/compact/TorrentTableRow.tsx` - Row with style prop, nested table structure
- `src/rqbit-web.tsx` - Changed to `h-screen` for proper height chain
- `src/components/RootContent.tsx` - Added height classes for card view
- `src/globals.css` - Added `.hide-scrollbar` utility
- `package.json` - Added react-window dependencies

## Testing

Use mock mode to test with 1000 torrents:
```bash
npm run dev:mock
```

Open Chrome DevTools Performance tab to measure:
- LCP (Largest Contentful Paint)
- DOM node count
- Layout/style recalculation times
