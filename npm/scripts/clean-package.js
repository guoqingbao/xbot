#!/usr/bin/env node
"use strict";

const fs = require("fs");
const path = require("path");

const packageRoot = path.resolve(__dirname, "..");

for (const entry of ["README.md", "LICENSE.txt", "docs", "skills"]) {
  fs.rmSync(path.join(packageRoot, entry), { recursive: true, force: true });
}
