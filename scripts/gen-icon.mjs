import { readFileSync, writeFileSync } from "node:fs";
import { Resvg } from "@resvg/resvg-js";

const svg = readFileSync(new URL("../src/assets/logo.svg", import.meta.url));
const render = (size) => new Resvg(svg, { fitTo: { mode: "width", value: size } }).render().asPng();

writeFileSync(new URL("../app-icon.png", import.meta.url), render(1024));
writeFileSync(new URL("../src-tauri/icons/tray.png", import.meta.url), render(64));
console.log("icons written");
