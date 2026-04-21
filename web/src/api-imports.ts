// Multipart upload for .nsys-rep files.
export async function importNsys(file: File): Promise<void> {
  const fd = new FormData();
  fd.append("file", file, file.name);
  const r = await fetch("/api/import/nsys", { method: "POST", body: fd });
  if (!r.ok) throw new Error(`import ${r.status}: ${await r.text()}`);
}
