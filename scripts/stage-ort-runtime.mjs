import {
  copyFile,
  mkdir,
  readdir,
  readFile,
  writeFile,
} from "node:fs/promises";
import { basename, join } from "node:path";

const libDir = process.env.MASR_ORT_LIB_DIR;
const target = process.env.MASR_ORT_TARGET;
if (!libDir || !target) {
  throw new Error("MASR_ORT_LIB_DIR and MASR_ORT_TARGET are required");
}

const configPath = "src-tauri/tauri.conf.json";
const config = JSON.parse(await readFile(configPath, "utf8"));
const stagedDir = join(".ci", "ort-runtime", target);
// Tauri resolves bundle file paths relative to src-tauri/tauri.conf.json,
// whereas this script stages CI inputs at the repository root.
const bundleSourceDir = join("..", stagedDir).replaceAll("\\", "/");
await mkdir(stagedDir, { recursive: true });
const resourceDir = "src-tauri";
await mkdir(resourceDir, { recursive: true });

const files = await readdir(libDir);
const runtimeFiles = files.filter((file) =>
  target === "windows-x64"
    ? file === "onnxruntime.dll"
    : target === "linux-x64"
      ? /^libonnxruntime\.so(?:\.\d+)*$/.test(file)
      : /^libonnxruntime(?:\.\d+(?:\.\d+)*)?\.dylib$/.test(file),
);
if (runtimeFiles.length === 0) {
  throw new Error(`No ONNX Runtime library found in ${libDir}`);
}

for (const file of runtimeFiles) {
  await copyFile(join(libDir, file), join(stagedDir, file));
  // Tauri expands resource globs before invoking Cargo build scripts, so this
  // copy must happen before `tauri build`, not solely from build.rs.
  await copyFile(join(libDir, file), join(resourceDir, file));
}

if (target === "windows-x64") {
  // Windows keeps the DLL as a normal bundle resource beside the executable.
} else if (target === "linux-x64") {
  // The checked-in config contains the Windows resource mapping for local
  // Windows builds. Remove it before Tauri validates Linux/macOS resources.
  delete config.bundle.resources?.["onnxruntime.dll"];
  config.bundle.linux.deb.files ??= {};
  config.bundle.linux.appimage.files ??= {};
  for (const file of runtimeFiles) {
    const source = `${bundleSourceDir}/${file}`;
    config.bundle.linux.deb.files[`/usr/lib/${file}`] = source;
    config.bundle.linux.appimage.files[`usr/lib/${file}`] = source;
  }
} else if (target === "macos-arm64") {
  delete config.bundle.resources?.["onnxruntime.dll"];
  const dylib =
    runtimeFiles.find((file) => /\.1\.24\.2\.dylib$/.test(file)) ??
    runtimeFiles[0];
  config.bundle.macOS.frameworks = [`${bundleSourceDir}/${dylib}`];
} else {
  throw new Error(`Unsupported MASR_ORT_TARGET: ${target}`);
}

await writeFile(configPath, `${JSON.stringify(config, null, 2)}\n`);
console.log(
  `Staged ${runtimeFiles.map((file) => basename(file)).join(", ")} for ${target}`,
);
