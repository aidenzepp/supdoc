// Tests for npm/build.mjs, which packs the seven npm packages from a release's assets.
//
// Each test builds fake release assets the way release.yml lays them out: eight archives
// `supdoc-V-TARGET.tar.gz`, each holding `supdoc-V-TARGET/{supdoc or supdoc.exe, LICENSE, README.md}`,
// plus SHA256SUMS. A fake binary's bytes name its target, so a test can tell which archive a package
// was built from. Fixtures live under the repository's gitignored work/ directory, not the system
// temp directory, and each test removes its own.

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  chmodSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readdirSync,
  readFileSync,
  rmSync,
  statSync,
  symlinkSync,
  writeFileSync,
} from "node:fs";
import path from "node:path";
import { after, afterEach, before, describe, test } from "node:test";
import { fileURLToPath } from "node:url";

import { BuildError, checkManifests, makeExecutable, PLATFORMS } from "../build.mjs";

const BUILD = fileURLToPath(new URL("../build.mjs", import.meta.url));
const SCRATCH = fileURLToPath(new URL("../../work/npm-build-test/", import.meta.url));
const VERSION = "9.8.7";
const TAG = `v${VERSION}`;
const TARGETS = [
  "x86_64-unknown-linux-gnu",
  "aarch64-unknown-linux-gnu",
  "x86_64-unknown-linux-musl",
  "aarch64-unknown-linux-musl",
  "aarch64-apple-darwin",
  "x86_64-apple-darwin",
  "x86_64-pc-windows-msvc",
  "aarch64-pc-windows-msvc",
];
const EXPECTED = [
  { name: "@supdoc/darwin-arm64", os: "darwin", cpu: "arm64", target: "aarch64-apple-darwin", binary: "supdoc" },
  { name: "@supdoc/darwin-x64", os: "darwin", cpu: "x64", target: "x86_64-apple-darwin", binary: "supdoc" },
  { name: "@supdoc/linux-arm64", os: "linux", cpu: "arm64", target: "aarch64-unknown-linux-musl", binary: "supdoc" },
  { name: "@supdoc/linux-x64", os: "linux", cpu: "x64", target: "x86_64-unknown-linux-musl", binary: "supdoc" },
  { name: "@supdoc/win32-arm64", os: "win32", cpu: "arm64", target: "aarch64-pc-windows-msvc", binary: "supdoc.exe" },
  { name: "@supdoc/win32-x64", os: "win32", cpu: "x64", target: "x86_64-pc-windows-msvc", binary: "supdoc.exe" },
];

const roots = [];
afterEach(() => {
  for (const root of roots.splice(0)) rmSync(root, { recursive: true, force: true });
});

/** Makes a fresh directory under the repository's work/ scratch area. */
function scratch(prefix) {
  mkdirSync(SCRATCH, { recursive: true });
  const dir = mkdtempSync(path.join(SCRATCH, prefix));
  roots.push(dir);
  return dir;
}

/** Runs the system tar in `cwd` and fails the test with its stderr if it fails. */
function tar(cwd, args) {
  const result = spawnSync("tar", args, { cwd, encoding: "utf8" });
  assert.equal(result.status, 0, `tar ${args.join(" ")} failed: ${result.stderr}`);
  return result.stdout;
}

/** The fake binary bytes for a target. */
function fakeBinary(target) {
  return `fake supdoc for ${target}\n`;
}

/**
 * Writes release assets for `version` into a new directory: one archive per target in `targets` and
 * a SHA256SUMS over them. Returns the assets directory and a LICENSE file beside it.
 */
