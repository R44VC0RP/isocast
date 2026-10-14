// Just enough of the zip format to read an Isocast report: the central
// directory, stored and deflated entries, no encryption, no zip64. Entries
// are streamed one at a time from the uploaded Blob with a bound on how much
// each may expand to.

export interface Entry {
  name: string;
  method: number;
  compressed: number;
  size: number;
  offset: number;
}

export const MAX_ENTRIES = 2000;
export const MAX_ENTRY_BYTES = 64 << 20;
export const MAX_TOTAL_BYTES = 160 << 20;

export class ZipError extends Error {}

async function bytes(blob: Blob, start: number, end: number): Promise<DataView> {
  return new DataView(await blob.slice(start, end).arrayBuffer());
}

export async function entries(zip: Blob): Promise<Entry[]> {
  const tailStart = Math.max(0, zip.size - (22 + 0xffff));
  const tail = await bytes(zip, tailStart, zip.size);
  let eocd = -1;
  for (let i = tail.byteLength - 22; i >= 0; i--) {
    if (tail.getUint32(i, true) === 0x06054b50) {
      eocd = i;
      break;
    }
  }
  if (eocd < 0) throw new ZipError("zip: not a zip archive (no end of central directory)");
  const count = tail.getUint16(eocd + 10, true);
  const cdSize = tail.getUint32(eocd + 12, true);
  const cdOffset = tail.getUint32(eocd + 16, true);
  if (count === 0xffff || cdOffset === 0xffffffff) throw new ZipError("zip: zip64 archives are not accepted");
  if (count > MAX_ENTRIES) throw new ZipError(`zip: more than ${MAX_ENTRIES} entries`);
  if (cdOffset + cdSize > zip.size) throw new ZipError("zip: central directory out of range");
  const cd = await bytes(zip, cdOffset, cdOffset + cdSize);
  const decoder = new TextDecoder("utf-8", { fatal: true, ignoreBOM: false });
  const out: Entry[] = [];
  let at = 0;
  let total = 0;
  for (let i = 0; i < count; i++) {
    if (at + 46 > cd.byteLength || cd.getUint32(at, true) !== 0x02014b50) {
      throw new ZipError("zip: malformed central directory");
    }
    const flags = cd.getUint16(at + 8, true);
    const method = cd.getUint16(at + 10, true);
    const compressed = cd.getUint32(at + 20, true);
    const size = cd.getUint32(at + 24, true);
    const nameLength = cd.getUint16(at + 28, true);
    const extraLength = cd.getUint16(at + 30, true);
    const commentLength = cd.getUint16(at + 32, true);
    const offset = cd.getUint32(at + 42, true);
    const name = decoder.decode(new Uint8Array(cd.buffer, cd.byteOffset + at + 46, nameLength));
    if (flags & 1) throw new ZipError(`zip: ${name} is encrypted`);
    if (method !== 0 && method !== 8) throw new ZipError(`zip: ${name} uses an unsupported compression method`);
    if (size > MAX_ENTRY_BYTES) throw new ZipError(`zip: ${name} expands past ${MAX_ENTRY_BYTES >> 20} MiB`);
    total += size;
    if (total > MAX_TOTAL_BYTES) throw new ZipError(`zip: contents expand past ${MAX_TOTAL_BYTES >> 20} MiB`);
    out.push({ name, method, compressed, size, offset });
    at += 46 + nameLength + extraLength + commentLength;
  }
  return out;
}

/** The entry's contents as a stream of UTF-8 text, at most its declared size. */
export async function text(zip: Blob, entry: Entry): Promise<ReadableStream<string>> {
  const local = await bytes(zip, entry.offset, entry.offset + 30);
  if (local.byteLength < 30 || local.getUint32(0, true) !== 0x04034b50) {
    throw new ZipError(`zip: ${entry.name} has no local header`);
  }
  const start = entry.offset + 30 + local.getUint16(26, true) + local.getUint16(28, true);
  if (start + entry.compressed > zip.size) throw new ZipError(`zip: ${entry.name} out of range`);
  let stream = zip.slice(start, start + entry.compressed).stream() as ReadableStream<Uint8Array>;
  if (entry.method === 8) {
    stream = stream.pipeThrough(new DecompressionStream("deflate-raw"));
  }
  let seen = 0;
  const limit = new TransformStream<Uint8Array, Uint8Array>({
    transform(chunk, controller) {
      seen += chunk.byteLength;
      if (seen > entry.size) {
        controller.error(new ZipError(`zip: ${entry.name} expands past its declared size`));
        return;
      }
      controller.enqueue(chunk);
    },
  });
  return stream.pipeThrough(limit).pipeThrough(new TextDecoderStream());
}

/** A whole (small) entry as text. */
export async function readText(zip: Blob, entry: Entry): Promise<string> {
  let out = "";
  const reader = (await text(zip, entry)).getReader();
  for (;;) {
    const { done, value } = await reader.read();
    if (done) return out;
    out += value;
  }
}
