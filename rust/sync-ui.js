// Copies the UI shared with the Python build into ui/, which Tauri embeds. Runs before each build.
const fs = require("fs");
const path = require("path");

const root = path.join(__dirname, "..");
const ui = path.join(__dirname, "ui");
fs.rmSync(ui, { recursive: true, force: true });
fs.mkdirSync(ui);
fs.copyFileSync(path.join(root, "ui.html"), path.join(ui, "index.html"));
fs.copyFileSync(path.join(root, "i18n.json"), path.join(ui, "i18n.json"));
fs.cpSync(path.join(root, "fonts"), path.join(ui, "fonts"), { recursive: true });
