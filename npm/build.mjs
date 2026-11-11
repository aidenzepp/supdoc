#!/usr/bin/env node
// Packs the seven npm packages for one supdoc release from that release's published assets.
//
//   node npm/build.mjs --tag vV --assets <dir> --license <file> --out <dir>
//
// <dir> holds the release archives and SHA256SUMS as `gh release download` wrote them. The script
// re-checks every archive it uses against SHA256SUMS, takes each binary out of its archive with the
// system `tar`, writes six platform packages and the `supdoc` wrapper under <out>/packages, packs
// each into <out>, and prints the seven tarball names. Nothing is compiled, and no generated package
// has a `scripts` key. Any disagreement stops the build before a tarball exists.

import { createHash } from "node:crypto";
import { spawnSync } from "node:child_process";
import {
  chmodSync,
  copyFileSync,
  existsSync,
  mkdirSync,
  readdirSync,
  readFileSync,
  writeFileSync,
} from "node:fs";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { parseArgs } from "node:util";

/**
 * The release targets that become npm packages. The package name is `@supdoc/<os>-<cpu>`, the
 * `process.platform`-`process.arch` pair the wrapper's shim looks up, and Linux ships the
 * statically linked musl build so one package serves glibc and musl distributions.
 */
export const PLATFORMS = [
  { target: "aarch64-apple-darwin", os: "darwin", cpu: "arm64", label: "macOS on Apple silicon" },
  { target: "x86_64-apple-darwin", os: "darwin", cpu: "x64", label: "macOS on Intel" },
  { target: "aarch64-unknown-linux-musl", os: "linux", cpu: "arm64", label: "Linux on arm64" },
  { target: "x86_64-unknown-linux-musl", os: "linux", cpu: "x64", label: "Linux on x64" },
  { target: "aarch64-pc-windows-msvc", os: "win32", cpu: "arm64", label: "Windows on arm64" },
  { target: "x86_64-pc-windows-msvc", os: "win32", cpu: "x64", label: "Windows on x64" },
].map((platform) => ({
  ...platform,
  name: `@supdoc/${platform.os}-${platform.cpu}`,
  binary: platform.os === "win32" ? "supdoc.exe" : "supdoc",
}));

/** Release targets that are published as archives but get no npm package. */
const UNPACKAGED_TARGETS = ["x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu"];

/** The file every platform package ships beside its binary, so `supdoc upgrade` can refuse it. */
const MARKER = "installed-by-npm";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const WRAPPER_DIR = path.join(HERE, "supdoc");
const PLATFORM_README = path.join(HERE, "platform", "README.md");

/** A refusal: the message names what disagreed, and the build exits 1 without packing. */
export class BuildError extends Error {}

/** Parses the command line into `{ tag, assets, license, out }`, refusing anything else. */
export function parseCommandLine(argv) {
  let values;
  try {
    ({ values } = parseArgs({
      args: argv,
      options: {
        tag: { type: "string" },
        assets: { type: "string" },
        license: { type: "string" },
        out: { type: "string" },
      },
      strict: true,
      allowPositionals: false,
    }));
  } catch (error) {
    throw new BuildError(`${error.message}. Usage: ${USAGE}`);
  }
  for (const option of ["tag", "assets", "license", "out"]) {
    if (values[option] === undefined) throw new BuildError(`--${option} is required. Usage: ${USAGE}`);
  }
  return values;
}

const USAGE = "node npm/build.mjs --tag vV --assets <dir> --license <file> --out <dir>";

/** Returns the npm version for a release tag: the tag without its leading `v`. */
export function versionFromTag(tag) {
  const match = /^v(\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?)$/.exec(tag);
  if (!match) throw new BuildError(`tag ${tag} is not a release tag of the form v1.2.3 or v1.2.3-rc.1.`);
  return match[1];
}

/** Reads SHA256SUMS in `assets` into a map from archive name to lowercase hex digest. */
function readChecksums(assets) {
  const file = path.join(assets, "SHA256SUMS");
  if (!existsSync(file)) throw new BuildError(`${file} does not exist; download it with the release archives.`);
  const sums = new Map();
  readFileSync(file, "utf8")
    .split("\n")
    .forEach((line, index) => {
      if (line.trim() === "") return;
      const match = /^([0-9a-f]{64}) [ *](\S+)$/.exec(line);
      if (!match) throw new BuildError(`SHA256SUMS line ${index + 1} is not "<sha256>  <file>": ${line}`);
      sums.set(match[2], match[1]);
    });
  return sums;
}

