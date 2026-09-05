#!/usr/bin/env node
import { strict as assert } from "node:assert";
import { readFile } from "node:fs/promises";
import { resolve } from "node:path";

const wasmPath = process.argv[2]
  ? resolve(process.argv[2])
  : new URL("../pkg/kiss_core_wasm_bg.wasm", import.meta.url);
const bytes = await readFile(wasmPath);
let offset = 8;

function readUleb() {
  let value = 0;
  let shift = 0;
  for (;;) {
    assert.ok(offset < bytes.length, "truncated Wasm LEB128 value");
    const byte = bytes[offset++];
    value += (byte & 0x7f) * 2 ** shift;
    if ((byte & 0x80) === 0) return value;
    shift += 7;
    assert.ok(shift < 35, "oversized Wasm LEB128 value");
  }
}

assert.equal(bytes.subarray(0, 4).toString("hex"), "0061736d", "invalid Wasm magic");
let initialPages = null;
while (offset < bytes.length) {
  const sectionId = bytes[offset++];
  const sectionSize = readUleb();
  const sectionEnd = offset + sectionSize;
  assert.ok(sectionEnd <= bytes.length, "truncated Wasm section");
  if (sectionId === 5) {
    assert.equal(readUleb(), 1, "kiss-core-wasm must define exactly one linear memory");
    const flags = readUleb();
    initialPages = readUleb();
    if ((flags & 1) !== 0) readUleb();
    assert.equal(offset, sectionEnd, "unexpected Wasm memory section contents");
    break;
  }
  offset = sectionEnd;
}

assert.notEqual(initialPages, null, "kiss-core-wasm did not define linear memory");
assert.ok(initialPages <= 32, `initial memory exceeds 32 pages: ${initialPages}`);
console.log(`kiss-core-wasm memory passed: ${initialPages} initial pages`);
