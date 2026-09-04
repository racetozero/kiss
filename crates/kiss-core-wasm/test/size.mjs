#!/usr/bin/env node
import { strict as assert } from "node:assert";
import { readFile } from "node:fs/promises";
import { gzipSync } from "node:zlib";

const wasm = await readFile(new URL("../pkg/kiss_core_wasm_bg.wasm", import.meta.url));
const loader = await readFile(new URL("../pkg/kiss_core_wasm.js", import.meta.url));
const gzip = gzipSync(wasm, { level: 9 });

assert.ok(wasm.length <= 650_000, `WASM exceeds 650000-byte budget: ${wasm.length}`);
assert.ok(gzip.length <= 230_000, `gzip WASM exceeds 230000-byte budget: ${gzip.length}`);
assert.ok(loader.length <= 40_000, `JS loader exceeds 40000-byte budget: ${loader.length}`);
console.log(`kiss-core-wasm size passed: raw=${wasm.length} gzip=${gzip.length} loader=${loader.length}`);
