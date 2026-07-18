import { readFile, writeFile } from "node:fs/promises";

const version = process.argv[2];

if (!/^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/.test(version ?? "")) {
  throw new Error("Expected a semantic version such as 1.2.3");
}

const tauriPath = "src-tauri/tauri.conf.json";
const tauri = JSON.parse(await readFile(tauriPath, "utf8"));
tauri.version = version;
await writeFile(tauriPath, `${JSON.stringify(tauri, null, 2)}\n`);

for (const path of ["package.json", "src-tauri/Cargo.toml"]) {
  const contents = await readFile(path, "utf8");
  const next = contents
    .replace(/^version = "[^"]+"$/m, `version = "${version}"`)
    .replace(/^(\s*"version": )"[^"]+"/m, `$1"${version}"`);

  if (next === contents) {
    throw new Error(`Could not update the version in ${path}`);
  }

  await writeFile(path, next);
}
