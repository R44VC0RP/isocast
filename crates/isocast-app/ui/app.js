// The Isocast interface: a thin client of isocastd, inside the app's
// native window (see crates/isocast-app/src/mac).

import { render } from "preact";
import { ROUTE, native } from "./lib/bridge.js";
import { html } from "./lib/html.js";
import { boot } from "./lib/session.js";
import { App } from "./ui/app.js";
import { AppIcon } from "./icon.js";

if (ROUTE === "icon") {
  document.documentElement.style.background = "transparent";
  document.body.style.background = "transparent";
  render(html`<${AppIcon} />`, document.getElementById("app"));
  requestAnimationFrame(() => native("ready"));
} else {
  boot();
  render(html`<${App} />`, document.getElementById("app"));
}
