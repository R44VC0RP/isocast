# Scene language (proposal)

Status: planned as phase 4 of `PLAN.md` (2026-10-11), from Ryan's idea of
building scenes with code "almost like React", with custom animations
between scenes, CSS-like styling, images, text and fonts, plus live data
from external APIs (like Twitch chat) and plugins that add node types.
Nothing here is built yet.

## The idea in one paragraph

A scene is a tree of nodes (video, image, text, groups), laid out and styled
with CSS vocabulary, and animated on the recording clock. People and agents
write it in TSX, which compiles to the same plain setup data that `plan` and
`apply` already diff and patch by id. The engine's own GPU compositor draws
it; a real browser is only one kind of source, for arbitrary HTML.

## What it looks like

```tsx
// scenes.tsx
const Facecam = (p) => (
  <Video id="facecam" source="facecam" crop={{ left: 120 }}
         style={{ borderRadius: 24, boxShadow: "0 8px 32px #0008", ...p.style }} />
);

export const desktop = (
  <Scene id="desktop" style={{ background: "#111" }}>
    <Video id="screen" source="screen" style={{ inset: 0 }} />
    <Facecam style={{ position: "absolute", right: 48, bottom: 48, width: 640 }} />
    <Text id="title" style={{ font: "600 48px Inter", color: "white", left: 64, bottom: 64,
                              animation: "slide-in 400ms ease-out" }}>
      Building Isocast
    </Text>
  </Scene>
);

export const closeup = (
  <Scene id="closeup"><Facecam style={{ inset: 0, borderRadius: 0 }} /></Scene>
);

export const transitions = {
  "desktop -> closeup": { match: "id", duration: 600, easing: "cubic-bezier(.2,.8,.2,1)" },
};
```

The compiled form is ordinary setup data, so agents can read and patch it
without running any code:

```toml
[[scenes]]
id = "desktop"
style = { background = "#111" }

[[scenes.nodes]]
id = "facecam"
kind = "video"
source = "facecam"
crop = { left = 120 }
style = { position = "absolute", right = 48, bottom = 48, width = 640, border_radius = 24 }
```

## Principles

- **Data, not code, at run time.** TSX runs when a setup is loaded or
  applied, the way React builds a DOM; nothing executes JavaScript per frame
  during a recording. Hand-written TOML/JSON stays first-class, and the one
  method table (GUI, CLI, MCP) is unchanged.
- **Ids everywhere.** Nodes have stable ids, so patches address them and
  transitions can match them.
- **Frame-exact animation.** Keyframes and transitions are evaluated at
  each canvas tick of the session clock, so the program file is
  deterministic. Because every source is also an ISO file, the program
  could be re-rendered later from the ISOs, the scene file and the
  manifest's scene timeline, even with a different layout.
- **Matched transitions.** Switching scenes animates nodes present in both
  (matched by id) from their old to their new box and style, like Keynote's
  Magic Move: the facecam grows from the corner to full screen. Cuts, fades
  and custom keyframe or shader transitions cover the rest.
- **Native rendering.** The wgpu compositor draws everything at 4K60 with
  predictable cost; a browser engine would be heavy, hard to keep on the
  clock, and not frame-exact.

## Building blocks

| Need | Approach | Licence |
|---|---|---|
| Layout (flexbox, grid, `inset`, %) | `taffy` | MIT |
| Text and fonts | `cosmic-text` (shaping), `glyphon` (wgpu text) | MIT/Apache |
| Rounded corners, borders, shadows, opacity, transforms, masks | our shader (signed-distance rounded rects) | ours |
| Images (PNG, SVG) | an `image` source; `resvg` for SVG | MIT/Apache |
| Animations | `@keyframes`-style tracks and `transition` with CSS easing, evaluated on ticks | ours |
| TSX authoring | an embedded JS runtime (`rquickjs`) or a build step, emitting setup JSON | MIT |
| Arbitrary HTML | a `web` source: an offscreen WKWebView rendering into an IOSurface, composited like a camera | Apple API |

All fit Isocast's MIT OR Apache-2.0 licence and the clean-room rule.

## Live data and plugins

Overlays like a Twitch chat need data that changes while recording:

- The engine keeps a data store (keys to JSON values) that the CLI, MCP,
  plugins and scene code can set; changes are events.
- Components that read a key re-render when it changes, and the result is
  merged into the live scene tree by id, as React does with the DOM, so new
  chat lines get enter animations and nothing re-runs per frame.
- Scene code may fetch and open WebSockets for the hosts its setup allows.
  Reading Twitch chat needs no credentials (an anonymous read-only chat
  connection), so it is the worked example.
- Plugins can declare node types: a props schema plus how to draw them (a
  WGSL shader, a TSX component, a web page, or an external process that
  renders frames). They appear in `isocast setup schema`, so agents can
  discover and use them.

## Order

Phase 4 of the plan, after phase 3 (program recording, mixer, switching,
soak test), since the compositor that draws scenes on the canvas clock
comes from phase 3:

1. Scene trees and styles (rounded corners, borders, shadows, opacity,
   transforms).
2. Layout (`taffy`).
3. Text and fonts.
4. Images.
5. Keyframes.
6. Scene transitions, including id-matched morphs and WGSL shaders.
7. Shader effects on nodes.
8. TSX authoring.
9. Live data.
10. External APIs (Twitch chat example).
11. The `web` source.
12. Plugin nodes.

Each step keeps setups valid TOML/JSON and adds to `isocast setup schema`,
so agents learn the new nodes from the schema.