function releaseAssets({ version = VERSION, targets = TARGETS } = {}) {
  const root = scratch("assets-");
  const staging = path.join(root, "staging");
  const assets = path.join(root, "assets");
  mkdirSync(assets, { recursive: true });
  const sums = [];
  for (const target of targets) {
    const name = `supdoc-${version}-${target}`;
    const dir = path.join(staging, name);
    mkdirSync(dir, { recursive: true });
    const binary = path.join(dir, target.includes("windows") ? "supdoc.exe" : "supdoc");
    writeFileSync(binary, fakeBinary(target));
    // Leave the execute bit off so the tests prove the build sets it.
    chmodSync(binary, 0o644);
    writeFileSync(path.join(dir, "LICENSE"), "archive license\n");
    writeFileSync(path.join(dir, "README.md"), "archive readme\n");
    tar(staging, ["-czf", path.join(assets, `${name}.tar.gz`), name]);
    const digest = createHash("sha256").update(readFileSync(path.join(assets, `${name}.tar.gz`))).digest("hex");
    sums.push(`${digest}  ${name}.tar.gz\n`);
  }
  writeFileSync(path.join(assets, "SHA256SUMS"), sums.join(""));
  const license = path.join(root, "LICENSE");
  writeFileSync(license, "MIT License for the test\n");
  return { assets, license, root };
}

/**
 * Rebuilds the archive for `target` in `assets` from a staging tree `populate(dir)` fills, where
 * `dir` is the archive's top directory, then fixes up its line in SHA256SUMS.
 */
function replaceArchive({ assets, root }, target, populate) {
  const name = `supdoc-${VERSION}-${target}`;
  const staging = mkdtempSync(path.join(root, "replace-"));
  mkdirSync(path.join(staging, name));
  populate(path.join(staging, name));
  const archive = path.join(assets, `${name}.tar.gz`);
  tar(staging, ["-czf", archive, name]);
  const digest = createHash("sha256").update(readFileSync(archive)).digest("hex");
  const sums = path.join(assets, "SHA256SUMS");
  writeFileSync(
    sums,
    readFileSync(sums, "utf8").replace(new RegExp(`^[0-9a-f]{64}(  ${name}\\.tar\\.gz)$`, "m"), `${digest}$1`),
  );
  return name;
}

/** The permission bits of `file`. */
function modeOf(file) {
  return statSync(file).mode & 0o777;
}

/** Runs build.mjs as the workflow does. */
function runBuild({ assets, license, out, tag = TAG, extra = [], cwd, env }) {
  return spawnSync(
    process.execPath,
    [BUILD, "--tag", tag, "--assets", assets, "--license", license, "--out", out, ...extra],
    { encoding: "utf8", cwd, env },
  );
}

/** Asserts the build refused with exit 1, printed nothing on stdout and wrote no tarball. */
function assertRefused(result, out, pattern) {
  assert.equal(result.status, 1, `expected a refusal, got status ${result.status}, stderr: ${result.stderr}`);
  assert.equal(result.stdout, "", "a refused build should print no tarball names");
  assert.match(result.stderr, pattern);
  if (existsSync(out)) {
    const tarballs = readdirSync(out, { recursive: true }).filter((entry) => String(entry).endsWith(".tgz"));
    assert.deepEqual(tarballs, [], "a refused build should leave no tarball");
  }
}

