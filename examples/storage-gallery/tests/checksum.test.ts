import { crc32, deflateSync } from "node:zlib";
import { expect, test } from "vitest";
import { Checksum } from "../src/checksum";

test("streaming checksums agree with zlib independently of read chunk boundaries", () => {
  for (const bytes of [Buffer.alloc(0), Buffer.from("storage gallery"), Buffer.from(Array.from({ length: 1024 * 1024 }, (_, i) => (i * 31 + 7) & 255))]) {
    const compressed = deflateSync(bytes);
    const expected = compressed.readUInt32BE(compressed.length - 4).toString(16).padStart(8, "0")
      + crc32(bytes).toString(16).padStart(8, "0");
    for (const chunkSize of [1, 4099, 65536, bytes.length || 1]) {
      const checksum = new Checksum();
      for (let i = 0; i < bytes.length; i += chunkSize) checksum.update(bytes.subarray(i, i + chunkSize));
      expect(checksum.hex()).toBe(expected);
    }
  }
});
