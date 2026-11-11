#!/usr/bin/env node
// Runs the supdoc binary from the platform package npm installed beside this one.
//
// The wrapper lists every `@supdoc/<platform>-<arch>` package as an optional dependency, and the
// package manager installs only the one whose `os` and `cpu` match. This file finds that package,
// runs its binary with the same arguments and terminal, and exits the way the binary exited. It
// downloads nothing and the packages run no install script.
"use strict";

const { spawnSync } = require("node:child_process");
const path = require("node:path");

const platform = `${process.platform}-${process.arch}`;
const packageName = `@supdoc/${platform}`;

/** Prints one sentence to stderr and exits 1. */
function fail(message) {
  process.stderr.write(`${message}\n`);
  process.exit(1);
}

const listed = require("../package.json").optionalDependencies || {};
if (!Object.prototype.hasOwnProperty.call(listed, packageName)) {
  fail(`supdoc has no prebuilt binary for ${platform}.`);
}

let packageDir;
try {
  packageDir = path.dirname(require.resolve(`${packageName}/package.json`));
} catch {
  fail(
    `supdoc's package for ${platform} (${packageName}) is not installed; reinstall without --omit=optional or --no-optional.`,
  );
}

const binary = path.join(packageDir, "bin", process.platform === "win32" ? "supdoc.exe" : "supdoc");
const child = spawnSync(binary, process.argv.slice(2), { stdio: "inherit" });

if (child.error) {
  fail(`supdoc could not run ${binary}: ${child.error.message}`);
}
if (child.signal) {
  // Die by the same signal so a caller sees what the binary saw. If the signal cannot end this
  // process (Windows has no POSIX signals), fall through to a failing exit code.
  process.kill(process.pid, child.signal);
}
process.exit(child.status ?? 1);