describe("a build from complete release assets", () => {
  const built = {};

  before(() => {
    mkdirSync(SCRATCH, { recursive: true });
    built.root = mkdtempSync(path.join(SCRATCH, "full-"));
    const { assets, license } = releaseAssets();
    roots.splice(0); // keep these assets for every test in this group; `after` removes them.
    built.assetsRoot = path.dirname(assets);
    built.out = path.join(built.root, "out");
    built.result = runBuild({ assets, license, out: built.out });
    assert.equal(built.result.status, 0, `build failed: ${built.result.stderr}`);
    built.tarballs = built.result.stdout.trim().split("\n");
    built.unpacked = {};
    for (const tarball of built.tarballs) {
      const dir = path.join(built.root, "unpacked", tarball);
      mkdirSync(dir, { recursive: true });
      tar(dir, ["-xzf", path.join(built.out, tarball)]);
      const manifest = JSON.parse(readFileSync(path.join(dir, "package", "package.json"), "utf8"));
      built.unpacked[manifest.name] = { dir: path.join(dir, "package"), manifest, tarball };
    }
  });

  after(() => {
    rmSync(built.root, { recursive: true, force: true });
    rmSync(built.assetsRoot, { recursive: true, force: true });
  });

  test("prints seven tarball names, the six platform packages before the wrapper", () => {
    assert.equal(built.result.stderr, "");
    assert.deepEqual(built.tarballs, [
      `supdoc-darwin-arm64-${VERSION}.tgz`,
      `supdoc-darwin-x64-${VERSION}.tgz`,
      `supdoc-linux-arm64-${VERSION}.tgz`,
      `supdoc-linux-x64-${VERSION}.tgz`,
      `supdoc-win32-arm64-${VERSION}.tgz`,
      `supdoc-win32-x64-${VERSION}.tgz`,
      `supdoc-${VERSION}.tgz`,
    ]);
    for (const tarball of built.tarballs) {
      assert.ok(existsSync(path.join(built.out, tarball)), `${tarball} should be in the output directory`);
    }
  });

  test("gives every package the tag's version and no scripts key", () => {
    assert.equal(Object.keys(built.unpacked).length, 7);
    for (const [name, { manifest }] of Object.entries(built.unpacked)) {
      assert.equal(manifest.version, VERSION, `${name} should carry the tag's version`);
      assert.ok(!("scripts" in manifest), `${name} must not have a scripts key`);
    }
  });

  test("pins all six platform packages exactly in the wrapper", () => {
    const { manifest, dir } = built.unpacked.supdoc;
    assert.deepEqual(
      manifest.optionalDependencies,
      Object.fromEntries(EXPECTED.map(({ name }) => [name, VERSION])),
    );
    assert.deepEqual(manifest.bin, { supdoc: "bin/supdoc.js" });
    assert.equal(
      readFileSync(path.join(dir, "bin", "supdoc.js"), "utf8"),
      readFileSync(new URL("../supdoc/bin/supdoc.js", import.meta.url), "utf8"),
      "the wrapper should ship the shim unchanged",
    );
    assert.equal(readFileSync(path.join(dir, "LICENSE"), "utf8"), "MIT License for the test\n");
  });

  test("gives each platform package its os, cpu and the binary from its own archive", () => {
    for (const expected of EXPECTED) {
      const unpacked = built.unpacked[expected.name];
      assert.ok(unpacked, `${expected.name} should be built`);
      const { manifest, dir } = unpacked;
      assert.deepEqual(manifest.os, [expected.os], `${expected.name} os`);
      assert.deepEqual(manifest.cpu, [expected.cpu], `${expected.name} cpu`);
      assert.equal(manifest.preferUnplugged, true, `${expected.name} preferUnplugged`);
      for (const key of ["bin", "libc", "scripts"]) {
        assert.ok(!(key in manifest), `${expected.name} must not have a ${key} key`);
      }
      assert.equal(
        readFileSync(path.join(dir, "bin", expected.binary), "utf8"),
        fakeBinary(expected.target),
        `${expected.name} should hold the binary from the ${expected.target} archive`,
      );
      assert.ok(existsSync(path.join(dir, "bin", "installed-by-npm")), `${expected.name} should ship the marker`);
      assert.equal(readFileSync(path.join(dir, "LICENSE"), "utf8"), "MIT License for the test\n");
      assert.match(readFileSync(path.join(dir, "README.md"), "utf8"), new RegExp(`^# ${expected.name}\n`));
    }
  });

  test("uses neither gnu archive", () => {
    const binaries = EXPECTED.map(({ name, binary }) =>
      readFileSync(path.join(built.unpacked[name].dir, "bin", binary), "utf8"),
    );
    for (const target of ["x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu"]) {
      assert.ok(!binaries.includes(fakeBinary(target)), `no package should hold the ${target} binary`);
    }
  });

  test("packs each binary with mode 0755", () => {
    for (const expected of EXPECTED) {
      const tarball = path.join(built.out, built.unpacked[expected.name].tarball);
      const listing = tar(built.out, ["-tvzf", tarball]);
      const line = listing.split("\n").find((entry) => entry.endsWith(`package/bin/${expected.binary}`));
      assert.ok(line, `${expected.name}'s tarball should hold package/bin/${expected.binary}:\n${listing}`);
      assert.match(line, /^-rwxr-xr-x /, `${expected.name}'s binary should be mode 0755: ${line}`);
    }
  });
});

