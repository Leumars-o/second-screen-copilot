// Writes public/icon-192.png and public/icon-512.png (dark tile, three amber "cue" lines).
// Deterministic; rerun only if the design changes: node scripts/make-icons.mjs
import { writeFileSync } from "node:fs";
import { deflateSync } from "node:zlib";

const CRC = new Int32Array(256).map((_, n) => {
  let c = n;
  for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
  return c;
});
const crc32 = (buf) => {
  let c = -1;
  for (const b of buf) c = CRC[(c ^ b) & 0xff] ^ (c >>> 8);
  return (c ^ -1) >>> 0;
};
const chunk = (type, data) => {
  const len = Buffer.alloc(4);
  len.writeUInt32BE(data.length);
  const td = Buffer.concat([Buffer.from(type), data]);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(td));
  return Buffer.concat([len, td, crc]);
};

function icon(size) {
  const bg = [0x0b, 0x0d, 0x10];
  const amber = [0xff, 0xb0, 0x20];
  const dim = [0xe6, 0xe8, 0xeb];
  const raw = Buffer.alloc((size * 3 + 1) * size);
  const bars = [
    { y: 0.3, x0: 0.22, x1: 0.78, c: amber },
    { y: 0.5, x0: 0.22, x1: 0.66, c: dim },
    { y: 0.7, x0: 0.22, x1: 0.56, c: dim },
  ];
  const h = 0.09; // bar thickness (fraction), rounded ends
  for (let y = 0; y < size; y++) {
    raw[y * (size * 3 + 1)] = 0;
    for (let x = 0; x < size; x++) {
      const fx = (x + 0.5) / size;
      const fy = (y + 0.5) / size;
      let c = bg;
      for (const b of bars) {
        const cx = Math.min(Math.max(fx, b.x0), b.x1);
        if (Math.hypot(fx - cx, fy - b.y) <= h / 2) c = b.c;
      }
      raw.set(c, y * (size * 3 + 1) + 1 + x * 3);
    }
  }
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(size, 0);
  ihdr.writeUInt32BE(size, 4);
  ihdr[8] = 8; // bit depth
  ihdr[9] = 2; // RGB
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk("IHDR", ihdr),
    chunk("IDAT", deflateSync(raw, { level: 9 })),
    chunk("IEND", Buffer.alloc(0)),
  ]);
}

for (const s of [192, 512]) writeFileSync(new URL(`../public/icon-${s}.png`, import.meta.url), icon(s));
console.log("icons written");
