// Tests for the wrapper's bin shim, npm/supdoc/bin/supdoc.js.
//
// Each test lays out a project the way a package manager would (the wrapper and, when the test
// wants one, the platform package under node_modules/), copies the real shim into it and runs it
// with Node. The platform package's "binary" is Node itself, so a test chooses what the binary does
// by passing `-e <script>` through the shim.

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { copyFileSync, mkdirSync, mkdtempSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { afterEach, test } from "node:test";
import { fileURLToPath } from "node:url";

const SHIM = fileURLToPath(new URL("../supdoc/bin/supdoc.js", import.meta.url));
const PLATFORM = `${process.platform}-${process.arch}`;
const PACKAGE = `@supdoc/${PLATFORM}`;
const WINDOWS = process.platform === "win32";

const roots = [];
afterEach(() => {
  for (const root of roots.splice(0)) rmSync(root, { recursive: true, force: true });
});

/** Writes `value` as JSON at `file`, creating its directory. */
function writeJson(file, value) {
  mkdirSync(path.dirname(file), { recursive: true });
  writeFileSync(file, `${JSON.stringify(value, null, 2)}\n`);
}

/**
 * Builds a project whose wrapper lists `listed` as optional dependencies and, when `installed` is
 * true, holds this platform's package with Node standing in for the supdoc binary (left out when
 * `binary` is false). Returns the path of the installed shim.
 */
function project({ listed, installed, binary = true }) {
  const root = mkdtempSync(path.join(tmpdir(), "supdoc-shim-"));
  roots.push(root);
  const modules = path.join(root, "node_modules");
  const wrapper = path.join(modules, "supdoc");
  writeJson(path.join(wrapper, "package.json"), {
    name: "supdoc",
    version: "0.0.0-test",
    bin: { supdoc: "bin/supdoc.js" },
    optionalDependencies: Object.fromEntries(listed.map((name) => [name, "0.0.0-test"])),
  });
  const shim = path.join(wrapper, "bin", "supdoc.js");
  mkdirSync(path.dirname(shim));
  copyFileSync(SHIM, shim);

  if (installed) {
    const platformDir = path.join(modules, ...PACKAGE.split("/"));
    writeJson(path.join(platformDir, "package.json"), { name: PACKAGE, version: "0.0.0-test" });
    mkdirSync(path.join(platformDir, "bin"));
    const file = path.join(platformDir, "bin", WINDOWS ? "supdoc.exe" : "supdoc");
    // Windows needs a real .exe to spawn without a shell, and symlinks need privileges there.
    if (binary && WINDOWS) copyFileSync(process.execPath, file);
    else if (binary) symlinkSync(process.execPath, file);
  }
  return shim;
}

/** Runs the shim with `args`, isolated from any NODE_PATH the caller set. */
function run(shim, args) {
  const env = { ...process.env };
  delete env.NODE_PATH;
  return spawnSync(process.execPath, [shim, ...args], { encoding: "utf8", env });
}

test("runs the installed platform binary with the caller's arguments and exits with its status", () => {
  const shim = project({ listed: [PACKAGE], installed: true });

  const result = run(shim, [
    "-e",
    "process.stdout.write(JSON.stringify(process.argv.slice(1))); process.exit(7)",
    "check",
    "--deny-warnings",
  ]);

  assert.equal(result.stderr, "", "the shim should add nothing to stderr");
  assert.deepEqual(
    JSON.parse(result.stdout),
    ["check", "--deny-warnings"],
    "the binary should receive the arguments that followed the shim",
  );
  assert.equal(result.status, 7, "the shim should exit with the binary's exit code");
});

test("exits 0 when the binary exits 0", () => {
  const shim = project({ listed: [PACKAGE], installed: true });

  const result = run(shim, ["-e", "process.exit(0)"]);

  assert.equal(result.status, 0, `expected exit 0, stderr: ${result.stderr}`);
});

test("dies by the binary's signal", { skip: WINDOWS && "Windows has no POSIX signals" }, () => {
  const shim = project({ listed: [PACKAGE], installed: true });

  const result = run(shim, ["-e", "process.kill(process.pid, 'SIGTERM')"]);

  assert.equal(result.signal, "SIGTERM", `expected the shim to die by SIGTERM, got status ${result.status}`);
});

test("refuses a platform the wrapper does not list, exit 1", () => {
  const shim = project({ listed: ["@supdoc/none-none"], installed: false });

  const result = run(shim, ["--version"]);

  assert.equal(result.stderr, `supdoc has no prebuilt binary for ${PLATFORM}.\n`);
  assert.equal(result.stdout, "");
  assert.equal(result.status, 1);
});

test("refuses a listed platform package that is not installed, exit 1", () => {
  const shim = project({ listed: [PACKAGE], installed: false });

  const result = run(shim, ["--version"]);

  assert.equal(
    result.stderr,
    `supdoc's package for ${PLATFORM} (${PACKAGE}) is not installed; reinstall without --omit=optional or --no-optional.\n`,
  );
  assert.equal(result.stdout, "");
  assert.equal(result.status, 1);
});

test("reports a platform package whose binary is missing, exit 1", () => {
  const shim = project({ listed: [PACKAGE], installed: true, binary: false });

  const result = run(shim, ["--version"]);

  assert.match(
    result.stderr,
    /^supdoc could not run .*supdoc(\.exe)?: .*ENOENT/,
    "the shim should name the binary it could not start",
  );
  assert.equal(result.stdout, "");
  assert.equal(result.status, 1);
});