test("refuses an archive whose bytes disagree with SHA256SUMS, naming it", () => {
  const { assets, license, root } = releaseAssets();
  const archive = path.join(assets, `supdoc-${VERSION}-x86_64-unknown-linux-musl.tar.gz`);
  writeFileSync(archive, Buffer.concat([readFileSync(archive), Buffer.from("tampered")]));
  const out = path.join(root, "out");

  const result = runBuild({ assets, license, out });

  assertRefused(
    result,
    out,
    new RegExp(`supdoc-${VERSION}-x86_64-unknown-linux-musl\\.tar\\.gz has SHA-256 [0-9a-f]{64}, but SHA256SUMS lists`),
  );
});

test("refuses a missing archive, naming it", () => {
  const { assets, license, root } = releaseAssets({
    targets: TARGETS.filter((target) => target !== "aarch64-pc-windows-msvc"),
  });
  const out = path.join(root, "out");

  const result = runBuild({ assets, license, out });

  assertRefused(result, out, new RegExp(`supdoc-${VERSION}-aarch64-pc-windows-msvc\\.tar\\.gz is missing from `));
});

test("refuses an archive SHA256SUMS does not list, naming it", () => {
  const { assets, license, root } = releaseAssets();
  const sums = path.join(assets, "SHA256SUMS");
  const kept = readFileSync(sums, "utf8")
    .split("\n")
    .filter((line) => !line.includes("aarch64-apple-darwin"));
  writeFileSync(sums, kept.join("\n"));
  const out = path.join(root, "out");

  const result = runBuild({ assets, license, out });

  assertRefused(result, out, new RegExp(`supdoc-${VERSION}-aarch64-apple-darwin\\.tar\\.gz is not listed in SHA256SUMS`));
});

test("refuses assets without SHA256SUMS", () => {
  const { assets, license, root } = releaseAssets();
  rmSync(path.join(assets, "SHA256SUMS"));
  const out = path.join(root, "out");

  const result = runBuild({ assets, license, out });

  assertRefused(result, out, /SHA256SUMS does not exist/);
});

test("refuses a SHA256SUMS line that is not a checksum and a file name", () => {
  const { assets, license, root } = releaseAssets();
  writeFileSync(path.join(assets, "SHA256SUMS"), `${readFileSync(path.join(assets, "SHA256SUMS"), "utf8")}not a checksum\n`);
  const out = path.join(root, "out");

  const result = runBuild({ assets, license, out });

  assertRefused(result, out, /SHA256SUMS line 9 is not "<sha256>  <file>": not a checksum/);
});

test("refuses a license file that does not exist", () => {
  const { assets, root } = releaseAssets();
  const out = path.join(root, "out");

  const result = runBuild({ assets, license: path.join(root, "NO-LICENSE"), out });

  assertRefused(result, out, /license file .*NO-LICENSE does not exist/);
});

test("refuses a command line without a required option", () => {
  const { assets, root } = releaseAssets();
  const out = path.join(root, "out");

  const result = spawnSync(process.execPath, [BUILD, "--tag", TAG, "--assets", assets, "--out", out], {
    encoding: "utf8",
  });

  assertRefused(result, out, /--license is required\. Usage: node npm\/build\.mjs/);
});

