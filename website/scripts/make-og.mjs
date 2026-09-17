// Updates the logo tile in public/og.png without regenerating the golden
// circuit artwork or typography. The operation is intentionally idempotent:
// every run replaces the complete tile before compositing the canonical mark.
import sharp from 'sharp';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, '..');
const output = join(root, 'public/og.png');

const metadata = await sharp(output).metadata();
if (metadata.width !== 1200 || metadata.height !== 630) {
  throw new Error(
    `expected public/og.png to be 1200x630, got ${metadata.width}x${metadata.height}`,
  );
}

const tileSize = 76;
const markSize = 56;
// The outer fill matches the artwork beneath the old tile. Keeping this layer
// opaque makes repeated generation pixel-stable instead of re-blending the
// antialiased rounded corners on every run.
const tile = await sharp(
  Buffer.from(`<svg width="${tileSize}" height="${tileSize}" xmlns="http://www.w3.org/2000/svg">
    <rect width="${tileSize}" height="${tileSize}" fill="#11100d"/>
    <rect width="${tileSize}" height="${tileSize}" rx="14" fill="#2c2c2c"/>
  </svg>`),
)
  .png()
  .toBuffer();

const mark = await sharp(join(root, 'public/logo.svg'))
  .trim()
  .resize(markSize, markSize, {
    fit: 'contain',
    kernel: sharp.kernel.nearest,
  })
  .png()
  .toBuffer();

const tileWithMark = await sharp(tile)
  .composite([{ input: mark, gravity: 'centre' }])
  .png()
  .toBuffer();

// Buffer the source first so Sharp can safely replace the same file.
const goldenCard = await sharp(output).png().toBuffer();
await sharp(goldenCard)
  .composite([{ input: tileWithMark, left: 64, top: 60 }])
  .png()
  .toFile(output);

console.log('updated logo in public/og.png (1200x630 golden card)');