/** Refuses any release archive in `assets` whose name carries another version or an unknown target. */
function checkArchiveNames(assets, version) {
  const expected = new Set(
    [...PLATFORMS.map((platform) => platform.target), ...UNPACKAGED_TARGETS].map((target) =>
      archiveName(version, target),
    ),
  );
  for (const entry of readdirSync(assets)) {
    if (entry.startsWith("supdoc-") && entry.endsWith(".tar.gz") && !expected.has(entry)) {
      throw new BuildError(`${entry} does not name supdoc ${version} and a release target; the assets disagree with the tag.`);
    }
  }
}

/** The release archive name for one version and target, as release.yml writes it. */
function archiveName(version, target) {
  return `supdoc-${version}-${target}.tar.gz`;
}

/** Refuses an archive that is missing, absent from SHA256SUMS, or whose bytes do not match it. */
function verifyArchive(assets, name, sums) {
  const file = path.join(assets, name);
  if (!existsSync(file)) throw new BuildError(`${name} is missing from ${assets}.`);
  const expected = sums.get(name);
  if (expected === undefined) throw new BuildError(`${name} is not listed in SHA256SUMS.`);
  const actual = createHash("sha256").update(readFileSync(file)).digest("hex");
  if (actual !== expected) {
    throw new BuildError(`${name} has SHA-256 ${actual}, but SHA256SUMS lists ${expected}.`);
  }
}

/** Extracts `<archive dir>/<binary>` from the archive into `binDir` and makes it executable. */
function extractBinary(assets, name, member, binDir) {
  mkdirSync(binDir, { recursive: true });
  // Run in the assets directory with a relative archive name: GNU tar reads `C:\...` as a remote host.
  const result = spawnSync("tar", ["-xzf", name, "-C", binDir, "--strip-components=1", member], {
    cwd: assets,
    encoding: "utf8",
  });
  const binary = path.join(binDir, path.basename(member));
  if (result.error) throw new BuildError(`could not run tar to extract ${name}: ${result.error.message}`);
  if (result.status !== 0 || !existsSync(binary)) {
    throw new BuildError(`could not extract ${member} from ${name}: ${result.stderr.trim() || `tar exited ${result.status}`}`);
  }
  chmodSync(binary, 0o755);
}

/** Writes `value` as two-space JSON with a trailing newline. */
function writeJson(file, value) {
  writeFileSync(file, `${JSON.stringify(value, null, 2)}\n`);
}

/** Writes one platform package into `dir` and returns its manifest. */
function writePlatformPackage({ platform, version, assets, license, wrapper, dir }) {
  const archive = archiveName(version, platform.target);
  extractBinary(assets, archive, `supdoc-${version}-${platform.target}/${platform.binary}`, path.join(dir, "bin"));
  writeFileSync(
    path.join(dir, "bin", MARKER),
    "This supdoc binary was installed by an npm package. Update it with the package manager, not supdoc upgrade.\n",
  );
  const readme = readFileSync(PLATFORM_README, "utf8")
    .replaceAll("{{name}}", platform.name)
    .replaceAll("{{description}}", platform.label)
    .replaceAll("{{archive}}", archive)
    .replaceAll("{{version}}", version);
  writeFileSync(path.join(dir, "README.md"), readme);
  copyFileSync(license, path.join(dir, "LICENSE"));

  const manifest = {
    name: platform.name,
    version,
    description: `The supdoc binary for ${platform.label}`,
    license: wrapper.license,
    homepage: wrapper.homepage,
    repository: wrapper.repository,
    bugs: wrapper.bugs,
    os: [platform.os],
    cpu: [platform.cpu],
    preferUnplugged: true,
    files: [`bin/${platform.binary}`, `bin/${MARKER}`, "README.md", "LICENSE"],
    publishConfig: wrapper.publishConfig,
  };
  writeJson(path.join(dir, "package.json"), manifest);
  return manifest;
}

