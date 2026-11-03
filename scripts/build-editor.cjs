"use strict";
const esbuild = require("esbuild");
const fs = require("node:fs");
const path = require("node:path");
const out = path.join(__dirname, "..", "web", "vendor", "sql-editor.js");
fs.mkdirSync(path.dirname(out), {recursive:true});
esbuild.buildSync({
  entryPoints:[path.join(__dirname, "..", "web", "editor-entry.js")],
  outfile:out, bundle:true, format:"iife", platform:"browser",
  target:["es2020"], minify:true, legalComments:"eof",
  logLevel:"warning"
});
console.log("Offline CodeMirror 6 SQL editor bundled:", fs.statSync(out).size, "bytes");
