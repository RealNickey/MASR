import { readdir, readFile, writeFile } from "node:fs/promises";
import { join, relative } from "node:path";

const [version, artifactDirectory, outputPath] = process.argv.slice(2);
if (!/^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/.test(version ?? "")) {
  throw new Error("Expected a semantic version as the first argument");
}
if (!artifactDirectory || !outputPath) {
  throw new Error(
    "Usage: generate-latest-json.ts <version> <artifacts-dir> <output>",
  );
}

async function filesIn(directory: string): Promise<string[]> {
  const entries = await readdir(directory, { withFileTypes: true });
  const files = await Promise.all(
    entries.map(async (entry) => {
      const path = join(directory, entry.name);
      return entry.isDirectory() ? filesIn(path) : [path];
    }),
  );
  return files.flat();
}

const files = await filesIn(artifactDirectory);
const releaseTag = `v${version}`;
const assetUrl = (file: string) =>
  `https://github.com/RealNickey/runtime-depot/releases/download/${releaseTag}/${encodeURIComponent(file.split(/[\\/]/).pop()!)}`;

async function platformAsset(target: string, archive: RegExp) {
  const artifact = files.find(
    (file) =>
      relative(artifactDirectory, file).includes(target) && archive.test(file),
  );
  if (!artifact) {
    throw new Error(`Missing updater archive for ${target}`);
  }

  const signature = `${artifact}.sig`;
  if (!files.includes(signature)) {
    throw new Error(
      `Missing updater signature for ${relative(artifactDirectory, artifact)}`,
    );
  }

  return {
    url: assetUrl(artifact),
    signature: (await readFile(signature, "utf8")).trim(),
  };
}

const platforms = {
  "windows-x86_64": await platformAsset(
    "x86_64-pc-windows-msvc",
    /\.nsis\.zip$/i,
  ),
  "darwin-x86_64": await platformAsset(
    "x86_64-apple-darwin",
    /\.app\.tar\.gz$/i,
  ),
  "darwin-aarch64": await platformAsset(
    "aarch64-apple-darwin",
    /\.app\.tar\.gz$/i,
  ),
  "linux-x86_64": await platformAsset(
    "x86_64-unknown-linux-gnu",
    /\.AppImage\.tar\.gz$/i,
  ),
};

await writeFile(
  outputPath,
  `${JSON.stringify(
    {
      version,
      notes: `ThegAi ${version}`,
      pub_date: new Date().toISOString(),
      platforms,
    },
    null,
    2,
  )}\n`,
);