/** Writes the `supdoc` wrapper into `dir`, pinning every platform package to `version`. */
function writeWrapperPackage({ template, version, license, dir }) {
  mkdirSync(path.join(dir, "bin"), { recursive: true });
  copyFileSync(path.join(WRAPPER_DIR, "bin", "supdoc.js"), path.join(dir, "bin", "supdoc.js"));
  chmodSync(path.join(dir, "bin", "supdoc.js"), 0o755);
  copyFileSync(path.join(WRAPPER_DIR, "README.md"), path.join(dir, "README.md"));
  copyFileSync(license, path.join(dir, "LICENSE"));

  const manifest = {
    ...template,
    version,
    optionalDependencies: Object.fromEntries(PLATFORMS.map((platform) => [platform.name, version])),
  };
  writeJson(path.join(dir, "package.json"), manifest);
  return manifest;
}

/**
 * Refuses generated manifests that would let a package run code at install time (a `scripts` key),
 * carry another version, or pin the platform packages to anything but exactly `version`.
 */
export function checkManifests(manifests, version) {
  const expectedPins = PLATFORMS.map((platform) => platform.name).sort();
  for (const manifest of manifests) {
    if (Object.hasOwn(manifest, "scripts")) {
      throw new BuildError(`${manifest.name} has a scripts key; no supdoc package may run an install script.`);
    }
    if (manifest.version !== version) {
      throw new BuildError(`${manifest.name} has version ${manifest.version}, but the tag says ${version}.`);
    }
    if (manifest.name !== "supdoc") continue;
    const pins = manifest.optionalDependencies ?? {};
    const names = Object.keys(pins).sort();
    if (names.join(" ") !== expectedPins.join(" ")) {
      throw new BuildError(`supdoc lists optional dependencies ${names.join(", ") || "(none)"}, not ${expectedPins.join(", ")}.`);
    }
    for (const [name, pin] of Object.entries(pins)) {
      if (pin !== version) throw new BuildError(`supdoc pins ${name} to ${pin}, not exactly ${version}.`);
    }
  }
}

/** Packs the package in `dir` into `out` with `npm pack` and returns the tarball's file name. */
function pack(dir, out) {
  const windows = process.platform === "win32";
  const args = ["pack", dir, "--pack-destination", out, "--ignore-scripts", "--json"];
  // npm is a .cmd on Windows, which Node starts only through a shell.
  const result = spawnSync("npm", windows ? args.map((arg) => `"${arg}"`) : args, {
    encoding: "utf8",
    shell: windows,
  });
  if (result.error) throw new BuildError(`could not run npm pack for ${dir}: ${result.error.message}`);
  if (result.status !== 0) throw new BuildError(`npm pack ${dir} failed: ${result.stderr.trim()}`);
  const [packed] = JSON.parse(result.stdout);
  return packed.filename;
}

/** Builds and packs all seven packages; returns the tarball file names, platform packages first. */
export function build({ tag, assets, license, out }) {
  const version = versionFromTag(tag);
  if (!existsSync(license)) throw new BuildError(`license file ${license} does not exist.`);
  if (existsSync(out) && readdirSync(out).length > 0) {
    throw new BuildError(`output directory ${out} is not empty; give the build an empty or new directory.`);
  }

  const sums = readChecksums(assets);
  checkArchiveNames(assets, version);
  for (const platform of PLATFORMS) verifyArchive(assets, archiveName(version, platform.target), sums);

  const template = JSON.parse(readFileSync(path.join(WRAPPER_DIR, "package.json"), "utf8"));
  const packages = path.join(out, "packages");
  const built = PLATFORMS.map((platform) => {
    const dir = path.join(packages, platform.name.replace("/", "-").replace("@", ""));
    mkdirSync(dir, { recursive: true });
    return { dir, manifest: writePlatformPackage({ platform, version, assets, license, wrapper: template, dir }) };
  });
  const wrapperDir = path.join(packages, "supdoc");
  built.push({ dir: wrapperDir, manifest: writeWrapperPackage({ template, version, license, dir: wrapperDir }) });

  checkManifests(
    built.map(({ manifest }) => manifest),
    version,
  );
  return built.map(({ dir }) => pack(dir, out));
}

if (process.argv[1] && pathToFileURL(path.resolve(process.argv[1])).href === import.meta.url) {
  try {
    const options = parseCommandLine(process.argv.slice(2));
    for (const tarball of build(options)) process.stdout.write(`${tarball}\n`);
  } catch (error) {
    if (!(error instanceof BuildError)) throw error;
    process.stderr.write(`npm/build.mjs: ${error.message}\n`);
    process.exit(1);
  }
}