test("refuses an archive that names another version", () => {
  const { assets, license, root } = releaseAssets({ version: "9.8.6" });
  const out = path.join(root, "out");

  const result = runBuild({ assets, license, out });

  assertRefused(result, out, new RegExp(`supdoc-9\\.8\\.6-[a-z0-9_-]+\\.tar\\.gz does not name supdoc ${VERSION}`));
});

test("refuses an archive that holds no binary at the release layout's path", () => {
  const fixture = releaseAssets();
  const { assets, license, root } = fixture;
  const name = replaceArchive(fixture, "aarch64-apple-darwin", (dir) => {
    writeFileSync(path.join(dir, "supdoc-cli"), fakeBinary("aarch64-apple-darwin"));
  });
  const out = path.join(root, "out");

  const result = runBuild({ assets, license, out });

  assertRefused(result, out, new RegExp(`could not list ${name}/supdoc in ${name}\\.tar\\.gz: .+`));
});

test("refuses a tag that is not vMAJOR.MINOR.PATCH", () => {
  const { assets, license, root } = releaseAssets();
  const out = path.join(root, "out");

  const result = runBuild({ assets, license, out, tag: VERSION });

  assertRefused(result, out, new RegExp(`tag ${VERSION.replaceAll(".", "\\.")} is not a release tag`));
});

test("refuses an output directory that already holds files", () => {
  const { assets, license, root } = releaseAssets();
  const out = path.join(root, "out");
  mkdirSync(out);
  writeFileSync(path.join(out, "stale.txt"), "left by an earlier run\n");

  const result = runBuild({ assets, license, out });

  assertRefused(result, out, /output directory .* is not empty/);
});

test("refuses an unknown option", () => {
  const { assets, license, root } = releaseAssets();
  const out = path.join(root, "out");

  const result = runBuild({ assets, license, out, extra: ["--publish"] });

  assertRefused(result, out, /Unknown option '--publish'.*Usage: node npm\/build\.mjs/);
});

describe("an archive whose binary member is not a regular file", () => {
  const unix = { skip: process.platform === "win32" && "symbolic links and FIFOs need a Unix tar" };

  test("refuses a symbolic link member, naming it", unix, () => {
    const fixture = releaseAssets();
    const name = replaceArchive(fixture, "x86_64-apple-darwin", (dir) => symlinkSync("LICENSE", path.join(dir, "supdoc")));
    const out = path.join(fixture.root, "out");

    const result = runBuild({ ...fixture, out });

    assertRefused(result, out, new RegExp(`${name}/supdoc in ${name}\\.tar\\.gz is a symbolic link, not a regular file`));
  });

  test("refuses a directory member, naming it", () => {
    const fixture = releaseAssets();
    const name = replaceArchive(fixture, "x86_64-unknown-linux-musl", (dir) => {
      mkdirSync(path.join(dir, "supdoc"));
      writeFileSync(path.join(dir, "supdoc", "inner"), "not a binary\n");
    });
    const out = path.join(fixture.root, "out");

    const result = runBuild({ ...fixture, out });

    assertRefused(result, out, new RegExp(`${name}/supdoc in ${name}\\.tar\\.gz is a directory, not a regular file`));
  });

  test("refuses a FIFO member, naming it", unix, () => {
    const fixture = releaseAssets();
    const name = replaceArchive(fixture, "aarch64-unknown-linux-musl", (dir) => {
      const made = spawnSync("mkfifo", [path.join(dir, "supdoc")], { encoding: "utf8" });
      assert.equal(made.status, 0, `mkfifo failed: ${made.stderr}`);
    });
    const out = path.join(fixture.root, "out");

    const result = runBuild({ ...fixture, out });

    assertRefused(result, out, new RegExp(`${name}/supdoc in ${name}\\.tar\\.gz is a FIFO, not a regular file`));
  });

  test("refuses a symbolic link to a file outside --out and leaves that file's mode alone", unix, () => {
    const fixture = releaseAssets();
    const outside = path.join(fixture.root, "outside");
    writeFileSync(outside, "a file the build must not touch\n");
    chmodSync(outside, 0o644);
    const name = replaceArchive(fixture, "aarch64-apple-darwin", (dir) => symlinkSync(outside, path.join(dir, "supdoc")));
    const out = path.join(fixture.root, "out");

    const result = runBuild({ ...fixture, out });

    assertRefused(result, out, new RegExp(`${name}/supdoc in ${name}\\.tar\\.gz is a symbolic link`));
    assert.equal(modeOf(outside), 0o644, "the file the link points at should keep mode 0644");
  });
});

