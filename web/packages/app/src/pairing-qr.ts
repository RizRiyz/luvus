import { renderSVG } from "uqr";

export function pairingQrDataUrl(url: string): string {
  const svg = renderSVG(url, { ecc: "M", border: 3 });
  return `data:image/svg+xml;charset=utf-8,${encodeURIComponent(svg)}`;
}
