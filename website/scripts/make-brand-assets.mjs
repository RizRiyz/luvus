// Generates the raster brand assets from the canonical public SVG mark.
// Keep the small icons optically cropped: rendering the full 1400px canvas at
// favicon sizes makes the pixel mark too small to recognize.
import sharp from 'sharp';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, '..');
const source = join(root, 'public/logo.svg');
const dark = { r: 44, g: 44, b: 44, alpha: 1 };

async function transparent(size, output) {
  await sharp(source)
    .resize(size, size, { fit: 'contain', kernel: sharp.kernel.nearest })
    .png()
    .toFile(join(root, output));
}

async function icon(size, markSize, output) {
  const mark = await sharp(source)
    .trim()
    .resize(markSize, markSize, {
      fit: 'contain',
      kernel: sharp.kernel.nearest,
    })
    .png()
    .toBuffer();

  await sharp({
    create: { width: size, height: size, channels: 4, background: dark },
  })
    .composite([{ input: mark, gravity: 'centre' }])
    .png()
    .toFile(join(root, output));
}

await Promise.all([
  transparent(1400, 'public/logo.png'),
  transparent(1024, 'public/brand/luvus-logo.png'),
  icon(32, 24, 'public/favicon-32.png'),
  icon(192, 144, 'public/favicon.png'),
  icon(180, 136, 'public/apple-touch-icon.png'),
]);

console.log('wrote logo, brand PNG, and favicon assets');