describe("makeExecutable", () => {
  test("refuses a symbolic link and leaves the file it points at alone", { skip: process.platform === "win32" }, () => {
    const root = scratch("make-executable-");
    const outside = path.join(root, "outside");
    writeFileSync(outside, "a file the build must not touch\n");
    chmodSync(outside, 0o644);
    const link = path.join(root, "supdoc");
    symlinkSync(outside, link);

    assert.throws(
      () => makeExecutable(link, "the extracted supdoc"),
      (error) => error instanceof BuildError && error.message === "the extracted supdoc is not a regular file; refusing to package it.",
    );
    assert.equal(modeOf(outside), 0o644, "the file the link points at should keep mode 0644");
  });

  test("refuses a directory", () => {
    const root = scratch("make-executable-");
    mkdirSync(path.join(root, "supdoc"));

    assert.throws(
      () => makeExecutable(path.join(root, "supdoc"), "the extracted supdoc"),
      (error) => error instanceof BuildError && error.message === "the extracted supdoc is not a regular file; refusing to package it.",
    );
  });

  test("sets mode 0755 on a regular file", { skip: process.platform === "win32" }, () => {
    const root = scratch("make-executable-");
    const file = path.join(root, "supdoc");
    writeFileSync(file, "binary\n");
    chmodSync(file, 0o644);

    makeExecutable(file, "the extracted supdoc");

    assert.equal(modeOf(file), 0o755);
  });
});

test("extracts from a private copy of the verified bytes, never the original archive", { skip: process.platform === "win32" }, () => {
  const fixture = releaseAssets();
  // A second set of archives whose binaries are evil, with the same names.
  const evil = path.join(fixture.root, "evil");
  mkdirSync(evil);
  for (const { target, binary } of EXPECTED) {
    const name = `supdoc-${VERSION}-${target}`;
    const staging = mkdtempSync(path.join(fixture.root, "evil-staging-"));
    mkdirSync(path.join(staging, name));
    writeFileSync(path.join(staging, name, binary), "evil\n");
    tar(staging, ["-czf", path.join(evil, `${name}.tar.gz`), name]);
  }
  // A tar on PATH that swaps the evil archives over the originals, after hashing, then runs tar.
  const realTar = spawnSync("sh", ["-c", "command -v tar"], { encoding: "utf8" }).stdout.trim();
  const bin = path.join(fixture.root, "bin");
  mkdirSync(bin);
  const log = path.join(fixture.root, "tar-cwd.log");
  writeFileSync(
    path.join(bin, "tar"),
    `#!/bin/sh\npwd >> '${log}'\ncp '${evil}'/*.tar.gz '${fixture.assets}'/\nexec '${realTar}' "$@"\n`,
  );
  chmodSync(path.join(bin, "tar"), 0o755);
  const out = path.join(fixture.root, "out");

  const result = runBuild({ ...fixture, out, env: { ...process.env, PATH: `${bin}${path.delimiter}${process.env.PATH}` } });

  assert.equal(result.status, 0, `build failed: ${result.stderr}`);
  const cwds = new Set(readFileSync(log, "utf8").trim().split("\n"));
  assert.deepEqual([...cwds], [path.join(out, "archives")], "tar should run only on the private copies");
  assert.equal(modeOf(path.join(out, "archives")), 0o700, "the private copies should be in a 0700 directory");
  for (const { name, target, binary } of EXPECTED) {
    const dir = path.join(out, "packages", name.replace("/", "-").replace("@", ""));
    assert.equal(readFileSync(path.join(dir, "bin", binary), "utf8"), fakeBinary(target), `${name} should hold the verified binary`);
  }
});

