# Common Types (`src/common/`) — Reference

Developer reference for `src/common/`. All types are OS-independent and have zero platform dependencies.

---

## Files

| File | Lines | Purpose |
|------|-------|---------|
| `types.rs` | 260 | Rect, Size, Point, Direction, WindowId |
| `error.rs` | 52 | StmError enum, StmResult alias |
| `mod.rs` | — | Re-exports |

Total: **312 lines**

---

## Geometry Types

### `Rect`

Axis-parallel rectangle with integer pixel coordinates. This is the **frozen cross-layer contract** — its shape (`x: i32, y: i32, width: i32, height: i32`) must not change.

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}
```

Methods:

```rust
impl Rect {
    /// Zero area (width <= 0 or height <= 0).
    pub fn is_empty(self) -> bool;

    /// x + width
    pub fn right(self) -> i32;

    /// y + height
    pub fn bottom(self) -> i32;

    /// AABB overlap test (touching edges = false).
    pub fn overlaps(self, other: Rect) -> bool;
}
```

**`overlaps` semantics**: touching rectangles are NOT overlapping (consistent with Win32 `IntersectRect` behavior):

```rust
let a = Rect { x: 0, y: 0, width: 100, height: 100 };
let b = Rect { x: 100, y: 0, width: 100, height: 100 };
assert!(!a.overlaps(b)); // touching at x=100 → no overlap
```

### `Size`

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Size {
    pub w: i32,
    pub h: i32,
}
```

### `Point`

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}
```

---

## Direction

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}
```

Used by focus, swap, and resize mutations. `Left`/`Right` operate on columns (horizontal container). `Up`/`Down` operate on rows within a column (vertical container).

---

## WindowId

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WindowId(pub isize);
```

Platform-independent opaque window handle. On Windows, stores the HWND value. The actual Win32 conversion happens only in the `registry` and `win32` modules.

Usage as HashMap/HashSet key:

```rust
use std::collections::HashSet;
let mut set = HashSet::new();
set.insert(WindowId(1));
assert!(set.contains(&WindowId(1)));
```

---

## Error Types

### `StmError`

```rust
#[derive(Debug)]
pub enum StmError {
    Config(String),           // YAML parse/validation failure
    Layout(String),           // Invalid layout state
    Io(std::io::Error),       // File/socket I/O
}

impl std::fmt::Display for StmError { /* "config error: ...", "layout error: ...", "I/O error: ..." */ }
impl std::error::Error for StmError {}
impl From<std::io::Error> for StmError { /* auto-wrap Io variant */ }
```

### `StmResult`

```rust
pub type StmResult<T> = Result<T, StmError>;
```

Used across all subsystems as the standard result type.

---

## Test Patterns

Tests for common types follow a consistent pattern:

```rust
#[test]
fn rect_overlaps_partial() {
    let a = Rect { x: 0, y: 0, width: 100, height: 100 };
    let b = Rect { x: 50, y: 50, width: 100, height: 100 };
    assert!(a.overlaps(b));
    assert!(b.overlaps(a));  // symmetry check
}

#[test]
fn display_config_error() {
    let err = StmError::Config("bad yaml".into());
    assert_eq!(format!("{err}"), "config error: bad yaml");
}
```

Negative tests use `assert!(!...)` or `assert!(result.is_none())` with comments marking the category:
- `// Positive: ...` — expected success case
- `// Negative: ...` — expected failure/boundary case
