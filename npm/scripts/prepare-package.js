#!/usr/bin/env node
"use strict";

const fs = require("fs");
const path = require("path");

const packageRoot = path.resolve(__dirname, "..");
const repoRoot = path.resolve(packageRoot, "..");

const entries = [
  ["README.md", "README.md"],
  ["LICENSE.txt", "LICENSE.txt"],
  ["docs", "docs"],
  ["skills", "skills"]
];

function copyEntry(source, destination) {
  fs.rmSync(destination, { recursive: true, force: true });
  fs.cpSync(source, destination, {
    recursive: true,
    dereference: true,
    filter: (file) => !file.includes(`${path.sep}.git${path.sep}`)
  });
}

for (const [from, to] of entries) {
  copyEntry(path.join(repoRoot, from), path.join(packageRoot, to));
}
