import { copyFileSync, existsSync } from "node:fs";
import { join } from "node:path";

const names = process.platform === "win32"
  ? ["kiss_node.dll"]
  : process.platform === "darwin"
    ? ["libkiss_node.dylib"]
    : ["libkiss_node.so"];
for (const name of names) {
  const source = join("target", "release", name);
  if (existsSync(source)) {
    copyFileSync(source, "kiss.node");
    process.exit(0);
  }
}
throw new Error(`Could not find the native library for ${process.platform}/${process.arch}`);