describe("path options", () => {
  test("accepts a relative --out, resolved against the working directory", () => {
    const fixture = releaseAssets();

    const result = runBuild({ ...fixture, out: "out", cwd: fixture.root });

    assert.equal(result.status, 0, `build failed: ${result.stderr}`);
    const tarballs = result.stdout.trim().split("\n");
    assert.equal(tarballs.length, 7);
    for (const tarball of tarballs) {
      assert.ok(existsSync(path.join(fixture.root, "out", tarball)), `${tarball} should be in <cwd>/out`);
    }
  });

  test("refuses an empty --out", () => {
    const fixture = releaseAssets();

    const result = runBuild({ ...fixture, out: "", cwd: fixture.root });

    assertRefused(result, path.join(fixture.root, "out"), /^npm\/build\.mjs: --out must not be empty\. Usage: /);
  });

  test("refuses an --out that names a file", () => {
    const fixture = releaseAssets();
    const out = path.join(fixture.root, "out");
    writeFileSync(out, "not a directory\n");

    const result = runBuild({ ...fixture, out });

    assertRefused(result, fixture.root, /^npm\/build\.mjs: output directory .*out exists and is not a directory\.\n$/);
  });

  test("refuses a --license that names a directory", () => {
    const fixture = releaseAssets();
    const license = path.join(fixture.root, "license-dir");
    mkdirSync(license);
    const out = path.join(fixture.root, "out");

    const result = runBuild({ ...fixture, license, out });

    assertRefused(result, out, /^npm\/build\.mjs: license file .*license-dir is not a regular file\.\n$/);
  });
});

describe("checkManifests", () => {
  /** Manifests the build would write for VERSION. */
  function manifests() {
    return [
      ...PLATFORMS.map((platform) => ({ name: platform.name, version: VERSION })),
      {
        name: "supdoc",
        version: VERSION,
        optionalDependencies: Object.fromEntries(PLATFORMS.map((platform) => [platform.name, VERSION])),
      },
    ];
  }

  test("accepts the manifests the build writes", () => {
    assert.doesNotThrow(() => checkManifests(manifests(), VERSION));
  });

  test("refuses a scripts key on any package", () => {
    const withScripts = manifests();
    withScripts[2].scripts = { postinstall: "node install.js" };

    assert.throws(() => checkManifests(withScripts, VERSION), (error) => {
      assert.ok(error instanceof BuildError);
      assert.equal(error.message, "@supdoc/linux-arm64 has a scripts key; no supdoc package may run an install script.");
      return true;
    });
  });

  test("refuses a package with another version", () => {
    const stale = manifests();
    stale[0].version = "9.8.6";

    assert.throws(() => checkManifests(stale, VERSION), /@supdoc\/darwin-arm64 has version 9\.8\.6, but the tag says 9\.8\.7/);
  });

  test("refuses a wrapper pin that is not exactly the version", () => {
    const ranged = manifests();
    ranged[6].optionalDependencies["@supdoc/win32-x64"] = `^${VERSION}`;

    assert.throws(() => checkManifests(ranged, VERSION), /supdoc pins @supdoc\/win32-x64 to \^9\.8\.7, not exactly 9\.8\.7/);
  });

  test("refuses a wrapper that does not list all six platform packages", () => {
    const missing = manifests();
    delete missing[6].optionalDependencies["@supdoc/linux-x64"];

    assert.throws(() => checkManifests(missing, VERSION), /supdoc lists optional dependencies .*, not /);
  });
});
