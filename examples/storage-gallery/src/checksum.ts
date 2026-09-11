// Incremental Adler-32 and CRC-32 for detecting corruption in the streaming demo.
const crcTable = Uint32Array.from({ length: 256 }, (_, value) => {
  for (let bit = 0; bit < 8; bit++) value = (value >>> 1) ^ ((value & 1) ? 0xedb88320 : 0);
  return value >>> 0;
});

export class Checksum {
  private a = 1;
  private b = 0;
  private crc = 0xffffffff;

  update(bytes: Uint8Array): void {
    // Bound the sums between reductions, independently of input chunk boundaries.
    const blockSize = 5552;
    for (let start = 0; start < bytes.length; start += blockSize) {
      const end = Math.min(start + blockSize, bytes.length);
      for (let i = start; i < end; i++) {
        this.a += bytes[i];
        this.b += this.a;
        this.crc = crcTable[(this.crc ^ bytes[i]) & 255] ^ (this.crc >>> 8);
      }
      this.a %= 65521;
      this.b %= 65521;
    }
  }

  hex(): string {
    const adler = (((this.b << 16) | this.a) >>> 0).toString(16).padStart(8, "0");
    return adler + ((this.crc ^ 0xffffffff) >>> 0).toString(16).padStart(8, "0");
  }
}
