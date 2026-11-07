# Beat Saber Light Timeline

A Blender-style dope sheet for Beat Saber light events: one row per light lane (and per
Chroma light-ID group), keyframes you click, drag and recolor, with the song's waveform
and a live color preview strip under every row.

Works on v2 and v3 difficulty files, including Chroma colors, light IDs, easings and
legacy Chroma RGB values. Everything it doesn't edit is written back exactly as it was.

## Run

```bash
cargo run --release -- "/path/to/Beat Saber_Data/CustomLevels/<map folder>"
```

The folder is the one containing `Info.dat`. The last folder is remembered.

## Editing

| Action | How |
|---|---|
| Add keyframe | Double-click a row, or `I` to insert at the playhead on the active row |
| Select | Click, Shift+click, or drag a box in empty space |
| Move | Drag keys (snaps to the grid; hold Ctrl for free movement) |
| Edit | Right panel: type, base color, Chroma color, brightness, easing, HSV blend |
| Delete | `Delete` or `X` |
| Copy / paste | `Ctrl+C`, `Ctrl+V` (pastes at the playhead) |
| Undo / redo | `Ctrl+Z`, `Ctrl+Shift+Z` |
| Play | `Space`; click the ruler to move the playhead; `←`/`→` step |
| Navigate | Wheel scrolls rows, Shift+wheel pans, Ctrl+wheel zooms, middle-drag pans |
| Save | `Ctrl+S` |

The **Preview** panel at the bottom (toggle in the toolbar, drag its edge to resize) is a 3D
view from the player's spot at the playhead:

- **Custom geometry from the map is exact**: Chroma `environment` objects (cubes, spheres,
  cylinders, planes, quads, triangles) with their materials, lit by your light events through
  their `ILightWithId` lane and light ID, and moved by `AnimateTrack` (position, rotation,
  scale, easings, repeat, point definitions).
- **The base environment is approximate**: the game's own scene isn't in the map, so a generic
  stage (back lasers, rings, side lasers, center lights) stands in. It's turned off
  automatically when the map hides base objects; toggle it with "Base stage".
- Not previewed: edits that move base-game objects, Vivify events, track parenting, bloom
  and fog are only approximated.

Drag to look around, use the wheel to move, double-click to reset the camera.

Key shapes: ◆ On, ● Transition, ▲ Flash, ▼ Fade, ■ Off. A **Transition** key blends from
the key before it using its easing, so chained transitions work like animation curves.

The first save of a session copies the original file to `<file>.timeline-backup-<time>`.
Don't edit the same difficulty in ChroMapper at the same time: whichever saves last wins.

## Checking maps

```bash
cargo run --release -- --check "/path/to/CustomLevels" --audio
```

Loads every map read-only and verifies that saving would reproduce the same light events.

```bash
cargo run --release -- --save-test "<map folder>" /tmp/light-timeline-test
```

Copies a map's `.dat` files to the work folder, edits, saves and reloads the copy.

## Headless UI test (podman)

```bash
cargo build --release && test/run-ui-test.sh "/path/to/CustomLevels" /tmp/bslt-ui
```

Runs the real app on Xvfb inside a podman container (`test/Containerfile`) with the maps
mounted read-only, drives it with xdotool (`test/ui-steps.sh`: add, recolor, slider, save,
delete, undo) on a copy of one map, and writes screenshots to the output folder.
