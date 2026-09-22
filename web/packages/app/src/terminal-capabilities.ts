const FILE_UPLOAD_ACTIONS = [
  "upload_start",
  "upload_chunk",
  "upload_finish",
  "upload_cancel",
] as const;

export function supportsFileUpload(capabilities: readonly string[] | undefined): boolean {
  if (!capabilities) return false;
  const advertised = new Set(capabilities);
  return FILE_UPLOAD_ACTIONS.every((action) => advertised.has(action));
}
